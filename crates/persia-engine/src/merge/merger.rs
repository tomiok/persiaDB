//! The segment merger (SPEC §5.5, ADR-0031, ROADMAP 6.5.2): N segments in, their live documents in one
//! [`SegmentBuilder`] out, which [`SegmentWriter`] writes as it writes a flush.
//!
//! Nothing is analyzed again: term dictionaries are read in byte order and every live posting is copied with its
//! frequency and positions, docids remapped; columns, `DocStore` rows, ids and blob refs are copied as they are. The
//! writer recomputes every statistic (term `df`, `ttf`, `max_tf`, `min_norm`, norms, field statistics) from what was
//! copied, so the merged segment is the one a flush of the same documents, in the same order, writes.

use std::ops::Bound;

use persia_analysis::AnalyzerRegistry;
use persia_format::Writer;

use crate::schema::{CollectionSchema, Field, FieldId, FieldKind};
use crate::segment::builder::FieldBuilder;
use crate::segment::builder::inverted::Inverted;
use crate::segment::{
    MAX_DOCS, ReadError, Segment, SegmentBuilder, SegmentError, SegmentMeta, SegmentWriter,
    Tombstones, WriteOptions,
};

/// A docid the merge leaves out.
const GONE: u32 = u32::MAX;

/// Segment `rocompat_flags` this build knows how to rewrite (SPEC §5.1 defines none yet).
const KNOWN_ROCOMPAT_FLAGS: u64 = 0;

/// One segment a merge reads, and its deletions at the sidecar version the plan read (SPEC §8.7.2).
#[derive(Debug, Clone, Copy)]
pub struct MergeSource<'a> {
    /// The segment, opened with the schema the merge writes (the collection's current one).
    pub segment: &'a Segment,
    /// Its deleted documents; `None` when it has none. They must be this segment's: [`Tombstones`] does not record
    /// whose they are, so another segment's would delete the wrong documents unnoticed.
    pub tombstones: Option<&'a Tombstones>,
}

/// Why a merge failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MergeError {
    /// An input could not be read.
    #[error("segment {segment:032x}: {source}")]
    Read {
        /// The input's id.
        segment: u128,
        /// What failed.
        source: ReadError,
    },
    /// An input sets `rocompat_flags` this build does not know: it may be read, but a merge must not rewrite it
    /// (SPEC §5.1).
    #[error(
        "segment {segment:032x}: unknown rocompat flags {flags:#x}, a merge must not rewrite it"
    )]
    Unsupported {
        /// The input's id.
        segment: u128,
        /// Its flags.
        flags: u64,
    },
    /// An input contradicts itself in a way its reader does not check: a frequency of 0, positions other than the
    /// frequency, a field of more than `u32::MAX` tokens, or one id in two rows.
    #[error(
        "segment {segment:032x}{}: {what}",
        field.map(|f| format!(", field {f}")).unwrap_or_default()
    )]
    Corrupt {
        /// The input's id.
        segment: u128,
        /// The field, if the damage is in one.
        field: Option<FieldId>,
        /// What did not hold.
        what: &'static str,
    },
    /// Two inputs hold a live copy of `id`, the later one being `segment`. SPEC §8.3 rules that out (a commit leaves
    /// at most one): the tombstones passed are not the ones the plan read.
    #[error("segment {segment:032x}: id {id:?} is live in an earlier input too")]
    DuplicateLiveId {
        /// The later input's id.
        segment: u128,
        /// The document id.
        id: String,
    },
    /// An input was opened with another schema than the one the merge writes.
    #[error("segment {segment:032x} was opened with another schema than the merge's")]
    SchemaMismatch {
        /// The input's id.
        segment: u128,
    },
    /// The merged documents do not fit one segment, or `registry` lacks an analyzer.
    #[error(transparent)]
    Build(#[from] SegmentError),
}

/// The live documents of a merge's inputs, ready to write.
#[derive(Debug)]
pub struct MergedSegment {
    builder: SegmentBuilder,
    merge_generation: u32,
}

impl MergedSegment {
    /// Live documents the merge copied.
    pub fn doc_count(&self) -> u32 {
        self.builder.doc_count()
    }

    /// One more than the highest of the inputs' (SPEC §5.1).
    pub fn merge_generation(&self) -> u32 {
        self.merge_generation
    }

    /// The merged segment's bytes, with id `segment_id`, written at `created_at` (microseconds since the Unix epoch).
    ///
    /// # Errors
    /// As [`SegmentWriter::finish`].
    pub fn write(
        &self,
        segment_id: u128,
        created_at: i64,
        options: WriteOptions,
    ) -> Result<Vec<u8>, SegmentError> {
        let meta = SegmentMeta {
            segment_id,
            created_at,
            merge_generation: self.merge_generation,
        };
        SegmentWriter::new(&self.builder, meta)
            .with_options(options)
            .finish()
    }
}

