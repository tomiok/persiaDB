//! The in-memory index builder: documents in, everything a segment holds accumulated per field (ROADMAP 5.1).
//!
//! [`SegmentBuilder::add`] validates and encodes a document, then appends to per-field accumulators that never
//! allocate per term or per document: interned terms ([`Arena`]), one flat posting log per field
//! ([`inverted::Inverted`]), sparse column values ([`Column`]), and the `DocStore` rows and ids as flat records.
//! [`super::SegmentWriter`] turns them into sections (ROADMAP 5.2).

pub(crate) mod inverted;

use std::sync::Arc;

use persia_analysis::{Analyzer, AnalyzerRegistry, TokenStream};
use persia_format::Writer;
use persia_format::for_codec::{f64_to_key, i64_to_key};

use super::SegmentError;
use super::arena::Arena;
use crate::document::{Document, FieldSet, Value};
use crate::schema::{CollectionSchema, FieldId, FieldKind};
use inverted::Inverted;

/// Most documents in one segment (SPEC §5.4): docids are `0..2^31`.
pub const MAX_DOCS: u32 = 1 << 31;

/// Positions between two values of a multi-valued `text` field, so a phrase never matches across values
/// (SPEC §5.3.3).
pub const POSITION_GAP: u32 = 100;

/// Longest term of a `TermDict`, in bytes (SPEC §5.3.1): a `keyword` value's limit, and the most a `text` field's
/// token may hold, whatever its analyzer (one without a length guard, §6.3.1, may emit longer ones).
pub const MAX_TERM_LEN: usize = crate::document::MAX_KEYWORD_LEN;

/// What a builder accumulates for one field.
#[derive(Debug)]
pub(crate) enum FieldBuilder {
    /// A `text` field: terms of its analyzer, positions if the field asks for them.
    Text {
        analyzer: Arc<Analyzer>,
        inverted: Inverted,
    },
    /// A `keyword` field: each value is one term; its column is derived from the postings.
    Keyword { inverted: Inverted },
    /// An `i64`, `f64` or `datetime` field: order-preserving keys (SPEC §5.2.1); `bool`: 0 or 1.
    Column(Column),
    /// A `stored` field: `DocStore` only.
    Stored,
}

/// Sparse column values: `(docid, key)` in docid order, one per document that has the field.
#[derive(Debug, Default)]
pub(crate) struct Column {
    pub(crate) values: Vec<(u32, u64)>,
}

/// Records in docid order, concatenated: record `d` is `bytes[ends[d-1]..ends[d]]` (SPEC §5.3.6).
#[derive(Debug, Default)]
pub(crate) struct Records {
    pub(crate) bytes: Vec<u8>,
    pub(crate) ends: Vec<usize>,
}

impl Records {
    fn push(&mut self, record: &[u8]) {
        self.bytes.extend_from_slice(record);
        self.ends.push(self.bytes.len());
    }

    fn size_in_bytes(&self) -> usize {
        self.bytes
            .capacity()
            .saturating_add(self.ends.capacity().saturating_mul(size_of::<usize>()))
    }
}

