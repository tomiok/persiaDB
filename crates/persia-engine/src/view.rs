//! What a search runs over, whatever holds the documents (ROADMAP 6.1.2, 6.1.5): a [`SearchView`] gives a field's
//! postings as a [`PostingsIterator`], its terms by range or automaton as a [`TermEnum`], norms, column values,
//! statistics and stored documents. A [`SegmentView`] (a segment and its tombstones, SPEC §8.3) and a
//! [`crate::memtable::MemtableView`] (SPEC §8.2) both implement it, so the query engine treats the memtable as one
//! more segment (ROADMAP 7.4.6).
//!
//! Both leave out deleted documents: a view's postings, [`SearchView::is_live`] and [`SearchView::docid_of`] only
//! see the documents a search may return. Column scans ([`SearchView::present`], [`SearchView::column_keys`])
//! include them; [`SearchView::deleted`] is what to take out.

use core::ops::Bound;

use fst::Automaton;
#[cfg(doc)]
use fst::automaton::AlwaysMatch;
use roaring::RoaringBitmap;

use crate::document::Document;
use crate::schema::{FieldId, FieldType};
use crate::score::FieldHit;
use crate::segment::{
    BlockMax, BoolColumn, FieldStats, KeywordOrds, NormsCursor, NumericKeys, Postings, ReadError,
    Segment, TermStream, Tombstones,
};

/// The documents of one term, in docid order, with their frequencies and positions; the interface of
/// [`Postings`].
///
/// A new iterator is before its first document: [`Self::next`] or [`Self::advance`] moves it onto one. After an
/// error the iterator is exhausted.
pub trait PostingsIterator {
    /// An upper bound on the documents the iterator yields (a segment's `df` counts deleted documents too).
    fn df(&self) -> u32;

    /// The current document, `None` before the first and after the last.
    fn doc(&self) -> Option<u32>;

    /// Moves to the next document and returns it, or `None` at the end.
    ///
    /// # Errors
    /// A corruption of the source.
    fn next(&mut self) -> Result<Option<u32>, ReadError>;

    /// Moves to the first document at or after `target` and returns it, or `None` if there is none; never moves
    /// back.
    ///
    /// # Errors
    /// As [`Self::next`].
    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError>;

    /// The current document's frequency, at least 1; 0 when not on a document.
    ///
    /// # Errors
    /// As [`Self::next`].
    fn freq(&mut self) -> Result<u32, ReadError>;

    /// The current document's positions, ascending (stacked tokens may repeat one); empty when not on a document
    /// or when the field does not record positions.
    ///
    /// # Errors
    /// As [`Self::next`].
    fn positions(&mut self) -> Result<&[u32], ReadError>;

    /// The score bounds of the current document's block; `None` when not on a document.
    fn block_max(&self) -> Option<BlockMax>;

    /// The score bounds of the block holding the first document at or after `target`, without moving; `None` if
    /// there is none. The bounds may cover deleted documents.
    ///
    /// # Errors
    /// As [`Self::next`].
    fn shallow_advance(&mut self, target: u32) -> Result<Option<BlockMax>, ReadError>;

    /// The largest frequency and the smallest norm byte of the whole list, without reading it: they bound the
    /// score of each of its documents (`MaxScore`, SPEC §7.3). They may cover deleted documents.
    fn term_bound(&self) -> FieldHit;
}

/// The norm bytes of one `text` field (SPEC §5.3.4), by docid.
pub trait NormReader {
    /// The norm byte of `docid`; 0 for a document without tokens in the field (or a docid past the source).
    ///
    /// # Errors
    /// A corruption of the source.
    fn get(&mut self, docid: u32) -> Result<u8, ReadError>;
}

impl NormReader for NormsCursor<'_> {
    fn get(&mut self, docid: u32) -> Result<u8, ReadError> {
        NormsCursor::get(self, docid)
    }
}

