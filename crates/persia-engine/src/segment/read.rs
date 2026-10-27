//! Reads a segment (SPEC §5, ROADMAP 5.3): [`Segment::open`] checks the footer, the section table and every block
//! table, then the accessors decode what they are asked for, checking it as they go. Nothing a segment holds can
//! make them panic: corrupt bytes are a [`ReadError`].

// Offsets and lengths come from disk: arithmetic must be checked or explicitly wrapping (ROADMAP 5.3.9).
#![deny(clippy::arithmetic_side_effects)]

mod columns;
mod error;
mod footer;
mod fst_check;
mod postings;
mod store;
mod stream;
mod terms;
mod verify;

use std::sync::atomic::{AtomicU64, Ordering};

use persia_format::checksum::crc32c;
use persia_format::{BlobRef, ByteSource, Reader};
use roaring::RoaringBitmap;

pub use columns::{BoolColumn, KeywordColumn, Norms, NumericColumn};
pub use error::{ReadError, SegmentCorruption, UnsupportedSegment};
pub use footer::FieldStats;
pub use postings::{BlockMax, Postings, Tombstones};
pub use terms::{TermIndex, TermInfo, TermStream};

use super::kind::{SEGMENT_FIELD, SectionKind};
use crate::document::{Document, MAX_DOCUMENT_LEN, MAX_ID_LEN, MAX_KEYWORD_LEN};
use crate::schema::{CollectionSchema, Field, FieldId, FieldKind, FieldType};
use footer::{Footer, Section};
use store::BlockCache;
use stream::Stream;
use terms::{Lists, TERM_INFO_LEN};

/// One entry of a segment's section table (SPEC §5.1), from [`Segment::sections`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionInfo {
    /// The section's kind.
    pub kind: SectionKind,
    /// Its field id ([`SEGMENT_FIELD`] for a segment-level section).
    pub field: u32,
    /// Where it starts, relative to the segment start.
    pub offset: u64,
    /// Its length on disk, block table or CRC included.
    pub len: u64,
    /// For a block-framed section, its logical stream (SPEC §5.2); `None` for a raw one.
    pub stream: Option<StreamInfo>,
}

/// The logical stream of a block-framed section (SPEC §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    /// Uncompressed length.
    pub len: u64,
    /// Number of framed blocks.
    pub block_count: u32,
    /// Uncompressed bytes per block (the last may hold fewer).
    pub block_size: u32,
}

/// The next [`Segment::open`]'s instance token.
static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(0);

/// An open segment (SPEC §5): immutable, so it can be shared by any number of readers (`Segment` is `Sync`).
///
/// ```
/// use persia_analysis::AnalyzerRegistry;
/// use persia_engine::document::{Document, Value};
/// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
/// use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
/// use persia_format::ByteSource;
///
/// let schema = SchemaBuilder::new("notes")
///     .field("body", FieldKind::Text(TextOptions::default()))
///     .build()?;
/// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
/// let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
/// builder.add(&Document::new("n1").with(body, Value::String("Hello, world".into())))?;
/// let meta = SegmentMeta { segment_id: 7, created_at: 0, merge_generation: 0 };
/// let bytes = SegmentWriter::new(&builder, meta).finish()?;
///
/// let segment = Segment::open(ByteSource::new(bytes.into()), &schema)?;
/// assert_eq!((segment.segment_id(), segment.doc_count()), (7, 1));
/// let info = segment.terms(body)?.get(b"hello")?.ok_or("no term")?;
/// assert_eq!(info.df(), 1);
/// assert_eq!(segment.docid_of("n1")?, Some(0));
/// assert_eq!(segment.document(0)?.get(body), Some(&Value::String("Hello, world".into())));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Segment {
    source: ByteSource,
    schema: CollectionSchema,
    footer: Footer,
    /// `(field id, dictionary)` of each `TermDict`, ascending.
    term_dicts: Vec<(u32, fst::Map<ByteSource>)>,
    /// Always `Some` once opened (SPEC §5.3 "Presence").
    id_map: Option<fst::Map<ByteSource>>,
    cache: BlockCache,
    /// Unique per [`Segment::open`] in this process: tells this segment's [`TermInfo`]s from any other's, even
    /// one with the same segment id.
    instance: u64,
}