/// Merges the live documents of `sources`, in their order, under `schema` (SPEC §5.5); `None` when none is live.
/// `registry` must have the analyzer of every `text` field, as for a flush, though nothing is analyzed.
///
/// ```
/// use persia_analysis::AnalyzerRegistry;
/// use persia_engine::document::{Document, Value};
/// use persia_engine::merge::{MergeSource, merge};
/// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
/// use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter, WriteOptions};
/// use persia_format::ByteSource;
///
/// let schema = SchemaBuilder::new("notes")
///     .field("body", FieldKind::Text(TextOptions::default()))
///     .build()?;
/// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
/// let registry = AnalyzerRegistry::builtin();
/// let flush = |id: u128, text: &str| -> Result<Segment, Box<dyn std::error::Error>> {
///     let mut b = SegmentBuilder::new(&schema, &registry)?;
///     b.add(&Document::new(format!("n{id}")).with(body, Value::String(text.into())))?;
///     let meta = SegmentMeta { segment_id: id, created_at: 0, merge_generation: 0 };
///     let bytes = SegmentWriter::new(&b, meta).finish()?;
///     Ok(Segment::open(ByteSource::new(bytes.into()), &schema)?)
/// };
/// let (a, b) = (flush(1, "hello world")?, flush(2, "hello again")?);
/// let sources = [MergeSource { segment: &a, tombstones: None }, MergeSource { segment: &b, tombstones: None }];
/// let merged = merge(&schema, &registry, &sources)?.ok_or("no live documents")?;
/// assert_eq!(merged.merge_generation(), 1);
/// let segment = Segment::open(ByteSource::new(merged.write(3, 0, WriteOptions::default())?.into()), &schema)?;
/// assert_eq!(segment.doc_count(), 2);
/// assert_eq!(segment.terms(body)?.get(b"hello")?.map(|t| t.df()), Some(2));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
/// [`MergeError::Read`] or [`MergeError::Corrupt`] when an input cannot be read; [`MergeError::Unsupported`] for an
/// input a merge must not rewrite; [`MergeError::SchemaMismatch`] for one opened with another schema;
/// [`MergeError::DuplicateLiveId`] when an id is live in two inputs; [`MergeError::Build`] when `registry` lacks an
/// analyzer or the documents do not fit one segment.
pub fn merge(
    schema: &CollectionSchema,
    registry: &AnalyzerRegistry,
    sources: &[MergeSource<'_>],
) -> Result<Option<MergedSegment>, MergeError> {
    let mut builder = SegmentBuilder::new(schema, registry)?;
    let mut generation = 0_u32;
    let mut scratch = Scratch::default();
    for source in sources {
        let segment = source.segment;
        let id = segment.segment_id();
        let flags = segment.rocompat_flags() & !KNOWN_ROCOMPAT_FLAGS;
        if flags != 0 {
            return Err(MergeError::Unsupported { segment: id, flags });
        }
        if segment.schema() != schema {
            return Err(MergeError::SchemaMismatch { segment: id });
        }
        generation = generation.max(segment.merge_generation());
        let docids = copy_documents(&mut builder, source)?;
        for (field, target) in schema.fields().iter().zip(builder.fields.iter_mut()) {
            match target {
                FieldBuilder::Text { inverted, .. } | FieldBuilder::Keyword { inverted } => {
                    copy_postings(segment, field.id(), &docids, inverted, &mut scratch)?;
                }
                FieldBuilder::Column(column) => {
                    copy_column(segment, field, &docids, &mut column.values)?;
                }
                FieldBuilder::Stored => {}
            }
        }
    }
    if builder.doc_count() == 0 {
        return Ok(None);
    }
    let merge_generation = generation.checked_add(1).ok_or(SegmentError::TooLarge {
        what: "merge generations",
    })?;
    Ok(Some(MergedSegment {
        builder,
        merge_generation,
    }))
}

/// Copies the live documents' ids, rows and blob refs; returns each input docid's merged docid, [`GONE`] for a
/// deleted one.
fn copy_documents(
    builder: &mut SegmentBuilder,
    source: &MergeSource<'_>,
) -> Result<Vec<u32>, MergeError> {
    let segment = source.segment;
    let read = |source| MergeError::Read {
        segment: segment.segment_id(),
        source,
    };
    // Merged docids below this came from earlier inputs.
    let first = builder.doc_count();
    let mut docids = Vec::with_capacity(segment.doc_count() as usize);
    for old in 0..segment.doc_count() {
        if source.tombstones.is_some_and(|t| t.contains(old)) {
            docids.push(GONE);
            continue;
        }
        let new = builder.doc_count();
        if new >= MAX_DOCS {
            return Err(SegmentError::Full { max_docs: MAX_DOCS }.into());
        }
        let (row, doc) = segment.row(old).map_err(read)?;
        let (earlier, fresh) =
            builder
                .ids
                .intern(doc.id().as_bytes())
                .map_err(|_| SegmentError::TooLarge {
                    what: "document ids",
                })?;
        if !fresh {
            // An id's arena id is its merged docid.
            return Err(if earlier < first {
                MergeError::DuplicateLiveId {
                    segment: segment.segment_id(),
                    id: doc.id().to_owned(),
                }
            } else {
                MergeError::Corrupt {
                    segment: segment.segment_id(),
                    field: None,
                    what: "one document id in two rows",
                }
            });
        }
        builder.rows.push(&row);
        if let Some(blob) = doc.blob_ref() {
            let mut w = Writer::new();
            blob.encode(&mut w);
            builder.blob_bytes.extend_from_slice(w.as_slice());
            builder.blobs.push((new, builder.blob_bytes.len()));
        }
        docids.push(new);
    }
    Ok(docids)
}

/// One live posting of the input being copied.
#[derive(Debug, Clone, Copy)]
struct Entry {
    doc: u32,
    term: u32,
    tf: u32,
    /// Where its positions start in [`Scratch::positions`].
    positions: usize,
}

/// Buffers reused across fields and inputs.
#[derive(Debug, Default)]
struct Scratch {
    entries: Vec<Entry>,
    positions: Vec<u32>,
}

/// Copies the live postings of one `text` or `keyword` field. Each input's dictionary is read in byte order and its
/// terms interned into the builder's one dictionary, inputs one after another; the writer sorts the union, as for a
/// flush, rather than a k-way merge of the inputs' streams. The postings are then regrouped by document, the order
/// [`Inverted`] keeps.
fn copy_postings(
    segment: &Segment,
    field: FieldId,
    docids: &[u32],
    inverted: &mut Inverted,
    scratch: &mut Scratch,
) -> Result<(), MergeError> {
    let id = segment.segment_id();
    let read = |source| MergeError::Read {
        segment: id,
        source,
    };
    let corrupt = |what| MergeError::Corrupt {
        segment: id,
        field: Some(field),
        what,
    };
    let keep_positions = inverted.positions.is_some();
    let terms = segment.terms(field).map_err(read)?;
    let mut stream = terms.range(Bound::Unbounded, Bound::Unbounded);
    scratch.entries.clear();
    scratch.positions.clear();
    while let Some(next) = stream.next() {
        let (term, info) = next.map_err(read)?;
        let mut postings = terms.postings(&info).map_err(read)?;
        // Interned only once a live posting needs it: a term every document of which is deleted is dropped.
        let mut term_id = None;
        while let Some(old) = postings.next().map_err(read)? {
            let new = docids.get(old as usize).copied().unwrap_or(GONE);
            if new == GONE {
                continue;
            }
            let tf = postings.freq().map_err(read)?;
            if tf == 0 {
                return Err(corrupt("a posting with frequency 0"));
            }
            let start = scratch.positions.len();
            if keep_positions {
                let positions = postings.positions().map_err(read)?;
                if positions.len() != tf as usize {
                    return Err(corrupt("positions that do not match the frequency"));
                }
                scratch.positions.extend_from_slice(positions);
            }
            let term = if let Some(t) = term_id {
                t
            } else {
                let (t, _) = inverted.intern(term)?;
                term_id = Some(t);
                t
            };
            scratch.entries.push(Entry {
                doc: new,
                term,
                tf,
                positions: start,
            });
        }
    }
    scratch.entries.sort_unstable_by_key(|e| (e.doc, e.term));
    for group in scratch.entries.chunk_by(|a, b| a.doc == b.doc) {
        let mut len = 0_u32;
        for e in group {
            let positions = if keep_positions {
                let end = e.positions.saturating_add(e.tf as usize);
                scratch.positions.get(e.positions..end).unwrap_or_default()
            } else {
                &[]
            };
            inverted.add_posting(e.doc, e.term, e.tf, positions)?;
            // A flush rejects a field of more tokens, so only a damaged input gets here.
            len = len
                .checked_add(e.tf)
                .ok_or_else(|| corrupt("a field of more than u32::MAX tokens"))?;
        }
        if let Some(e) = group.first() {
            inverted.end_doc(e.doc, len);
        }
    }
    Ok(())
}

/// Copies the live values of one `i64`, `f64`, `datetime` or `bool` field, as order-preserving keys (SPEC §5.2.1).
fn copy_column(
    segment: &Segment,
    field: &Field,
    docids: &[u32],
    values: &mut Vec<(u32, u64)>,
) -> Result<(), MergeError> {
    let read = |source| MergeError::Read {
        segment: segment.segment_id(),
        source,
    };
    let live = (0_u32..).zip(docids).filter(|&(_, &new)| new != GONE);
    if matches!(field.kind(), FieldKind::Bool { .. }) {
        let column = segment.bool_column(field.id()).map_err(read)?;
        for (old, &new) in live {
            if let Some(v) = column.get(old) {
                values.push((new, u64::from(v)));
            }
        }
    } else {
        let column = segment.numeric_column(field.id()).map_err(read)?;
        let mut cursor = column.cursor();
        for (old, &new) in live {
            if let Some(key) = cursor.key(old).map_err(read)? {
                values.push((new, key));
            }
        }
    }
    Ok(())
}