/// A field's terms in byte order, each with the `df` of its postings, from [`SearchView::terms`]; a lending stream:
/// the term borrows it.
pub trait TermEnum {
    /// The next term and its `df` ([`PostingsIterator::df`]: a segment's counts deleted documents too), or `None`
    /// after the last.
    ///
    /// # Errors
    /// A corruption of the source.
    fn next(&mut self) -> Result<Option<(&[u8], u32)>, ReadError>;
}

impl<A: Automaton> TermEnum for TermStream<'_, A> {
    fn next(&mut self) -> Result<Option<(&[u8], u32)>, ReadError> {
        TermStream::next(self)
            .transpose()
            .map(|t| t.map(|(term, info)| (term, info.df())))
    }
}

/// A column's order-preserving keys by docid (SPEC §5.2.1), from [`SearchView::keys`]: what a sort compares
/// (ROADMAP 7.5.1). Reading docids in ascending order is the fast path.
pub trait KeyReader {
    /// The key of `docid`: for an `i64`, `f64` or `datetime` field the column's key, for a `bool` 0 or 1; `None` for
    /// a document without a value (or a docid past the view).
    ///
    /// # Errors
    /// A corruption of the source.
    fn key(&mut self, docid: u32) -> Result<Option<u64>, ReadError>;
}

/// A `keyword` field's values by docid, as ordinals of a dictionary in byte order (SPEC §5.3.5), from
/// [`SearchView::keywords`]: what a sort and a facet read (ROADMAP 7.5.1, 7.5.3). The ordinals are the view's own: a
/// lower ord is a smaller value in the same view, and values compare across views by their bytes.
pub trait KeywordReader {
    /// The ordinals of `docid`'s distinct values, ascending; empty when the document has none.
    ///
    /// # Errors
    /// A corruption of the source. A docid at or above a memtable view's document count is
    /// [`ReadError::DocOutOfRange`]: that is not an absent value, and it must not sort or facet as one. A segment
    /// column returns an empty slice for a docid past the segment (its `starts` column defines the range).
    fn ords(&mut self, docid: u32) -> Result<&[u64], ReadError>;

    /// The value of `ord`; `None` past the dictionary.
    fn value(&self, ord: u64) -> Option<&[u8]>;

    /// The number of values in the dictionary: every ord is below it.
    fn len(&self) -> u64;

    /// Whether the dictionary is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl KeyReader for NumericKeys<'_> {
    fn key(&mut self, docid: u32) -> Result<Option<u64>, ReadError> {
        NumericKeys::key(self, docid)
    }
}

impl KeywordReader for KeywordOrds<'_> {
    fn ords(&mut self, docid: u32) -> Result<&[u64], ReadError> {
        KeywordOrds::ords(self, docid)
    }

    fn value(&self, ord: u64) -> Option<&[u8]> {
        self.column().value(ord)
    }

    fn len(&self) -> u64 {
        self.column().len()
    }
}

/// The keys of a segment's column, from [`SegmentView::keys`].
#[derive(Debug, Clone)]
pub enum SegmentKeys<'s> {
    /// An `i64`, `f64` or `datetime` field.
    Numeric(NumericKeys<'s>),
    /// A `bool` field.
    Bool(BoolColumn),
}

impl KeyReader for SegmentKeys<'_> {
    fn key(&mut self, docid: u32) -> Result<Option<u64>, ReadError> {
        match self {
            Self::Numeric(keys) => keys.key(docid),
            Self::Bool(column) => Ok(column.get(docid).map(u64::from)),
        }
    }
}

/// A searchable set of documents with docids `0..doc_count()`: a segment, or the memtable.
pub trait SearchView {
    /// The norms reader of this view.
    type Norms<'a>: NormReader
    where
        Self: 'a;

    /// The posting-list iterator of this view.
    type Postings<'a>: PostingsIterator
    where
        Self: 'a;

    /// The term stream of this view.
    type Terms<'a, A>: TermEnum
    where
        Self: 'a,
        A: Automaton + 'a;

    /// The column key reader of this view.
    type Keys<'a>: KeyReader
    where
        Self: 'a;

    /// The keyword value reader of this view.
    type Keywords<'a>: KeywordReader
    where
        Self: 'a;