impl Segment {
    /// Opens the segment `source` (its whole bytes, at the offset they have in their file or object) of a
    /// collection whose current schema is `schema` (SPEC §5.1).
    ///
    /// Checks the locator, the footer (its CRC first), the schema hash against `schema` at the field count the
    /// segment records (SPEC §6.2.5), the section table, padding, which sections are present (SPEC §5.3), the
    /// block table of every block-framed section, the CRC of the raw ones, and the shape of the FSTs and record
    /// arrays. Blocks are checked when they are decoded.
    ///
    /// # Errors
    /// [`ReadError::Unsupported`] for another format major version, unknown `incompat_flags` or schema encoding
    /// version; a corruption ([`ReadError::is_corruption`]) for anything else that breaks a rule of SPEC §5.1 or
    /// §5.3, including a schema hash other than `schema`'s.
    pub fn open(source: ByteSource, schema: &CollectionSchema) -> Result<Self, ReadError> {
        let footer = footer::parse(source.as_slice(), source.base_offset(), schema)?;
        let mut segment = Self {
            id_map: None,
            source,
            schema: schema.clone(),
            footer,
            term_dicts: Vec::new(),
            cache: BlockCache::default(),
            instance: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
        };
        segment.check_presence()?;
        segment.validate_tables()?;
        segment.check_sections()?;
        Ok(segment)
    }

    /// The segment's id (SPEC §5.1).
    pub fn segment_id(&self) -> u128 {
        self.footer.segment_id
    }

    /// Number of documents; docids are `0..doc_count` (SPEC §5.4).
    pub fn doc_count(&self) -> u32 {
        self.footer.doc_count
    }

    /// 0 for a flushed segment, one more than its highest input for a merged one (SPEC §5.5).
    pub fn merge_generation(&self) -> u32 {
        self.footer.merge_generation
    }

    /// When the segment was written: microseconds since the Unix epoch, UTC.
    pub fn created_at(&self) -> i64 {
        self.footer.created_at
    }

    /// The format minor version that wrote the segment (readers never branch on it, SPEC §5.1).
    pub fn format_minor(&self) -> u16 {
        self.footer.format_minor
    }

    /// Read-only compatible feature flags: a segment with bits this build does not know may be read, but a merge
    /// must not rewrite it (SPEC §5.1).
    pub fn rocompat_flags(&self) -> u64 {
        self.footer.rocompat_flags
    }

    /// The number of schema fields the segment was indexed with (SPEC §6.2.5); later fields have no data in it.
    pub fn schema_field_count(&self) -> u32 {
        self.footer.schema_field_count
    }

    /// The smallest and largest document id in the segment (byte order), for routing and pruning.
    pub fn id_range(&self) -> (&[u8], &[u8]) {
        (&self.footer.min_id, &self.footer.max_id)
    }

    /// The section table (SPEC §5.1), in segment order, with each block-framed section's stream size: for tools
    /// such as `persia inspect segment`.
    ///
    /// ```
    /// # use persia_analysis::AnalyzerRegistry;
    /// # use persia_engine::document::Document;
    /// # use persia_engine::schema::SchemaBuilder;
    /// # use persia_engine::segment::{SEGMENT_FIELD, SectionKind, Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    /// # use persia_format::ByteSource;
    /// # let schema = SchemaBuilder::new("c").build()?;
    /// # let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
    /// # builder.add(&Document::new("d1"))?;
    /// # let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
    /// # let bytes = SegmentWriter::new(&builder, meta).finish()?;
    /// let segment = Segment::open(ByteSource::new(bytes.into()), &schema)?;
    /// let kinds: Vec<_> = segment.sections()?.iter().map(|s| (s.kind, s.field, s.stream.is_some())).collect();
    /// assert_eq!(kinds, [
    ///     (SectionKind::DocStore, SEGMENT_FIELD, true),
    ///     (SectionKind::IdMap, SEGMENT_FIELD, false), // raw: an FST
    ///     (SectionKind::IdList, SEGMENT_FIELD, true),
    /// ]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// A corruption if a block table no longer reads (it was checked at open).
    pub fn sections(&self) -> Result<Vec<SectionInfo>, ReadError> {
        self.footer
            .sections
            .iter()
            .map(|s| {
                let stream = if s.kind.is_block_framed() {
                    self.stream(s.kind, s.field)?.map(|st| StreamInfo {
                        len: st.len(),
                        block_count: st.block_count(),
                        // At most 1 MiB (SPEC §5.2), checked when the table was opened.
                        block_size: u32::try_from(st.block_size()).unwrap_or(u32::MAX),
                    })
                } else {
                    None
                };
                Ok(SectionInfo {
                    kind: s.kind,
                    field: s.field,
                    offset: s.range.start,
                    len: s.range.end.saturating_sub(s.range.start),
                    stream,
                })
            })
            .collect()
    }