/// Accumulates documents for one segment (SPEC §5, ROADMAP 5.1); [`super::SegmentWriter`] writes it.
///
/// Docids are assigned in insertion order, from 0 (SPEC §5.4). Every document is validated against the schema; a
/// document that fails, or whose id the segment already holds, is rejected and leaves the builder as it was.
///
/// ```
/// use persia_analysis::AnalyzerRegistry;
/// use persia_engine::document::{Document, Value};
/// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
/// use persia_engine::segment::SegmentBuilder;
///
/// let schema = SchemaBuilder::new("notes")
///     .field("body", FieldKind::Text(TextOptions::default()))
///     .build()?;
/// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
/// let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
/// let docid = builder.add(&Document::new("n1").with(body, Value::String("Hello, world".into())))?;
/// assert_eq!((docid, builder.doc_count()), (0, 1));
/// assert!(builder.size_in_bytes() > 0);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct SegmentBuilder {
    pub(crate) schema: CollectionSchema,
    /// Indexed by field id.
    pub(crate) fields: Vec<FieldBuilder>,
    /// Document ids; a document's docid is its id's arena id.
    pub(crate) ids: Arena,
    /// `DocStore` rows (SPEC §6.4.2).
    pub(crate) rows: Records,
    /// `(docid, end)` of each document's `BlobRef` encoding in `blob_bytes`, in docid order.
    pub(crate) blobs: Vec<(u32, usize)>,
    pub(crate) blob_bytes: Vec<u8>,
    max_docs: u32,
    /// Largest position a token may get; `u32::MAX` but in tests.
    max_position: u32,
    tokens: TokenStream,
    /// `(term id, position)` of the field being indexed.
    occurrences: Vec<(u32, u32)>,
}

impl SegmentBuilder {
    /// A builder for documents of `schema`, analyzing `text` fields with the analyzers `registry` has for them.
    ///
    /// # Errors
    /// [`SegmentError::UnknownAnalyzer`] if `registry` lacks the analyzer (name and version) of a `text` field.
    pub fn new(
        schema: &CollectionSchema,
        registry: &AnalyzerRegistry,
    ) -> Result<Self, SegmentError> {
        let mut fields = Vec::with_capacity(schema.fields().len());
        for field in schema.fields() {
            fields.push(match field.kind() {
                FieldKind::Text(options) => {
                    let analyzer = &options.analyzer;
                    let found = registry
                        .get(analyzer.name(), analyzer.version())
                        .ok_or_else(|| SegmentError::UnknownAnalyzer {
                            field: field.name().to_owned(),
                            analyzer: analyzer.to_string(),
                        })?;
                    FieldBuilder::Text {
                        analyzer: Arc::clone(found),
                        inverted: Inverted::new(options.positions),
                    }
                }
                FieldKind::Keyword(_) => FieldBuilder::Keyword {
                    inverted: Inverted::new(false),
                },
                FieldKind::I64 { .. }
                | FieldKind::F64 { .. }
                | FieldKind::Bool { .. }
                | FieldKind::Datetime { .. } => FieldBuilder::Column(Column::default()),
                FieldKind::Stored => FieldBuilder::Stored,
            });
        }
        Ok(Self {
            schema: schema.clone(),
            fields,
            ids: Arena::default(),
            rows: Records::default(),
            blobs: Vec::new(),
            blob_bytes: Vec::new(),
            max_docs: MAX_DOCS,
            max_position: u32::MAX,
            tokens: TokenStream::new(),
            occurrences: Vec::new(),
        })
    }

    /// Documents added so far.
    pub fn doc_count(&self) -> u32 {
        // At most `max_docs` <= 2^31 ids.
        u32::try_from(self.ids.len()).unwrap_or(u32::MAX)
    }

    /// The schema documents are indexed with.
    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// Adds `doc` and returns its docid.
    ///
    /// # Errors
    /// [`SegmentError::Document`] for a document `schema` rejects, [`SegmentError::DuplicateId`] for an id already
    /// added, [`SegmentError::Full`] at [`MAX_DOCS`] documents, [`SegmentError::TooLarge`] when the document would
    /// overflow an internal index (a position past `u32::MAX`, more than `u32::MAX` terms). The builder is then as it
    /// was (only spare capacity may have grown, so [`Self::size_in_bytes`] can too).
    pub fn add(&mut self, doc: &Document) -> Result<u32, SegmentError> {
        let docid = self.doc_count();
        if docid >= self.max_docs {
            return Err(SegmentError::Full {
                max_docs: self.max_docs,
            });
        }
        // Validates the whole document before anything changes.
        let row = doc.encode(&self.schema, FieldSet::Stored)?;
        if self.ids.get(doc.id().as_bytes()).is_some() {
            return Err(SegmentError::DuplicateId {
                id: doc.id().to_owned(),
            });
        }
        let mark = self.mark();
        let result = self.index(docid, doc, &row);
        if result.is_err() {
            self.rollback(&mark);
        }
        result.map(|()| docid)
    }