    /// Every docid is below this; deleted documents included (a memtable's replaced versions too), so not the
    /// BM25 document count, which the collection statistics give (ROADMAP 7.2.1).
    fn doc_count(&self) -> u32;

    /// Whether `docid` is a document a search may return: below [`Self::doc_count`] and not deleted. Postings
    /// already leave deleted documents out; this is for documents reached otherwise (a filter, a sort).
    fn is_live(&self, docid: u32) -> bool;

    /// The first document at or after `from` that [`Self::is_live`] accepts, `None` when there is none: what
    /// `match_all` and a `bool` without `must` or `should` walk. A search's interrupts read it as it scans
    /// (ROADMAP 7.6.3).
    ///
    /// # Errors
    /// None from a segment or the memtable; [`ReadError::Interrupted`] from a view a search wraps.
    fn next_live(&self, from: u32) -> Result<Option<u32>, ReadError> {
        Ok((from..self.doc_count()).find(|&d| self.is_live(d)))
    }

    /// The statistics of a `text` or `keyword` field (SPEC §5.1); `None` for another type or a field the source
    /// has no statistics for. A segment counts its deleted documents too; the memtable counts live ones, which is
    /// what the segment it flushes to will count.
    fn field_stats(&self, field: FieldId) -> Option<FieldStats>;

    /// The postings of `term` in a `text` or `keyword` field, deleted documents left out. `None` when the source
    /// has no such term; a segment whose tombstones delete every document of a term still gives an iterator, which
    /// yields none.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`] or [`ReadError::FieldType`] for a field that is not a `text` or `keyword` field
    /// of the schema; a corruption of the source.
    fn postings(
        &self,
        field: FieldId,
        term: &[u8],
    ) -> Result<Option<Self::Postings<'_>>, ReadError>;

    /// The terms of a `text` or `keyword` field between `lower` and `upper` (byte order) that `automaton` accepts
    /// ([`AlwaysMatch`] for every one), in byte order: a prefix, a range, a Levenshtein automaton (SPEC §7.6). A term
    /// is listed exactly when [`Self::postings`] gives it an iterator, so a segment lists a term whose documents are
    /// all deleted, and the memtable only terms with a live document in the view. So the `df`s are each view's own
    /// (a segment's count its deleted documents): rank expansions by the collection's statistics, not by these.
    ///
    /// The memtable copies the term bytes in range under its read lock, runs `automaton` on that copy, then takes
    /// the lock again to count each match's `df`. `automaton` is not called under the lock.
    ///
    /// # Errors
    /// As [`Self::postings`].
    fn terms<'a, A: Automaton + 'a>(
        &'a self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        automaton: A,
    ) -> Result<Self::Terms<'a, A>, ReadError>;

    /// The documents below [`Self::doc_count`] a search may not return: deleted, or replaced in the memtable.
    fn deleted(&self) -> RoaringBitmap;

    /// The documents that have a value in `field`, deleted ones included (see [`Self::deleted`]): a `text` field's
    /// with at least one token, a `keyword` field's with at least one value, a column's with a value.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`], or [`ReadError::FieldType`] for a `stored` field; a corruption of the source.
    fn present(&self, field: FieldId) -> Result<RoaringBitmap, ReadError>;

    /// The documents whose `bool` field is `true`, deleted ones included: with [`Self::present`], a `bool` filter's
    /// answer without a key per document.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`], or [`ReadError::FieldType`] for a field of another type; a corruption of the
    /// source.
    fn trues(&self, field: FieldId) -> Result<RoaringBitmap, ReadError>;

    /// Calls `f` with each document that has a value in an `i64`, `f64`, `datetime` or `bool` field, deleted ones
    /// included, and its key, in docid order: the order-preserving key of the value (SPEC §5.2.1; for `f64`,
    /// `-0.0` is never stored), 0 or 1 for a `bool`.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`], or [`ReadError::FieldType`] for a field of another type; a corruption of the
    /// source.
    fn column_keys(&self, field: FieldId, mut f: impl FnMut(u32, u64)) -> Result<(), ReadError> {
        self.try_column_keys(field, |docid, key| {
            f(docid, key);
            Ok(())
        })
    }