    /// The statistics of a `text` or `keyword` field (SPEC §5.1); `None` for another type or a field added after
    /// the segment was written.
    pub fn field_stats(&self, field: FieldId) -> Option<FieldStats> {
        let id = field.get();
        self.footer
            .stats
            .binary_search_by_key(&id, |&(f, _)| f)
            .ok()
            .and_then(|i| self.footer.stats.get(i))
            .map(|&(_, s)| s)
    }

    /// `deleted` as this segment's tombstones (SPEC §8.3), to apply with [`Postings::with_tombstones`].
    ///
    /// # Errors
    /// [`ReadError::Tombstone`] if a docid is at or above [`Self::doc_count`].
    pub fn tombstones(&self, deleted: RoaringBitmap) -> Result<Tombstones, ReadError> {
        Tombstones::new(deleted, self.doc_count())
    }

    /// The term dictionary and posting lists of a `text` or `keyword` field (ROADMAP 5.3.2); empty when no
    /// document of the segment has the field.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`] or [`ReadError::FieldType`] for a field that is not a `text` or `keyword`
    /// field of the schema; a corruption if a block table fails (checked again on each call).
    pub fn terms(&self, field: FieldId) -> Result<TermIndex<'_>, ReadError> {
        let f = self.field(field)?;
        let keyword = match f.field_type() {
            FieldType::Text => false,
            FieldType::Keyword => true,
            other => return Err(type_error(field, other, "terms")),
        };
        let id = field.get();
        let Some(dict) = self
            .term_dicts
            .binary_search_by_key(&id, |(f, _)| *f)
            .ok()
            .and_then(|i| self.term_dicts.get(i))
            .map(|(_, d)| d)
        else {
            return Ok(TermIndex::empty());
        };
        let need = |kind| {
            self.stream(kind, id)?
                .ok_or_else(|| self.presence_error(kind, id, false))
        };
        let lists = Lists {
            postings: need(SectionKind::Postings)?,
            freqs: need(SectionKind::Freqs)?,
            skip: self.stream(SectionKind::Skip, id)?,
            positions: self.stream(SectionKind::Positions, id)?,
        };
        Ok(TermIndex::new(
            dict,
            need(SectionKind::TermInfo)?,
            lists,
            self.doc_count(),
            keyword,
            (self.instance, id),
        ))
    }

    /// The norms of a `text` field (ROADMAP 5.3.5): all 0 when no document has a token in it.
    ///
    /// # Errors
    /// As [`Self::terms`], for a field that is not `text`; a corruption if the section is.
    pub fn norms(&self, field: FieldId) -> Result<Norms, ReadError> {
        let f = self.field(field)?;
        if f.field_type() != FieldType::Text {
            return Err(type_error(field, f.field_type(), "norms"));
        }
        Norms::read(
            self.stream(SectionKind::Norms, field.get())?,
            self.doc_count(),
        )
    }

    /// The doc values of an `i64`, `f64` or `datetime` field (ROADMAP 5.3.5).
    ///
    /// # Errors
    /// As [`Self::norms`].
    pub fn numeric_column(&self, field: FieldId) -> Result<NumericColumn, ReadError> {
        let f = self.field(field)?;
        let ty = f.field_type();
        if !matches!(ty, FieldType::I64 | FieldType::F64 | FieldType::Datetime) {
            return Err(type_error(field, ty, "numeric column"));
        }
        NumericColumn::read(
            self.stream(SectionKind::Columns, field.get())?,
            ty,
            self.doc_count(),
            false,
        )
    }

    /// The doc values of a `bool` field (ROADMAP 5.3.5).
    ///
    /// # Errors
    /// As [`Self::norms`].
    pub fn bool_column(&self, field: FieldId) -> Result<BoolColumn, ReadError> {
        let f = self.field(field)?;
        if f.field_type() != FieldType::Bool {
            return Err(type_error(field, f.field_type(), "bool column"));
        }
        BoolColumn::read(
            self.stream(SectionKind::Columns, field.get())?,
            self.doc_count(),
        )
    }

    /// The doc values of a `keyword` field (ROADMAP 5.3.5).
    ///
    /// # Errors
    /// As [`Self::norms`].
    pub fn keyword_column(&self, field: FieldId) -> Result<KeywordColumn, ReadError> {
        let f = self.field(field)?;
        if f.field_type() != FieldType::Keyword {
            return Err(type_error(field, f.field_type(), "keyword column"));
        }
        KeywordColumn::read(
            self.stream(SectionKind::Columns, field.get())?,
            self.doc_count(),
            false,
        )
    }

    /// Document `docid` as stored: its id, blob reference and stored fields (SPEC §6.4.2, ROADMAP 5.3.6).
    ///
    /// # Errors
    /// [`ReadError::DocOutOfRange`] for a docid at or above [`Self::doc_count`]; [`ReadError::Document`] if its
    /// row does not decode; a corruption if the `DocStore` section is.
    pub fn document(&self, docid: u32) -> Result<Document, ReadError> {
        let row = self.record(SectionKind::DocStore, docid)?;
        Document::decode(&row, &self.schema).map_err(|source| ReadError::Document { docid, source })
    }

    /// The document with id `id`, and its docid, if the segment has it (via the `IdMap`).
    ///
    /// # Errors
    /// As [`Self::document`] and [`Self::docid_of`].
    pub fn document_by_id(&self, id: &str) -> Result<Option<(u32, Document)>, ReadError> {
        match self.docid_of(id)? {
            Some(docid) => Ok(Some((docid, self.document(docid)?))),
            None => Ok(None),
        }
    }

    /// The docid of the document with id `id`, if the segment has it (SPEC §5.3.6).
    ///
    /// # Errors
    /// A corruption if the `IdMap` maps `id` to a docid at or above [`Self::doc_count`].
    pub fn docid_of(&self, id: &str) -> Result<Option<u32>, ReadError> {
        let Some(value) = self.id_map.as_ref().and_then(|m| m.get(id)) else {
            return Ok(None);
        };
        u32::try_from(value)
            .ok()
            .filter(|&d| d < self.doc_count())
            .map(Some)
            .ok_or(ReadError::Section {
                kind: SectionKind::IdMap,
                field: SEGMENT_FIELD,
                position: 0,
                reason: SegmentCorruption::Invalid {
                    what: "an id mapped to a docid at or above doc_count",
                },
            })
    }

    /// The id of document `docid` (from the `IdList`).
    ///
    /// # Errors
    /// As [`Self::document`]; an empty or non-UTF-8 id is a corruption.
    pub fn id_of(&self, docid: u32) -> Result<String, ReadError> {
        let bytes = self.record(SectionKind::IdList, docid)?;
        let bad = || ReadError::Section {
            kind: SectionKind::IdList,
            field: SEGMENT_FIELD,
            position: u64::from(docid).saturating_mul(8),
            reason: SegmentCorruption::Invalid {
                what: "an empty or non-UTF-8 document id",
            },
        };
        if bytes.is_empty() {
            return Err(bad());
        }
        String::from_utf8(bytes).map_err(|_| bad())
    }

    /// The blob reference of document `docid`, if it has one (from `BlobRefs`, SPEC §9.1).
    ///
    /// # Errors
    /// As [`Self::document`]; a record that is not exactly one `BlobRef` encoding is a corruption.
    pub fn blob_ref(&self, docid: u32) -> Result<Option<BlobRef>, ReadError> {
        self.check_docid(docid)?;
        if self.section(SectionKind::BlobRefs, SEGMENT_FIELD).is_none() {
            return Ok(None);
        }
        let bytes = self.record(SectionKind::BlobRefs, docid)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let bad = |reason| ReadError::Section {
            kind: SectionKind::BlobRefs,
            field: SEGMENT_FIELD,
            position: u64::from(docid).saturating_mul(8),
            reason,
        };
        let mut r = Reader::new(&bytes);
        let blob = BlobRef::decode(&mut r).map_err(|e| match e.corruption() {
            Some((_, reason)) => bad(reason.into()),
            None => ReadError::Block {
                kind: SectionKind::BlobRefs,
                field: SEGMENT_FIELD,
                source: e,
            },
        })?;
        if !r.is_empty() {
            return Err(bad(SegmentCorruption::TrailingBytes));
        }
        Ok(Some(blob))
    }

    /// A section SPEC §5.3 "Presence" requires is missing, or one it rules out is there; reported at the segment start.
    fn presence_error(&self, kind: SectionKind, field: u32, present: bool) -> ReadError {
        ReadError::Corrupt {
            offset: self.source.base_offset(),
            reason: SegmentCorruption::Presence {
                kind: kind.code(),
                field,
                present,
            },
        }
    }

    fn check_docid(&self, docid: u32) -> Result<(), ReadError> {
        if docid < self.doc_count() {
            Ok(())
        } else {
            Err(ReadError::DocOutOfRange {
                docid,
                doc_count: self.doc_count(),
            })
        }
    }

    fn record(&self, kind: SectionKind, docid: u32) -> Result<Vec<u8>, ReadError> {
        self.check_docid(docid)?;
        let stream = self
            .stream(kind, SEGMENT_FIELD)?
            .ok_or_else(|| self.presence_error(kind, SEGMENT_FIELD, false))?;
        // The longest record any valid segment holds (SPEC §6.4.1, §9.1).
        let max = match kind {
            SectionKind::IdList => MAX_ID_LEN,
            SectionKind::BlobRefs => BlobRef::MAX_ENCODED_LEN,
            _ => MAX_DOCUMENT_LEN,
        };
        let max = u64::try_from(max).unwrap_or(u64::MAX);
        store::record(&stream, self.doc_count(), docid, max, &self.cache)
    }

    /// A field of the schema.
    fn field(&self, field: FieldId) -> Result<&Field, ReadError> {
        self.schema
            .field_by_id(field)
            .ok_or(ReadError::UnknownField { field })
    }

    fn section(&self, kind: SectionKind, field: u32) -> Option<&Section> {
        let sections = &self.footer.sections;
        sections
            .binary_search_by_key(&(field, kind), |s| (s.field, s.kind))
            .ok()
            .and_then(|i| sections.get(i))
    }

    /// The bytes of `section`, at their absolute offsets.
    fn bytes(&self, section: &Section) -> Result<ByteSource, ReadError> {
        let base = self.source.base_offset();
        self.source
            .slice(base.saturating_add(section.range.start)..base.saturating_add(section.range.end))
            .map_err(ReadError::Format)
    }

    /// The block-framed section `(kind, field)`, if present.
    fn stream(&self, kind: SectionKind, field: u32) -> Result<Option<Stream<'_>>, ReadError> {
        let Some(section) = self.section(kind, field) else {
            return Ok(None);
        };
        let bytes = section_slice(self.source.as_slice(), section);
        let base = self
            .source
            .base_offset()
            .saturating_add(section.range.start);
        match section.table {
            Some(table) => Stream::reopen(kind, field, bytes, base, table).map(Some),
            None => Stream::open(kind, field, bytes, base).map(Some),
        }
    }

    /// Validates every block table once (ROADMAP 5.3.10) and keeps what it found, so every later [`Self::stream`]
    /// costs O(1) instead of a CRC over the whole table.
    fn validate_tables(&mut self) -> Result<(), ReadError> {
        let mut tables = Vec::with_capacity(self.footer.sections.len());
        for section in &self.footer.sections {
            tables.push(if section.kind.is_block_framed() {
                self.stream(section.kind, section.field)?
                    .map(|s| s.validated())
            } else {
                None
            });
        }
        for (section, table) in self.footer.sections.iter_mut().zip(tables) {
            section.table = table;
        }
        Ok(())
    }

    /// SPEC §5.3 "Presence", and the field statistics against it.
    fn check_presence(&self) -> Result<(), ReadError> {
        let count = usize::try_from(self.footer.schema_field_count).unwrap_or(usize::MAX);
        for f in self.schema.fields().get(..count).unwrap_or_default() {
            let id = f.id().get();
            let has = |kind| self.section(kind, id).is_some();
            let any = SectionKind::ALL.into_iter().any(has);
            let required: &[SectionKind] = match f.kind() {
                FieldKind::Text(o) => {
                    if any && has(SectionKind::Positions) != o.positions {
                        return Err(self.presence_error(SectionKind::Positions, id, !o.positions));
                    }
                    &[
                        SectionKind::TermDict,
                        SectionKind::TermInfo,
                        SectionKind::Postings,
                        SectionKind::Freqs,
                        SectionKind::Norms,
                    ]
                }
                FieldKind::Keyword(_) => &[
                    SectionKind::TermDict,
                    SectionKind::TermInfo,
                    SectionKind::Postings,
                    SectionKind::Freqs,
                    SectionKind::Columns,
                ],
                _ => &[],
            };
            if any && let Some(&kind) = required.iter().find(|&&k| !has(k)) {
                return Err(self.presence_error(kind, id, false));
            }
            if let Some(stats) = self.field_stats(f.id())
                && any != (stats.docs_with_field > 0)
            {
                return Err(ReadError::Corrupt {
                    offset: self.source.base_offset(),
                    reason: SegmentCorruption::FieldStats { field: id },
                });
            }
        }
        for kind in [
            SectionKind::DocStore,
            SectionKind::IdMap,
            SectionKind::IdList,
        ] {
            if self.section(kind, SEGMENT_FIELD).is_none() {
                return Err(self.presence_error(kind, SEGMENT_FIELD, false));
            }
        }
        Ok(())
    }

    /// The longest key the FST of `(kind, field)` can hold, which bounds its longest path (SPEC §5.3.1, §5.3.6):
    /// an id's length for the `IdMap`, a keyword value's for a `keyword` field's `TermDict` (its terms are its
    /// values). A `text` field's terms are analyzer tokens, which SPEC does not bound yet (a registered analyzer
    /// need not apply the length guard of §6.3.1), so its paths are not bounded here.
    fn max_key_len(&self, kind: SectionKind, field: u32) -> u64 {
        let keyword = || {
            self.schema
                .fields()
                .iter()
                .find(|f| f.id().get() == field)
                .is_some_and(|f| matches!(f.kind(), FieldKind::Keyword(_)))
        };
        let len = if kind == SectionKind::IdMap {
            MAX_ID_LEN
        } else if keyword() {
            MAX_KEYWORD_LEN
        } else {
            return u64::MAX;
        };
        u64::try_from(len).unwrap_or(u64::MAX)
    }

    /// Every block table and raw CRC; the FSTs and the record arrays' shape.
    fn check_sections(&mut self) -> Result<(), ReadError> {
        let doc_count = self.doc_count();
        for section in &self.footer.sections {
            let (kind, field) = (section.kind, section.field);
            let fst_error = |what| ReadError::Section {
                kind,
                field,
                position: 0,
                reason: what,
            };
            if kind.is_block_framed() {
                let stream = self
                    .stream(kind, field)?
                    .ok_or_else(|| self.presence_error(kind, field, false))?;
                match kind {
                    SectionKind::TermInfo => {
                        let terms = self
                            .term_dicts
                            .last()
                            .filter(|(f, _)| *f == field)
                            .map(|(_, d)| u64::try_from(d.len()).unwrap_or(u64::MAX));
                        if terms.and_then(|n| n.checked_mul(TERM_INFO_LEN)) != Some(stream.len()) {
                            return Err(fst_error(SegmentCorruption::Fst));
                        }
                    }
                    SectionKind::DocStore | SectionKind::IdList | SectionKind::BlobRefs => {
                        store::check_records(&stream, doc_count, &self.cache)?;
                    }
                    _ => {}
                }
                continue;
            }
            let raw = raw_payload(self.source.as_slice(), section)?;
            // `fst` does not validate what it reads: check the structure first (SPEC §5.3 "Framing", `fst_check`),
            // and bound the key length, which bounds what a stream over the keys costs (SPEC §5.3.1, §5.3.6).
            let shape = fst_check::check(raw, self.max_key_len(kind, field)).map_err(|d| {
                ReadError::Section {
                    kind,
                    field,
                    position: u64::try_from(d.at).unwrap_or(u64::MAX),
                    reason: SegmentCorruption::Fst,
                }
            })?;
            // Bound the key count too: a small DAG can hold astronomically many keys. An `IdMap` stream stops by
            // itself after `doc_count + 1` keys (its values are distinct and below `doc_count`); a `TermDict` must
            // have one `TermInfo` record per key, checked now with the error the `TermInfo` check would give.
            if kind != SectionKind::IdMap {
                let infos = self.stream(SectionKind::TermInfo, field)?.map(|s| s.len());
                if shape.keys.checked_mul(TERM_INFO_LEN) != infos {
                    return Err(ReadError::Section {
                        kind: SectionKind::TermInfo,
                        field,
                        position: 0,
                        reason: SegmentCorruption::Fst,
                    });
                }
            }
            let range = section.range.start..section.range.end.saturating_sub(4);
            let payload = self.bytes(&Section {
                kind,
                field,
                range,
                table: None,
            })?;
            let map = fst::Map::new(payload).map_err(|_| fst_error(SegmentCorruption::Fst))?;
            check_fst_values(&map, kind, doc_count).map_err(fst_error)?;
            if kind == SectionKind::IdMap {
                if shape.keys != u64::from(doc_count) {
                    return Err(fst_error(SegmentCorruption::Fst));
                }
                self.id_map = Some(map);
            } else {
                self.term_dicts.push((field, map));
            }
        }
        Ok(())
    }
}

