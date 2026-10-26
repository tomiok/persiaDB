//! Turns a [`SegmentBuilder`] into the bytes of one segment (SPEC §5.1, §5.3, ROADMAP 5.2).

mod columns;
mod store;
mod terms;

use persia_format::Writer;
use persia_format::block::{BlockWriter, Compression, DEFAULT_BLOCK_SIZE};
use persia_format::checksum::crc32c;
use persia_format::compress::DEFAULT_ZSTD_LEVEL;

use super::builder::inverted::{Grouped, Inverted};
use super::builder::{FieldBuilder, SegmentBuilder};
use super::kind::{SEGMENT_FIELD, SectionKind};
use super::{SegmentError, norms};
use crate::schema::{FieldType, SchemaHash};

/// The last 4 bytes of every segment (SPEC §5.1).
pub const FOOTER_MAGIC: [u8; 4] = *b"PSEG";
/// Segment format major version this build writes and reads (SPEC §5.1, §15).
pub const FORMAT_MAJOR: u16 = 1;
/// Segment format minor version this build writes (SPEC §5.1, §15).
pub const FORMAT_MINOR: u16 = 0;
/// Largest footer, CRC included (SPEC §5.1): bounds what a reader fetches before it can check anything.
pub const MAX_FOOTER_LEN: u32 = 1 << 20;

/// Sections and the footer start at multiples of this, relative to the segment start (SPEC §4.1, §5.1).
const ALIGN: usize = 8;
/// Bytes of the footer before its variable-length parts (SPEC §5.1).
const FOOTER_FIXED_LEN: usize = 88;

/// The facts about a segment that do not come from its documents (SPEC §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentMeta {
    /// The segment's id, unique in its database.
    pub segment_id: u128,
    /// When the segment was written: microseconds since the Unix epoch, UTC. Passed in, not read from a clock, so
    /// output is reproducible.
    pub created_at: i64,
    /// 0 for a segment flushed from new documents; one more than the highest of its inputs for a merge (SPEC §5.5).
    pub merge_generation: u32,
}

/// How a segment's sections are encoded; readers need none of it, every section describes itself (SPEC §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOptions {
    /// Uncompressed bytes per block of a block-framed section, `1..=1 MiB` (SPEC §5.2).
    pub block_size: u32,
    /// zstd level of the compressed sections (`DocStore`, `IdList`, `BlobRefs`).
    pub zstd_level: i32,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            block_size: DEFAULT_BLOCK_SIZE,
            zstd_level: DEFAULT_ZSTD_LEVEL,
        }
    }
}

/// Writes one segment from a [`SegmentBuilder`] (ROADMAP 5.2.8): every section of SPEC §5.3 the documents call
/// for, then the footer. Same builder contents, meta and options give the same bytes.
#[derive(Debug)]
pub struct SegmentWriter<'a> {
    builder: &'a SegmentBuilder,
    meta: SegmentMeta,
    options: WriteOptions,
}

/// A section table entry (SPEC §5.1).
#[derive(Debug, Clone, Copy)]
struct Entry {
    kind: SectionKind,
    field: u32,
    offset: u64,
    len: u64,
}

/// Per-field statistics of the footer (SPEC §5.1).
#[derive(Debug, Clone, Copy)]
struct FieldStats {
    field: u32,
    docs_with_field: u32,
    sum_field_len: u64,
}

impl<'a> SegmentWriter<'a> {
    /// A writer of `builder`'s documents with default [`WriteOptions`].
    pub fn new(builder: &'a SegmentBuilder, meta: SegmentMeta) -> Self {
        Self {
            builder,
            meta,
            options: WriteOptions::default(),
        }
    }

    /// Uses `options` instead of the defaults.
    #[must_use]
    pub fn with_options(mut self, options: WriteOptions) -> Self {
        self.options = options;
        self
    }