    /// As [`Self::column_keys`], stopping at the first error `f` returns.
    ///
    /// # Errors
    /// As [`Self::column_keys`]; what `f` returns.
    fn try_column_keys(
        &self,
        field: FieldId,
        f: impl FnMut(u32, u64) -> Result<(), ReadError>,
    ) -> Result<(), ReadError>;

    /// The norms of a `text` field (SPEC §5.3.4), for scoring each candidate document: a reader that keeps what it
    /// read (a segment's [`NormsCursor`]; the memtable's norms as of the view, copied once per view and field).
    ///
    /// # Errors
    /// As [`Self::postings`], for a field that is not `text`; a corruption of the source.
    fn norms(&self, field: FieldId) -> Result<Self::Norms<'_>, ReadError>;

    /// The keys of an `i64`, `f64`, `datetime` or `bool` field by docid, for sorting (ROADMAP 7.5.1); deleted
    /// documents keep theirs.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`], or [`ReadError::FieldType`] for a field of another type; a corruption of the
    /// source.
    fn keys(&self, field: FieldId) -> Result<Self::Keys<'_>, ReadError>;

    /// The values of a `keyword` field by docid, for sorting and facets (ROADMAP 7.5.1, 7.5.3); deleted documents
    /// keep theirs. The memtable copies the field's values as of the view once per call.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`], or [`ReadError::FieldType`] for a field of another type; a corruption of the
    /// source.
    fn keywords(&self, field: FieldId) -> Result<Self::Keywords<'_>, ReadError>;

    /// Document `docid` as stored: its id, blob reference and stored fields (SPEC §6.4.2).
    ///
    /// # Errors
    /// [`ReadError::DocOutOfRange`] for a docid at or above [`Self::doc_count`]; a corruption of the source.
    fn document(&self, docid: u32) -> Result<Document, ReadError>;

    /// The docid of the live document with id `id`, if there is one.
    ///
    /// # Errors
    /// A corruption of the source.
    fn docid_of(&self, id: &str) -> Result<Option<u32>, ReadError>;
}

impl PostingsIterator for Postings<'_> {
    fn df(&self) -> u32 {
        Postings::df(self)
    }

    fn doc(&self) -> Option<u32> {
        Postings::doc(self)
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        Postings::next(self)
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        Postings::advance(self, target)
    }

    fn freq(&mut self) -> Result<u32, ReadError> {
        Postings::freq(self)
    }

    fn positions(&mut self) -> Result<&[u32], ReadError> {
        Postings::positions(self)
    }

    fn block_max(&self) -> Option<BlockMax> {
        Postings::block_max(self)
    }

    fn shallow_advance(&mut self, target: u32) -> Result<Option<BlockMax>, ReadError> {
        Postings::shallow_advance(self, target)
    }

    fn term_bound(&self) -> FieldHit {
        FieldHit::from(self.term_info())
    }
}

/// A [`Segment`] with the tombstones of the snapshot that lists it (SPEC §8.3, §8.7), as a [`SearchView`].
///
/// ```
/// use persia_analysis::AnalyzerRegistry;
/// use persia_engine::document::{Document, Value};
/// use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder};
/// use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
/// use persia_engine::view::{PostingsIterator, SearchView, SegmentView};
/// use persia_format::ByteSource;
/// use roaring::RoaringBitmap;
///
/// let schema = SchemaBuilder::new("c").field("tag", FieldKind::Keyword(KeywordOptions::default())).build()?;
/// let tag = schema.field("tag").map(|f| f.id()).ok_or("no field")?;
/// let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
/// for id in ["a", "b", "c"] {
///     b.add(&Document::new(id).with(tag, Value::String("x".into())))?;
/// }
/// let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
/// let segment = Segment::open(ByteSource::new(SegmentWriter::new(&b, meta).finish()?.into()), &schema)?;
/// let tombstones = segment.tombstones(RoaringBitmap::from_iter([1]))?;
/// let view = SegmentView::new(&segment, Some(&tombstones));
/// let mut postings = view.postings(tag, b"x")?.ok_or("no term")?;
/// assert_eq!((postings.next()?, postings.next()?, postings.next()?), (Some(0), Some(2), None));
/// assert_eq!(view.docid_of("b")?, None);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy)]
pub struct SegmentView<'s> {
    segment: &'s Segment,
    tombstones: Option<&'s Tombstones>,
}

