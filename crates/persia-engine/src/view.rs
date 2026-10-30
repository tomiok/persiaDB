//! What a search runs over, whatever holds the documents (ROADMAP 6.1.2): a [`SearchView`] gives a field's
//! postings as a [`PostingsIterator`], norms, statistics and stored documents. A [`SegmentView`] (a segment and
//! its tombstones, SPEC §8.3) and a [`crate::memtable::MemtableView`] (SPEC §8.2) both implement it, so the query
//! engine treats the memtable as one more segment (ROADMAP 7.4.6).
//!
//! Both leave out deleted documents: a view's postings, [`SearchView::is_live`] and [`SearchView::docid_of`] only
//! see the documents a search may return.

use crate::document::Document;
use crate::schema::FieldId;
use crate::score::FieldHit;
use crate::segment::{BlockMax, FieldStats, NormsCursor, Postings, ReadError, Segment, Tombstones};

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

    /// Every docid is below this; deleted documents included (a memtable's replaced versions too), so not the
    /// BM25 document count, which the collection statistics give (ROADMAP 7.2.1).
    fn doc_count(&self) -> u32;

    /// Whether `docid` is a document a search may return: below [`Self::doc_count`] and not deleted. Postings
    /// already leave deleted documents out; this is for documents reached otherwise (a filter, a sort).
    fn is_live(&self, docid: u32) -> bool;

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

    /// The norms of a `text` field (SPEC §5.3.4), for scoring each candidate document: a reader that keeps what it
    /// read (a segment's [`NormsCursor`]; the memtable's norms as of the view, copied once per view and field).
    ///
    /// # Errors
    /// As [`Self::postings`], for a field that is not `text`; a corruption of the source.
    fn norms(&self, field: FieldId) -> Result<Self::Norms<'_>, ReadError>;

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
