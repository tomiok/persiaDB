//! Turns a [`SegmentBuilder`] into the bytes of one segment (SPEC §5.1, §5.3, ROADMAP 5.2).

pub(super) mod columns;
mod store;
mod terms;

use persia_format::Writer;
use persia_format::block::{BlockWriter, Compression, DEFAULT_BLOCK_SIZE};
use persia_format::checksum::{Crc32c, crc32c};
use persia_format::compress::DEFAULT_ZSTD_LEVEL;

use super::builder::inverted::Inverted;
use super::builder::{FieldBuilder, SegmentBuilder};
use super::kind::SectionKind;
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
    /// Every section is written straight into the returned buffer as it is encoded, never held whole on its own
    /// (ROADMAP 5.1.6). Besides the builder ([`SegmentBuilder::size_in_bytes`]), a flush then needs at most: the
    /// segment bytes (a growing buffer: up to twice their length, three times while an allocator that copies on
    /// reallocation holds the old buffer too); for the `text` or `keyword` field with the most postings (`p` of them,
    /// `t` terms, `n` documents) `4p + 12t` for its postings grouped by term, `n` for its norms and `40 / 128` bytes
    /// per posting for its `Skip` entries; `8n` for the sorted ids; and about 4 MiB for a block in progress, a zstd
    /// context and an FST builder's node registry. The memory threshold of a flush (SPEC §8.2) can count on that bound
    /// (`tests/flush_memory.rs` checks it). The lists are encoded twice (once to learn where each starts), which
    /// makes a flush about 20 % slower than holding them would.
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
                    let grouped = inverted.by_term()?;
                    terms::write_terms(&mut out, id, inverted, &grouped, Some(&norms), options)?;
                    out.framed(SectionKind::Norms, id, Compression::None, options, |s| {
                        s.put(&norms)
                    })?;
                }
                FieldBuilder::Keyword { inverted } => {
                    stats.push(field_stats(id, inverted));
                    if inverted.postings.is_empty() {
                        continue;
                    }
                    let grouped = inverted.by_term()?;
                    terms::write_terms(&mut out, id, inverted, &grouped, None, options)?;
                    out.framed(SectionKind::Columns, id, Compression::None, options, |s| {
                        columns::keyword(inverted, &grouped, doc_count, s)
                    })?;
                }
                FieldBuilder::Column(column) => {
                    if column.values.is_empty() {
                        continue;
                    }
                    out.framed(SectionKind::Columns, id, Compression::None, options, |s| {
                        if field.field_type() == FieldType::Bool {
                            columns::boolean(column, doc_count, s)
                        } else {
                            columns::numeric(column, doc_count, s)
                        }
                    })?;
                }
                FieldBuilder::Stored => {}
            }
        }
        store::doc_store(b, &mut out, options)?;
        let ids = b.ids.sorted_ids();
        store::id_map(b, &ids, &mut out)?;
        store::id_list(b, &mut out, options)?;
        store::blob_refs(b, &mut out, options)?;
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

/// Where the bytes of a section's stream go (ROADMAP 5.1.6): into the segment, as they are encoded.
pub(super) trait Sink {
    /// Appends `bytes` to the stream.
    fn put(&mut self, bytes: &[u8]) -> Result<(), SegmentError>;
}

impl Sink for Writer {
    fn put(&mut self, bytes: &[u8]) -> Result<(), SegmentError> {
        self.write_bytes(bytes);
        Ok(())
    }
}

/// A block-framed section being written into the segment: each block moves to the segment when it fills.
#[derive(Debug)]
pub(super) struct Framed<'o> {
    blocks: BlockWriter,
    /// Uncompressed bytes per block: `put` feeds the writer this much at a time, so at most one encoded block waits.
    block_size: usize,
    out: &'o mut Writer,
}

impl Framed<'_> {
    /// The stream position of the next byte.
    pub(super) fn position(&self) -> u64 {
        self.blocks.position()
    }

    fn drain(&mut self) {
        let done = self.blocks.take_output();
        self.out.write_bytes(&done);
    }
}

impl Sink for Framed<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), SegmentError> {
        for chunk in bytes.chunks(self.block_size.max(1)) {
            self.blocks.write(chunk)?;
            self.drain();
        }
        Ok(())
    }
}

/// A raw section's payload being written into the segment, its CRC32C computed as it goes (SPEC §5.3 "Framing").
#[derive(Debug)]
pub(super) struct Raw<'o> {
    out: &'o mut Writer,
    crc: Crc32c,
}

impl std::io::Write for Raw<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.out.write_bytes(buf);
        self.crc.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The segment being assembled: sections in table order, each 8-aligned.
#[derive(Debug, Default)]
pub(super) struct Sections {
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

    /// Writes section `(kind, field)`: `write` puts its bytes after the previous section's.
    fn section<T>(
        &mut self,
        kind: SectionKind,
        field: u32,
        write: impl FnOnce(&mut Writer) -> Result<T, SegmentError>,
    ) -> Result<T, SegmentError> {
        let too_large = || SegmentError::TooLarge { what: "segment" };
        self.align();
        let start = self.bytes.len();
        let t = write(&mut self.bytes)?;
        let len = self.bytes.len().saturating_sub(start);
        self.table.push(Entry {
            kind,
            field,
            offset: u64::try_from(start).map_err(|_| too_large())?,
            len: u64::try_from(len).map_err(|_| too_large())?,
        });
        Ok(t)
    }

    /// Writes the block-framed section `(kind, field)`, whose stream `write` puts.
    pub(super) fn framed<T>(
        &mut self,
        kind: SectionKind,
        field: u32,
        compression: Compression,
        options: WriteOptions,
        write: impl FnOnce(&mut Framed<'_>) -> Result<T, SegmentError>,
    ) -> Result<T, SegmentError> {
        let blocks = BlockWriter::new(options.block_size, compression)?;
        self.section(kind, field, |out| {
            let block_size = usize::try_from(options.block_size).unwrap_or(usize::MAX);
            let mut framed = Framed {
                blocks,
                block_size,
                out,
            };
            let t = write(&mut framed)?;
            let Framed { blocks, out, .. } = framed;
            out.write_bytes(&blocks.finish()?);
            Ok(t)
        })
    }

    /// Writes the raw section `(kind, field)`: the FST `build` fills, then its CRC (SPEC §5.3.1, §5.3.6).
    pub(super) fn fst(
        &mut self,
        kind: SectionKind,
        field: u32,
        build: impl FnOnce(&mut fst::MapBuilder<&mut Raw<'_>>) -> Result<(), SegmentError>,
    ) -> Result<(), SegmentError> {
        // Writing into a `Vec` cannot fail, so `fst` fails only on keys out of order, which `build` reports.
        let internal = |_| SegmentError::Internal { what: "an FST" };
        self.section(kind, field, |out| {
            let mut raw = Raw {
                out,
                crc: Crc32c::new(),
            };
            let mut map = fst::MapBuilder::new(&mut raw).map_err(internal)?;
            build(&mut map)?;
            map.finish().map_err(internal)?;
            let crc = raw.crc.finish();
            raw.out.write_u32(crc);
            Ok(())
        })
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