fn section_slice<'a>(segment: &'a [u8], section: &Section) -> &'a [u8] {
    // The footer parser checked every range against the segment.
    usize::try_from(section.range.start)
        .ok()
        .zip(usize::try_from(section.range.end).ok())
        .and_then(|(s, e)| segment.get(s..e))
        .unwrap_or_default()
}

/// The payload of a raw section, after checking its CRC (SPEC §5.3 "Framing").
fn raw_payload<'a>(segment: &'a [u8], section: &Section) -> Result<&'a [u8], ReadError> {
    let (kind, field) = (section.kind, section.field);
    let Some((payload, stored)) = section_slice(segment, section).split_last_chunk::<4>() else {
        return Err(ReadError::Section {
            kind,
            field,
            position: 0,
            reason: SegmentCorruption::Invalid {
                what: "a raw section shorter than its CRC",
            },
        });
    };
    let stored = u32::from_le_bytes(*stored);
    let computed = crc32c(payload);
    if stored != computed {
        return Err(ReadError::Section {
            kind,
            field,
            position: u64::try_from(payload.len()).unwrap_or(u64::MAX),
            reason: SegmentCorruption::RawChecksum { stored, computed },
        });
    }
    Ok(payload)
}

fn type_error(field: FieldId, field_type: FieldType, what: &'static str) -> ReadError {
    ReadError::FieldType {
        field,
        field_type,
        what,
    }
}

/// Walks an FST once (SPEC §5.3.1, §5.3.6): a `TermDict` maps each term to its rank, an `IdMap` each id to a distinct
/// docid below `doc_count`. `fst_check` has validated its structure and
/// key count, so the walk ends and cannot panic.
fn check_fst_values(
    map: &fst::Map<ByteSource>,
    kind: SectionKind,
    doc_count: u32,
) -> Result<(), SegmentCorruption> {
    let mut stream = map.stream();
    let mut rank = 0_u64;
    let mut docids = RoaringBitmap::new();
    while let Some((_, value)) = fst::Streamer::next(&mut stream) {
        let ok = if kind == SectionKind::IdMap {
            u32::try_from(value).is_ok_and(|d| d < doc_count && docids.insert(d))
        } else {
            value == rank
        };
        if !ok {
            return Err(SegmentCorruption::Invalid {
                what: "an FST value other than the term's rank or a distinct docid",
            });
        }
        rank = rank.saturating_add(1);
    }
    Ok(())
}