    /// The segment's bytes.
    ///
    /// # Errors
    /// [`SegmentError::Empty`] for a builder without documents, [`SegmentError::Encode`] for invalid options,
    /// [`SegmentError::TooLarge`] when the footer or a section would exceed what the format addresses.
    pub fn finish(self) -> Result<Vec<u8>, SegmentError> {
        let b = self.builder;
        let doc_count = b.doc_count();
        if doc_count == 0 {
            return Err(SegmentError::Empty);
        }
        let options = self.options;
        let mut out = Sections::default();
        let mut stats = Vec::new();
        for (field, builder) in b.schema.fields().iter().zip(&b.fields) {
            let id = field.id().get();
            match builder {
                FieldBuilder::Text { inverted, .. } => {
                    stats.push(field_stats(id, inverted));
                    if inverted.postings.is_empty() {
                        continue;
                    }
                    let norms = dense_norms(inverted, doc_count);
                    let grouped = inverted.by_term();
                    let t = terms::write_terms(inverted, &grouped, Some(&norms), options)?;
                    out.push_terms(id, &t)?;
                    out.push_framed(SectionKind::Norms, id, &norms, options)?;
                }
                FieldBuilder::Keyword { inverted } => {
                    stats.push(field_stats(id, inverted));
                    if inverted.postings.is_empty() {
                        continue;
                    }
                    let grouped = inverted.by_term();
                    let t = terms::write_terms(inverted, &grouped, None, options)?;
                    out.push_terms(id, &t)?;
                    let column = columns::keyword(inverted, &grouped, doc_count)?;
                    out.push_framed(SectionKind::Columns, id, &column, options)?;
                }
                FieldBuilder::Column(column) => {
                    if column.values.is_empty() {
                        continue;
                    }
                    let bytes = if field.field_type() == FieldType::Bool {
                        columns::boolean(column, doc_count)?
                    } else {
                        columns::numeric(column, doc_count)?
                    };
                    out.push_framed(SectionKind::Columns, id, &bytes, options)?;
                }
                FieldBuilder::Stored => {}
            }
        }
        out.push(
            SectionKind::DocStore,
            SEGMENT_FIELD,
            &store::doc_store(b, options)?,
        )?;
        let ids = b.ids.sorted_ids();
        out.push(SectionKind::IdMap, SEGMENT_FIELD, &store::id_map(b, &ids)?)?;
        out.push(
            SectionKind::IdList,
            SEGMENT_FIELD,
            &store::id_list(b, options)?,
        )?;
        if let Some(blobs) = store::blob_refs(b, options)? {
            out.push(SectionKind::BlobRefs, SEGMENT_FIELD, &blobs)?;
        }
        out.align();
        let min_id = ids.first().map_or(&[][..], |&id| b.ids.key(id));
        let max_id = ids.last().map_or(&[][..], |&id| b.ids.key(id));
        let footer = Footer {
            meta: &self.meta,
            schema_hash: b.schema.hash(),
            field_count: b.schema.fields().len(),
            doc_count,
            stats: &stats,
            sections: &out.table,
            min_id,
            max_id,
        };
        footer.write(&mut out.bytes)?;
        Ok(out.bytes.into_inner())
    }
}

fn field_stats(field: u32, inverted: &Inverted) -> FieldStats {
    FieldStats {
        field,
        docs_with_field: inverted.docs_with_field(),
        sum_field_len: inverted.sum_len,
    }
}

/// The `Norms` array: one byte per document, 0 for a document without the field (SPEC §5.3.4).
fn dense_norms(inverted: &Inverted, doc_count: u32) -> Vec<u8> {
    let mut norms = vec![0_u8; doc_count as usize];
    for &(doc, len) in &inverted.lengths {
        if let Some(n) = norms.get_mut(doc as usize) {
            *n = norms::encode(len);
        }
    }
    norms
}