    fn index(&mut self, docid: u32, doc: &Document, row: &[u8]) -> Result<(), SegmentError> {
        self.ids
            .intern(doc.id().as_bytes())
            .map_err(|_| SegmentError::TooLarge {
                what: "document ids",
            })?;
        self.rows.push(row);
        if let Some(blob) = doc.blob_ref() {
            let mut w = Writer::new();
            blob.encode(&mut w);
            self.blob_bytes.extend_from_slice(w.as_slice());
            self.blobs.push((docid, self.blob_bytes.len()));
        }
        for (field, value) in doc.fields() {
            let Some(builder) = usize::try_from(field.get())
                .ok()
                .and_then(|i| self.fields.get_mut(i))
            else {
                continue;
            };
            match (builder, value) {
                (FieldBuilder::Text { analyzer, inverted }, Value::String(s)) => {
                    index_text(
                        analyzer,
                        (field, inverted),
                        docid,
                        [s.as_str()],
                        self.max_position,
                        &mut self.tokens,
                        &mut self.occurrences,
                    )?;
                }
                (FieldBuilder::Text { analyzer, inverted }, Value::Strings(list)) => {
                    let values = list.iter().map(String::as_str);
                    index_text(
                        analyzer,
                        (field, inverted),
                        docid,
                        values,
                        self.max_position,
                        &mut self.tokens,
                        &mut self.occurrences,
                    )?;
                }
                (FieldBuilder::Keyword { inverted }, Value::String(s)) => {
                    index_keywords(inverted, docid, [s.as_str()], &mut self.occurrences)?;
                }
                (FieldBuilder::Keyword { inverted }, Value::Strings(list)) => {
                    let values = list.iter().map(String::as_str);
                    index_keywords(inverted, docid, values, &mut self.occurrences)?;
                }
                (FieldBuilder::Column(column), value) => {
                    let key = match *value {
                        Value::I64(v) | Value::Datetime(v) => i64_to_key(v),
                        Value::F64(v) => f64_to_key(v),
                        Value::Bool(v) => u64::from(v),
                        // `encode` validated the document: no other value reaches a column field.
                        _ => continue,
                    };
                    column.values.push((docid, key));
                }
                // `stored` values live in the row only; validation rules out any other pairing.
                _ => {}
            }
        }
        Ok(())
    }

    /// Heap bytes the builder holds, for flush thresholds (ROADMAP 5.1.5). Counts capacities, so it tracks
    /// what the allocator handed out, not just what is filled, analysis scratch buffers included; it excludes the
    /// schema and analyzers, which do not grow with the documents.
    pub fn size_in_bytes(&self) -> usize {
        let fields = self.fields.iter().fold(0_usize, |sum, field| {
            sum.saturating_add(match field {
                FieldBuilder::Text { inverted, .. } | FieldBuilder::Keyword { inverted } => {
                    inverted.size_in_bytes()
                }
                FieldBuilder::Column(column) => column
                    .values
                    .capacity()
                    .saturating_mul(size_of::<(u32, u64)>()),
                FieldBuilder::Stored => 0,
            })
        });
        fields
            .saturating_add(self.ids.size_in_bytes())
            .saturating_add(self.rows.size_in_bytes())
            .saturating_add(
                self.blobs
                    .capacity()
                    .saturating_mul(size_of::<(u32, usize)>()),
            )
            .saturating_add(self.blob_bytes.capacity())
            .saturating_add(self.tokens.size_in_bytes())
            .saturating_add(
                self.occurrences
                    .capacity()
                    .saturating_mul(size_of::<(u32, u32)>()),
            )
    }