impl<'s> SegmentView<'s> {
    /// `segment` without the documents `tombstones` deletes; they must be `segment`'s ([`Segment::tombstones`]).
    pub fn new(segment: &'s Segment, tombstones: Option<&'s Tombstones>) -> Self {
        Self {
            segment,
            tombstones,
        }
    }

    /// The segment.
    pub fn segment(&self) -> &'s Segment {
        self.segment
    }
}

impl SearchView for SegmentView<'_> {
    type Norms<'a>
        = NormsCursor<'a>
    where
        Self: 'a;

    type Postings<'a>
        = Postings<'a>
    where
        Self: 'a;

    type Terms<'a, A>
        = TermStream<'a, A>
    where
        Self: 'a,
        A: Automaton + 'a;

    type Keys<'a>
        = SegmentKeys<'a>
    where
        Self: 'a;

    type Keywords<'a>
        = KeywordOrds<'a>
    where
        Self: 'a;

    fn doc_count(&self) -> u32 {
        self.segment.doc_count()
    }

    fn is_live(&self, docid: u32) -> bool {
        docid < self.segment.doc_count() && !self.tombstones.is_some_and(|t| t.contains(docid))
    }

    fn field_stats(&self, field: FieldId) -> Option<FieldStats> {
        self.segment.field_stats(field)
    }

    fn postings(&self, field: FieldId, term: &[u8]) -> Result<Option<Postings<'_>>, ReadError> {
        let mut terms = self.segment.terms(field)?;
        let Some(info) = terms.get(term)? else {
            return Ok(None);
        };
        let postings = terms.postings(&info)?;
        Ok(Some(match self.tombstones {
            Some(t) => postings.with_tombstones(t),
            None => postings,
        }))
    }

    fn terms<'a, A: Automaton + 'a>(
        &'a self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        automaton: A,
    ) -> Result<TermStream<'a, A>, ReadError> {
        Ok(self
            .segment
            .terms(field)?
            .search_range(automaton, lower, upper))
    }

    fn deleted(&self) -> RoaringBitmap {
        self.tombstones
            .map(|t| t.docids().clone())
            .unwrap_or_default()
    }

    fn present(&self, field: FieldId) -> Result<RoaringBitmap, ReadError> {
        self.segment.present(field)
    }

    fn trues(&self, field: FieldId) -> Result<RoaringBitmap, ReadError> {
        self.segment.trues(field)
    }

    fn try_column_keys(
        &self,
        field: FieldId,
        f: impl FnMut(u32, u64) -> Result<(), ReadError>,
    ) -> Result<(), ReadError> {
        self.segment.try_column_keys(field, f)
    }

    fn keys(&self, field: FieldId) -> Result<SegmentKeys<'_>, ReadError> {
        let is_bool = self
            .segment
            .schema()
            .field_by_id(field)
            .is_some_and(|f| f.field_type() == FieldType::Bool);
        Ok(if is_bool {
            SegmentKeys::Bool(self.segment.bool_column(field)?)
        } else {
            SegmentKeys::Numeric(self.segment.numeric_column(field)?.into_keys())
        })
    }

    fn keywords(&self, field: FieldId) -> Result<KeywordOrds<'_>, ReadError> {
        Ok(self.segment.keyword_column(field)?.into_ords())
    }

    fn norms(&self, field: FieldId) -> Result<NormsCursor<'_>, ReadError> {
        Ok(self.segment.norms(field)?.cursor())
    }

    fn document(&self, docid: u32) -> Result<Document, ReadError> {
        self.segment.document(docid)
    }

    fn docid_of(&self, id: &str) -> Result<Option<u32>, ReadError> {
        Ok(self
            .segment
            .docid_of(id)?
            .filter(|&docid| self.is_live(docid)))
    }
}