/// A raw (not block-framed) section: its bytes, then their CRC32C (SPEC §5.3).
fn raw_section(mut bytes: Vec<u8>) -> Vec<u8> {
    let crc = crc32c(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes
}

/// The segment being assembled: sections in table order, each 8-aligned.
#[derive(Debug, Default)]
struct Sections {
    bytes: Writer,
    table: Vec<Entry>,
}

impl Sections {
    fn align(&mut self) {
        let pad = self
            .bytes
            .len()
            .next_multiple_of(ALIGN)
            .saturating_sub(self.bytes.len());
        self.bytes.write_zeros(pad);
    }

    fn push(&mut self, kind: SectionKind, field: u32, section: &[u8]) -> Result<(), SegmentError> {
        let too_large = || SegmentError::TooLarge { what: "segment" };
        self.align();
        self.table.push(Entry {
            kind,
            field,
            offset: u64::try_from(self.bytes.len()).map_err(|_| too_large())?,
            len: u64::try_from(section.len()).map_err(|_| too_large())?,
        });
        self.bytes.write_bytes(section);
        Ok(())
    }

    fn push_framed(
        &mut self,
        kind: SectionKind,
        field: u32,
        stream: &[u8],
        options: WriteOptions,
    ) -> Result<(), SegmentError> {
        let mut w = BlockWriter::new(options.block_size, Compression::None)?;
        w.write(stream)?;
        self.push(kind, field, &w.finish()?)
    }

    fn push_terms(&mut self, field: u32, t: &terms::TermSections) -> Result<(), SegmentError> {
        self.push(SectionKind::TermDict, field, &t.dict)?;
        self.push(SectionKind::TermInfo, field, &t.info)?;
        self.push(SectionKind::Postings, field, &t.postings)?;
        self.push(SectionKind::Freqs, field, &t.freqs)?;
        if let Some(skip) = &t.skip {
            self.push(SectionKind::Skip, field, skip)?;
        }
        if let Some(positions) = &t.positions {
            self.push(SectionKind::Positions, field, positions)?;
        }
        Ok(())
    }
}

/// The footer and the trailing locator (SPEC §5.1).
#[derive(Debug)]
struct Footer<'a> {
    meta: &'a SegmentMeta,
    schema_hash: SchemaHash,
    field_count: usize,
    doc_count: u32,
    stats: &'a [FieldStats],
    sections: &'a [Entry],
    min_id: &'a [u8],
    max_id: &'a [u8],
}

impl Footer<'_> {
    fn write(&self, out: &mut Writer) -> Result<(), SegmentError> {
        let too_large = || SegmentError::TooLarge { what: "footer" };
        let count = |n: usize| u32::try_from(n).map_err(|_| too_large());
        let mut w = Writer::new();
        w.write_u16(FORMAT_MAJOR);
        w.write_u16(FORMAT_MINOR);
        w.write_u32(0);
        w.write_u64(0); // incompat_flags
        w.write_u64(0); // rocompat_flags
        w.write_u128(self.meta.segment_id);
        w.write_u128(self.schema_hash.get());
        w.write_u8(SchemaHash::ENCODING_VERSION);
        w.write_zeros(3);
        w.write_u32(count(self.field_count)?);
        w.write_u32(self.doc_count);
        w.write_u32(self.meta.merge_generation);
        w.write_i64(self.meta.created_at);
        w.write_u32(count(self.stats.len())?);
        w.write_u32(count(self.sections.len())?);
        debug_assert_eq!(w.len(), FOOTER_FIXED_LEN);
        for s in self.stats {
            w.write_u32(s.field);
            w.write_u32(s.docs_with_field);
            w.write_u64(s.sum_field_len);
        }
        for e in self.sections {
            w.write_u16(e.kind.code());
            w.write_u16(0);
            w.write_u32(e.field);
            w.write_u64(e.offset);
            w.write_u64(e.len);
        }
        for id in [self.min_id, self.max_id] {
            w.write_u16(u16::try_from(id.len()).map_err(|_| too_large())?);
            w.write_bytes(id);
        }
        let crc = crc32c(w.as_slice());
        w.write_u32(crc);
        let footer_len = count(w.len())
            .ok()
            .filter(|&n| n <= MAX_FOOTER_LEN)
            .ok_or_else(too_large)?;
        out.write_bytes(w.as_slice());
        out.write_u32(footer_len);
        out.write_bytes(&FOOTER_MAGIC);
        Ok(())
    }
}