    /// Where every accumulator ends before a document, so a failed one can be undone.
    fn mark(&self) -> Mark {
        Mark {
            ids: self.ids.len(),
            rows: self.rows.ends.len(),
            row_bytes: self.rows.bytes.len(),
            blobs: self.blobs.len(),
            blob_bytes: self.blob_bytes.len(),
            fields: self
                .fields
                .iter()
                .map(|f| match f {
                    FieldBuilder::Text { inverted, .. } | FieldBuilder::Keyword { inverted } => {
                        FieldMark::Inverted(inverted.mark())
                    }
                    FieldBuilder::Column(c) => FieldMark::Column(c.values.len()),
                    FieldBuilder::Stored => FieldMark::None,
                })
                .collect(),
        }
    }

    /// Undoes everything added since `mark`.
    fn rollback(&mut self, mark: &Mark) {
        self.ids.truncate(mark.ids);
        self.rows.ends.truncate(mark.rows);
        self.rows.bytes.truncate(mark.row_bytes);
        self.blobs.truncate(mark.blobs);
        self.blob_bytes.truncate(mark.blob_bytes);
        for (field, m) in self.fields.iter_mut().zip(&mark.fields) {
            match (field, m) {
                (
                    FieldBuilder::Text { inverted, .. } | FieldBuilder::Keyword { inverted },
                    FieldMark::Inverted(m),
                ) => inverted.rollback(m),
                (FieldBuilder::Column(c), &FieldMark::Column(n)) => c.values.truncate(n),
                _ => {}
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_max_position(&mut self, max_position: u32) {
        self.max_position = max_position;
    }

    #[cfg(test)]
    pub(crate) fn set_max_docs(&mut self, max_docs: u32) {
        self.max_docs = max_docs;
    }
}

/// The lengths of a builder's accumulators before a document.
#[derive(Debug)]
struct Mark {
    ids: usize,
    rows: usize,
    row_bytes: usize,
    blobs: usize,
    blob_bytes: usize,
    fields: Vec<FieldMark>,
}

#[derive(Debug)]
enum FieldMark {
    Inverted(inverted::Mark),
    Column(usize),
    None,
}

/// Indexes the values of one `text` field of document `docid` (SPEC §5.3.3: value `i + 1` starts
/// [`POSITION_GAP`] positions after the end of value `i`); a token longer than [`MAX_TERM_LEN`] rejects
/// the document (SPEC §5.3.1).
fn index_text<'a>(
    analyzer: &Analyzer,
    (field, inverted): (FieldId, &mut Inverted),
    docid: u32,
    values: impl IntoIterator<Item = &'a str>,
    max_position: u32,
    tokens: &mut TokenStream,
    occurrences: &mut Vec<(u32, u32)>,
) -> Result<(), SegmentError> {
    occurrences.clear();
    // u64: the base after the last value may pass u32::MAX without any position doing so.
    let mut base = 0_u64;
    let mut len = 0_u32;
    let overflow = || SegmentError::TooLarge { what: "positions" };
    for value in values {
        analyzer.analyze(value, tokens);
        let mut end = 0_u64;
        for token in tokens.iter() {
            let position = u32::try_from(base.saturating_add(u64::from(token.position)))
                .ok()
                .filter(|&p| p <= max_position)
                .ok_or_else(overflow)?;
            if token.text.len() > MAX_TERM_LEN {
                return Err(SegmentError::TermTooLong {
                    field,
                    len: token.text.len(),
                });
            }
            let (term, _) = inverted.intern(token.text.as_bytes())?;
            occurrences.push((term, position));
            end = end.max(u64::from(token.position).saturating_add(1));
            len = len.checked_add(1).ok_or_else(overflow)?;
        }
        base = base
            .saturating_add(end)
            .saturating_add(u64::from(POSITION_GAP));
    }
    inverted.add_doc(docid, occurrences, len)
}

/// Indexes the values of one `keyword` field of document `docid`: each value is one term, as is.
fn index_keywords<'a>(
    inverted: &mut Inverted,
    docid: u32,
    values: impl IntoIterator<Item = &'a str>,
    occurrences: &mut Vec<(u32, u32)>,
) -> Result<(), SegmentError> {
    occurrences.clear();
    let mut len = 0_u32;
    for value in values {
        let (term, _) = inverted.intern(value.as_bytes())?;
        // Keywords have no positions; the value index keeps the pairs distinct.
        occurrences.push((term, len));
        len = len.checked_add(1).ok_or(SegmentError::TooLarge {
            what: "keyword values",
        })?;
    }
    inverted.add_doc(docid, occurrences, len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{KeywordOptions, SchemaBuilder, TextOptions};

    fn builder() -> (SegmentBuilder, FieldId, FieldId) {
        let schema = SchemaBuilder::new("c")
            .field(
                "t",
                FieldKind::Text(TextOptions {
                    positions: true,
                    ..TextOptions::default()
                }),
            )
            .field("k", FieldKind::Keyword(KeywordOptions::default()))
            .build()
            .unwrap();
        let (t, k) = (
            schema.field("t").unwrap().id(),
            schema.field("k").unwrap().id(),
        );
        (
            SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap(),
            t,
            k,
        )
    }

    #[test]
    fn full_segment_rejects_without_changing() {
        let (mut b, _, k) = builder();
        b.set_max_docs(2);
        b.add(&Document::new("a")).unwrap();
        b.add(&Document::new("b")).unwrap();
        let size = b.size_in_bytes();
        let doc = Document::new("c").with(k, Value::String("v".into()));
        assert_eq!(b.add(&doc), Err(SegmentError::Full { max_docs: 2 }));
        assert_eq!((b.doc_count(), b.size_in_bytes()), (2, size));
    }

    /// A `Full` rejection leaves the segment byte-identical to never having tried (`add` docs).
    #[test]
    fn full_rejection_leaves_output_byte_identical() {
        use crate::segment::{SegmentMeta, SegmentWriter};
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 2,
            merge_generation: 0,
        };
        let (mut a, t, k) = builder();
        let (mut b, _, _) = builder();
        let doc = |id: &str| {
            Document::new(id)
                .with(t, Value::String(format!("{id} words")))
                .with(k, Value::String(id.into()))
        };
        for x in [&mut a, &mut b] {
            x.set_max_docs(2);
            x.add(&doc("a")).unwrap();
            x.add(&doc("b")).unwrap();
        }
        assert_eq!(b.add(&doc("c")), Err(SegmentError::Full { max_docs: 2 }));
        assert_eq!(b.add(&doc("a")), Err(SegmentError::Full { max_docs: 2 }));
        assert_eq!(
            SegmentWriter::new(&a, meta).finish().unwrap(),
            SegmentWriter::new(&b, meta).finish().unwrap()
        );
    }

    /// Duplicates every token at its own position: a stacking filter whose stacked tokens share their text.
    struct Twice;
    impl persia_analysis::TokenFilter for Twice {
        fn apply(&self, tokens: &mut TokenStream) {
            tokens.rebuild(|token, out| {
                out.push(&token.text, token.start, token.end, token.position);
                out.push_token(token);
            });
        }
        fn stacks_tokens(&self) -> bool {
            true
        }
    }

    /// An analyzer without a length guard may yield any token: one longer than [`MAX_TERM_LEN`] rejects its
    /// document and leaves the builder as it was; one of exactly that length is indexed (SPEC §5.3.1, ROADMAP 5.4.5).
    #[test]
    fn tokens_longer_than_max_term_len_reject_the_document() {
        use crate::segment::{Segment, SegmentMeta, SegmentWriter};
        use persia_analysis::WhitespaceTokenizer;
        let (mut b, t, _) = builder();
        match &mut b.fields[t.get() as usize] {
            FieldBuilder::Text { analyzer, .. } => {
                *analyzer = Arc::new(Analyzer::builder(WhitespaceTokenizer).build());
            }
            _ => panic!("t is a text field"),
        }
        b.add(&Document::new("a").with(t, Value::String("short".into())))
            .unwrap();
        let long = format!("x {}", "y".repeat(MAX_TERM_LEN + 1));
        assert_eq!(
            b.add(&Document::new("b").with(t, Value::Strings(vec!["ok".into(), long]))),
            Err(SegmentError::TermTooLong {
                field: t,
                len: MAX_TERM_LEN + 1
            })
        );
        assert_eq!(b.doc_count(), 1);
        let longest = "y".repeat(MAX_TERM_LEN);
        b.add(&Document::new("c").with(t, Value::String(longest.clone())))
            .unwrap();
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
        let seg = Segment::open(persia_format::ByteSource::new(bytes.into()), &b.schema).unwrap();
        let info = seg.terms(t).unwrap().get(longest.as_bytes()).unwrap();
        assert_eq!(info.map(|i| i.df()), Some(1));
        // The rejected document left nothing behind.
        assert_eq!(seg.doc_count(), 2);
        assert!(seg.terms(t).unwrap().get(b"ok").unwrap().is_none());
    }

    /// No built-in analyzer stacks tokens and a schema can only name built-ins, so this swaps the analyzer in:
    /// stacked tokens (edge n-grams, and equal tokens at one position) count in `tf` and the field length, their
    /// positions repeat (delta 0, SPEC §5.3.3), and the writer accepts them.
    #[test]
    fn stacked_tokens_are_indexed_and_written() {
        use crate::segment::{SegmentMeta, SegmentWriter};
        use persia_analysis::{EdgeNgramFilter, WhitespaceTokenizer};
        use std::num::NonZeroUsize;
        let n = |n| NonZeroUsize::new(n).unwrap();
        let (mut b, t, _) = builder();
        let stacking = Analyzer::builder(WhitespaceTokenizer)
            .filter(
                EdgeNgramFilter::new(n(1), n(3))
                    .unwrap()
                    .preserve_original(true),
            )
            .filter(Twice)
            .build();
        assert!(stacking.stacks_tokens());
        match &mut b.fields[t.get() as usize] {
            FieldBuilder::Text { analyzer, .. } => *analyzer = Arc::new(stacking),
            _ => panic!("t is a text field"),
        }
        let doc = Document::new("d").with(t, Value::Strings(vec!["abcd ab".into(), "a".into()]));
        b.add(&doc).unwrap();
        let FieldBuilder::Text { inverted, .. } = &b.fields[t.get() as usize] else {
            panic!("t is a text field")
        };
        let g = inverted.by_term();
        let got: Vec<(&[u8], u32, &[u32])> = g
            .order
            .iter()
            .zip(&g.postings)
            .map(|(&id, p)| (inverted.terms.key(id), p.tf, inverted.positions_of(p)))
            .collect();
        // Value 0: a ab abc abcd @0, a ab @1 (each twice); end 2. Value 1 starts at 102: a @102 (twice).
        assert_eq!(
            got,
            [
                (&b"a"[..], 6, &[0, 0, 1, 1, 102, 102][..]),
                (b"ab", 4, &[0, 0, 1, 1]),
                (b"abc", 2, &[0, 0]),
                (b"abcd", 2, &[0, 0]),
            ]
        );
        assert_eq!(inverted.lengths, [(0, 14)]);
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        SegmentWriter::new(&b, meta).finish().unwrap();
    }

    #[test]
    fn values_are_a_position_gap_apart() {
        let (b, t, _) = builder();
        let mut inverted = Inverted::new(true);
        let mut tokens = TokenStream::new();
        let mut occ = Vec::new();
        let analyzer = match &b.fields[t.get() as usize] {
            FieldBuilder::Text { analyzer, .. } => Arc::clone(analyzer),
            _ => panic!("t is a text field"),
        };
        let many = vec!["x"; 3];
        index_text(
            &analyzer,
            (t, &mut inverted),
            0,
            many,
            u32::MAX,
            &mut tokens,
            &mut occ,
        )
        .unwrap();
        assert_eq!(inverted.positions.as_deref(), Some(&[0, 101, 202][..]));
        assert_eq!(inverted.lengths, [(0, 3)]);
    }

    /// A document that overflows positions halfway is rolled back entirely: new terms, postings, its id and row,
    /// columns of fields indexed before the failing one.
    #[test]
    fn too_large_rolls_the_document_back() {
        use crate::segment::{SegmentMeta, SegmentWriter};
        let schema = SchemaBuilder::new("c")
            .field("a", FieldKind::I64 { stored: true })
            .field("k", FieldKind::Keyword(KeywordOptions::default()))
            .field(
                "t",
                FieldKind::Text(TextOptions {
                    positions: true,
                    ..TextOptions::default()
                }),
            )
            .build()
            .unwrap();
        let id = |n: &str| schema.field(n).unwrap().id();
        let registry = AnalyzerRegistry::builtin();
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let ok = Document::new("ok")
            .with(id("t"), Value::String("one two".into()))
            .with(id("k"), Value::String("x".into()));
        // Positions 0 and 101 + 0: the second value passes a limit of 100.
        let bad = Document::new("bad")
            .with(id("a"), Value::I64(7))
            .with(id("k"), Value::String("new".into()))
            .with(
                id("t"),
                Value::Strings(vec!["fresh".into(), "words".into()]),
            );

        let mut expected = SegmentBuilder::new(&schema, &registry).unwrap();
        expected.add(&ok).unwrap();
        let expected = SegmentWriter::new(&expected, meta).finish().unwrap();

        let mut b = SegmentBuilder::new(&schema, &registry).unwrap();
        b.set_max_position(100);
        b.add(&ok).unwrap();
        assert_eq!(
            b.add(&bad),
            Err(SegmentError::TooLarge { what: "positions" })
        );
        assert_eq!(b.doc_count(), 1);
        assert_eq!(SegmentWriter::new(&b, meta).finish().unwrap(), expected);
        // The id is free again, and the builder keeps working.
        b.set_max_position(u32::MAX);
        assert_eq!(b.add(&bad), Ok(1));
    }

    #[test]
    fn size_in_bytes_counts_the_analysis_buffers() {
        let (mut b, t, _) = builder();
        let text = "many words to analyze ".repeat(200);
        b.add(&Document::new("a").with(t, Value::String(text)))
            .unwrap();
        let (total, tokens) = (b.size_in_bytes(), b.tokens.size_in_bytes());
        assert!(tokens > 0);
        b.tokens = TokenStream::new();
        assert_eq!(b.size_in_bytes(), total - tokens);
    }

    #[test]
    fn keyword_values_are_terms_as_is() {
        let (mut b, _, k) = builder();
        let doc = Document::new("a").with(
            k,
            Value::Strings(vec!["B".into(), "a".into(), "B".into(), String::new()]),
        );
        b.add(&doc).unwrap();
        let FieldBuilder::Keyword { inverted } = &b.fields[k.get() as usize] else {
            panic!("k is a keyword field")
        };
        let g = inverted.by_term();
        let terms: Vec<(&[u8], u32)> = g
            .order
            .iter()
            .zip(&g.postings)
            .map(|(&id, p)| (inverted.terms.key(id), p.tf))
            .collect();
        assert_eq!(terms, [(&b""[..], 1), (b"B", 2), (b"a", 1)]);
        assert_eq!((inverted.sum_len, inverted.docs_with_field()), (4, 1));
    }
}
