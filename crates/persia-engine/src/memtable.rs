//! The memtable (SPEC §2, §8.2, ROADMAP 6.1): recent writes of one collection, indexed in memory and searchable
//! before they are flushed into a segment.
//!
//! One writer, any number of readers (SPEC §8.4). The writer, [`Memtable`], applies each operation under a short
//! write lock; a reader takes a [`MemtableView`], which names a point in the history (the documents and operations
//! applied when it was taken) and takes the read lock only while it copies out what a call needs. Everything the
//! memtable holds is appended, never moved or rewritten, but the version at which a document was replaced or deleted;
//! so a view sees exactly the writes before it, however many follow (MVCC, like a manifest snapshot), and a reader
//! never holds the lock across a search.
//!
//! Text is analyzed by the same code as [`crate::segment::SegmentBuilder`], so the memtable has the terms, positions
//! and norms the segment it flushes to will have.
//!
//! An upsert inside the memtable replaces in place (ROADMAP 6.1.3, SPEC §8.3): the new version gets the next docid
//! and the earlier one is marked dead, left out of every later view and of the flush. A delete marks the memtable's
//! version dead and is recorded even for an id the memtable does not have, so it can shadow older segments
//! ([`IdState::Deleted`]).
//!
//! ```
//! use persia_analysis::AnalyzerRegistry;
//! use persia_engine::document::{Document, Value};
//! use persia_engine::memtable::Memtable;
//! use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
//! use persia_engine::view::{PostingsIterator, SearchView};
//!
//! let schema = SchemaBuilder::new("notes")
//!     .field("body", FieldKind::Text(TextOptions::default()))
//!     .build()?;
//! let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
//! let mut memtable = Memtable::new(&schema, &AnalyzerRegistry::builtin())?;
//! memtable.put(&Document::new("n1").with(body, Value::String("hello world".into())))?;
//! let before = memtable.view();
//! memtable.put(&Document::new("n1").with(body, Value::String("hello again".into())))?;
//!
//! // The earlier view still sees the first version; a new one sees only the second.
//! assert!(before.postings(body, b"world")?.is_some());
//! let now = memtable.view();
//! assert!(now.postings(body, b"world")?.is_none());
//! let mut hello = now.postings(body, b"hello")?.ok_or("no term")?;
//! assert_eq!((hello.next()?, hello.next()?), (Some(1), None));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod flush;
mod view;

use std::convert::Infallible;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use persia_analysis::{Analyzer, AnalyzerRegistry, TokenStream};

pub use flush::{FlushPolicy, FlushReason};
pub use view::{IdState, MemtableNorms, MemtablePostings, MemtableView};

use crate::document::{Document, DocumentError, FieldSet, MAX_ID_LEN, Value};
use crate::schema::{CollectionSchema, FieldKind};
use crate::segment::arena::Arena;
use crate::segment::builder::inverted::{Inverted, Mark};
use crate::segment::builder::{index_keywords, index_text};
use crate::segment::{FieldStats, MAX_DOCS, SegmentError, norms};

/// No posting / no document.
const NONE: u32 = u32::MAX;
/// `killed_at` of a document no operation has replaced or deleted.
const ALIVE: u64 = u64::MAX;

/// Why the memtable rejected an operation. It is then as it was.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MemtableError {
    /// What a segment would reject: an invalid document or id, an unknown analyzer, a token over
    /// [`crate::segment::MAX_TERM_LEN`] bytes, or [`SegmentError::Full`] when the memtable has used every docid
    /// (replaced documents keep theirs until the flush).
    #[error(transparent)]
    Segment(#[from] SegmentError),
    /// An earlier operation panicked half-way; the memtable takes no more writes (its views still read the writes
    /// before it).
    #[error("the memtable is poisoned by a panic in an earlier write")]
    Poisoned,
}

/// Why a write-ahead logged operation ([`Memtable::put_logged`], [`Memtable::delete_logged`]) did not apply.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Logged<E> {
    /// The memtable rejected the operation before it was logged.
    #[error(transparent)]
    Rejected(MemtableError),
    /// Logging failed; the operation is not in the memtable.
    #[error("logging the operation failed: {0}")]
    Log(#[source] E),
}

/// A `put` that passed every check and is indexed, but not visible yet.
#[derive(Debug)]
struct StagedPut {
    docid: u32,
    version: u64,
    /// The id's arena id.
    id: u32,
    /// The `FieldSet::All` encoding.
    encoded: Vec<u8>,
    marks: Vec<Option<Mark>>,
}

/// A read handle on a memtable: takes [`MemtableView`]s of its latest writes, from any thread (SPEC §8.4). A
/// published snapshot holds one per collection, so a search sees the writes applied when it starts.
#[derive(Debug, Clone)]
pub struct MemtableReader {
    shared: Arc<RwLock<Inner>>,
}

impl MemtableReader {
    /// A view of every write applied so far.
    pub fn view(&self) -> MemtableView {
        MemtableView::new(Arc::clone(&self.shared), &read(&self.shared))
    }
}

/// The writer of a memtable: applies `put` and `delete` (SPEC §8.1) one at a time and hands out [`MemtableView`]s.
#[derive(Debug)]
pub struct Memtable {
    schema: Arc<CollectionSchema>,
    shared: Arc<RwLock<Inner>>,
    tokens: TokenStream,
    occurrences: Vec<(u32, u32)>,
    /// [`MAX_DOCS`], but in tests.
    max_docs: u32,
}

/// What a memtable holds; shared with its views.
#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) schema: Arc<CollectionSchema>,
    /// Indexed by field id.
    pub(crate) fields: Vec<MemField>,
    /// The `FieldSet::All` encoding of each document, by docid: what the flush indexes, and `get` decodes.
    pub(crate) docs: Vec<u8>,
    pub(crate) doc_ends: Vec<usize>,
    /// The version of the operation that replaced or deleted each document, by docid; [`ALIVE`] if none did.
    pub(crate) killed_at: Vec<u64>,
    /// Ids of every operation; an id's index is its arena id.
    pub(crate) ids: Arena,
    /// The version of the latest operation on each id.
    pub(crate) latest: Vec<u64>,
    /// Operation `v` (versions start at 1) is `ops[v - 1]`.
    pub(crate) ops: Vec<Op>,
    /// Statistics of the live documents, by field id; `None` for a field that is not `text` or `keyword`.
    pub(crate) stats: Vec<Option<FieldStats>>,
    pub(crate) live: u32,
}

/// One `put` or `delete`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Op {
    /// The document a `put` added; [`NONE`] for a `delete`.
    docid: u32,
    /// The version of the previous operation on the same id; 0 for none.
    prev: u64,
}

/// What a memtable holds for one field.
#[derive(Debug)]
pub(crate) enum MemField {
    Text {
        analyzer: Arc<Analyzer>,
        index: FieldIndex,
        /// The norm byte of each document (SPEC §5.3.4), by docid.
        norms: Vec<u8>,
    },
    Keyword {
        index: FieldIndex,
    },
    /// Columns and stored fields live in the document encodings only.
    Other,
}

/// The inverted index of one `text` or `keyword` field: the builder's posting log, and per term a chain of its
/// postings in docid order, so one term's list is found without scanning the others.
#[derive(Debug)]
pub(crate) struct FieldIndex {
    pub(crate) inverted: Inverted,
    /// First and last posting of each term id; [`NONE`] when it has none.
    pub(crate) heads: Vec<u32>,
    tails: Vec<u32>,
    /// The term's next posting after each posting.
    pub(crate) next: Vec<u32>,
}

impl FieldIndex {
    fn new(positions: bool) -> Self {
        Self {
            inverted: Inverted::new(positions),
            heads: Vec::new(),
            tails: Vec::new(),
            next: Vec::new(),
        }
    }

    /// Chains the postings from `from` on, the ones the last document added; their indices fit below [`NONE`]
    /// ([`Inner::check_postings`]).
    fn link(&mut self, from: usize) {
        let terms = self.inverted.terms.len();
        self.heads.resize(terms, NONE);
        self.tails.resize(terms, NONE);
        let start = u32::try_from(from).unwrap_or(NONE);
        let new = self.inverted.postings.get(from..).unwrap_or_default();
        for (i, posting) in (start..).zip(new) {
            let term = posting.term as usize;
            self.next.push(NONE);
            let slot = match self.tails.get(term) {
                Some(&NONE) => self.heads.get_mut(term),
                Some(&tail) => self.next.get_mut(tail as usize),
                None => None,
            };
            if let Some(slot) = slot {
                *slot = i;
            }
            if let Some(t) = self.tails.get_mut(term) {
                *t = i;
            }
        }
    }

    /// The field length of `docid`, 0 if it has no tokens or values in the field.
    fn length(&self, docid: u32) -> u32 {
        let lengths = &self.inverted.lengths;
        lengths
            .binary_search_by_key(&docid, |&(d, _)| d)
            .ok()
            .and_then(|i| lengths.get(i))
            .map_or(0, |&(_, len)| len)
    }

    fn size_in_bytes(&self) -> usize {
        let u32s = self
            .heads
            .capacity()
            .saturating_add(self.tails.capacity())
            .saturating_add(self.next.capacity());
        self.inverted
            .size_in_bytes()
            .saturating_add(u32s.saturating_mul(size_of::<u32>()))
    }
}

impl MemField {
    pub(crate) fn index(&self) -> Option<&FieldIndex> {
        match self {
            Self::Text { index, .. } | Self::Keyword { index } => Some(index),
            Self::Other => None,
        }
    }

    fn index_mut(&mut self) -> Option<&mut FieldIndex> {
        match self {
            Self::Text { index, .. } | Self::Keyword { index } => Some(index),
            Self::Other => None,
        }
    }
}

impl Memtable {
    /// An empty memtable for documents of `schema`, analyzing `text` fields with the analyzers `registry` has for
    /// them.
    ///
    /// # Errors
    /// [`SegmentError::UnknownAnalyzer`] if `registry` lacks the analyzer of a `text` field.
    pub fn new(
        schema: &CollectionSchema,
        registry: &AnalyzerRegistry,
    ) -> Result<Self, MemtableError> {
        let mut fields = Vec::with_capacity(schema.fields().len());
        let mut stats = Vec::with_capacity(schema.fields().len());
        for field in schema.fields() {
            let (memfield, stat) = match field.kind() {
                FieldKind::Text(options) => {
                    let analyzer = &options.analyzer;
                    let found = registry
                        .get(analyzer.name(), analyzer.version())
                        .ok_or_else(|| SegmentError::UnknownAnalyzer {
                            field: field.name().to_owned(),
                            analyzer: analyzer.to_string(),
                        })?;
                    let text = MemField::Text {
                        analyzer: Arc::clone(found),
                        index: FieldIndex::new(options.positions),
                        norms: Vec::new(),
                    };
                    (text, Some(FieldStats::default()))
                }
                FieldKind::Keyword(_) => (
                    MemField::Keyword {
                        index: FieldIndex::new(false),
                    },
                    Some(FieldStats::default()),
                ),
                _ => (MemField::Other, None),
            };
            fields.push(memfield);
            stats.push(stat);
        }
        let schema = Arc::new(schema.clone());
        let inner = Inner {
            schema: Arc::clone(&schema),
            fields,
            docs: Vec::new(),
            doc_ends: Vec::new(),
            killed_at: Vec::new(),
            ids: Arena::default(),
            latest: Vec::new(),
            ops: Vec::new(),
            stats,
            live: 0,
        };
        Ok(Self {
            schema,
            shared: Arc::new(RwLock::new(inner)),
            tokens: TokenStream::new(),
            occurrences: Vec::new(),
            max_docs: MAX_DOCS,
        })
    }

    /// Caps the docids, so tests reach [`SegmentError::Full`].
    #[cfg(test)]
    pub(crate) fn with_max_docs(mut self, max_docs: u32) -> Self {
        self.max_docs = max_docs;
        self
    }

    /// The schema documents are indexed with.
    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// A read handle that takes views of this memtable's latest writes.
    pub fn reader(&self) -> MemtableReader {
        MemtableReader {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Calls `f` with the `FieldSet::All` encoding of each live document, in docid order: what a flush writes
    /// (ROADMAP 6.3.1). Holds the read lock throughout, so views can still be taken; `&self` keeps writes out.
    pub(crate) fn for_each_live<E>(
        &self,
        mut f: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        let inner = read(&self.shared);
        let mut start = 0;
        for (&end, &killed) in inner.doc_ends.iter().zip(&inner.killed_at) {
            if killed == ALIVE
                && let Some(bytes) = inner.docs.get(start..end)
            {
                f(bytes)?;
            }
            start = end;
        }
        Ok(())
    }

    /// Adds `doc` and returns its docid; an earlier version of the same id in the memtable is replaced (SPEC §8.3).
    ///
    /// # Errors
    /// [`MemtableError::Segment`] for what a [`crate::segment::SegmentBuilder`] would reject (an invalid document,
    /// a token over [`crate::segment::MAX_TERM_LEN`] bytes, an overflow) and [`SegmentError::Full`] when every docid
    /// is used; [`MemtableError::Poisoned`]. The memtable is then as it was (only spare capacity may have grown).
    pub fn put(&mut self, doc: &Document) -> Result<u32, MemtableError> {
        self.put_logged(doc, |_| Ok::<(), Infallible>(()))
            .map_err(|e| match e {
                Logged::Rejected(e) => e,
                Logged::Log(never) => match never {},
            })
    }

    /// [`Memtable::put`] for a write-ahead logged operation (SPEC §8.2.1 "What may be logged", ROADMAP 6.3.9): checks
    /// and indexes `doc` without making it visible, passes its `FieldSet::All` encoding (the WAL record's payload) to
    /// `log`, and makes it visible only if `log` succeeds. So a document the memtable rejects is never logged, and no
    /// view sees one that was not. The write lock is not held while `log` runs.
    ///
    /// # Errors
    /// [`Logged::Rejected`] as [`Memtable::put`], before `log` is called; [`Logged::Log`] with `log`'s error, after
    /// which the document is not in the memtable (its id may stay interned, unseen by any view).
    pub fn put_logged<E>(
        &mut self,
        doc: &Document,
        log: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Result<u32, Logged<E>> {
        let staged = self.stage_put(doc).map_err(Logged::Rejected)?;
        if let Err(e) = log(&staged.encoded) {
            write(&self.shared)
                .map_err(Logged::Rejected)?
                .rollback(&staged.marks);
            return Err(Logged::Log(e));
        }
        let mut inner = write(&self.shared).map_err(Logged::Rejected)?;
        // Nothing fails from here on. `&mut self` kept every other write out since the stage, so the docid and the
        // version are still the next ones.
        inner.link(staged.docid, &staged.marks);
        inner.docs.extend_from_slice(&staged.encoded);
        let end = inner.docs.len();
        inner.doc_ends.push(end);
        inner.killed_at.push(ALIVE);
        inner.live = inner.live.saturating_add(1);
        inner.record(staged.id, staged.docid, staged.version);
        Ok(staged.docid)
    }

    /// Everything of a `put` that can fail: validates, indexes `doc` (its postings stay unchained, so no view sees
    /// them) and interns its id.
    fn stage_put(&mut self, doc: &Document) -> Result<StagedPut, MemtableError> {
        let mut inner = write(&self.shared)?;
        let docid = u32::try_from(inner.killed_at.len())
            .ok()
            .filter(|&d| d < self.max_docs)
            .ok_or(SegmentError::Full {
                max_docs: self.max_docs,
            })?;
        let version = inner.next_version()?;
        // Validates the whole document before anything changes.
        let encoded = doc
            .encode(&inner.schema, FieldSet::All)
            .map_err(SegmentError::from)?;
        let marks = inner.marks();
        // The id is interned last: undoing it would rehash every id (`Arena::truncate`).
        let result = inner
            .index(docid, doc, &mut self.tokens, &mut self.occurrences)
            .and_then(|()| inner.check_postings())
            .and_then(|()| inner.intern(doc.id()));
        match result {
            Ok(id) => Ok(StagedPut {
                docid,
                version,
                id,
                encoded,
                marks,
            }),
            Err(e) => {
                inner.rollback(&marks);
                Err(e.into())
            }
        }
    }

    /// Deletes the document with id `id` (SPEC §8.1): the memtable's version, if any, is left out from now on, and
    /// the delete is recorded to shadow older segments. Returns whether the memtable had a live version.
    ///
    /// # Errors
    /// [`DocumentError::IdLen`] (as [`MemtableError::Segment`]) for an id that is empty or longer than
    /// [`MAX_ID_LEN`] bytes; [`SegmentError::TooLarge`] past `u32::MAX` ids or operations;
    /// [`MemtableError::Poisoned`].
    pub fn delete(&mut self, id: &str) -> Result<bool, MemtableError> {
        self.delete_logged(id, || Ok::<(), Infallible>(()))
            .map_err(|e| match e {
                Logged::Rejected(e) => e,
                Logged::Log(never) => match never {},
            })
    }

    /// [`Memtable::delete`] for a write-ahead logged operation, as [`Memtable::put_logged`]: `log` runs once the
    /// delete is known to apply, and the delete is visible only if it succeeds.
    ///
    /// # Errors
    /// [`Logged::Rejected`] as [`Memtable::delete`], before `log` is called; [`Logged::Log`] with `log`'s error.
    pub fn delete_logged<E>(
        &mut self,
        id: &str,
        log: impl FnOnce() -> Result<(), E>,
    ) -> Result<bool, Logged<E>> {
        if !(1..=MAX_ID_LEN).contains(&id.len()) {
            let e = SegmentError::Document(DocumentError::IdLen { len: id.len() });
            return Err(Logged::Rejected(e.into()));
        }
        let (version, id) = {
            let mut inner = write(&self.shared).map_err(Logged::Rejected)?;
            let version = inner
                .next_version()
                .map_err(|e| Logged::Rejected(e.into()))?;
            let id = inner.intern(id).map_err(|e| Logged::Rejected(e.into()))?;
            (version, id)
        };
        log().map_err(Logged::Log)?;
        let mut inner = write(&self.shared).map_err(Logged::Rejected)?;
        Ok(inner.record(id, NONE, version))
    }

    /// A view of every write applied so far.
    pub fn view(&self) -> MemtableView {
        MemtableView::new(Arc::clone(&self.shared), &read(&self.shared))
    }

    /// Live documents: added and not replaced or deleted since.
    pub fn live_count(&self) -> u32 {
        read(&self.shared).live
    }

    /// Operations applied (each `put` and `delete`); the version of the latest one.
    pub fn version(&self) -> u64 {
        read(&self.shared).version()
    }

    /// Whether no operation was applied: there is nothing to flush.
    pub fn is_empty(&self) -> bool {
        self.version() == 0
    }

    /// Heap bytes the memtable holds, for the flush threshold (ROADMAP 6.1.4): capacities, as
    /// [`crate::segment::SegmentBuilder::size_in_bytes`], the analysis scratch buffers included; the schema and
    /// analyzers excluded.
    pub fn size_in_bytes(&self) -> usize {
        let scratch = self.tokens.size_in_bytes().saturating_add(
            self.occurrences
                .capacity()
                .saturating_mul(size_of::<(u32, u32)>()),
        );
        read(&self.shared).size_in_bytes().saturating_add(scratch)
    }
}

/// The read lock, poisoned or not. A view taken before a write that panicked never sees any of it: it stops at the
/// documents and versions recorded when it was taken. One taken after may see that write half applied; the writer
/// then refuses every write ([`MemtableError::Poisoned`]) and the memtable is to be rebuilt from the WAL.
pub(crate) fn read(lock: &RwLock<Inner>) -> RwLockReadGuard<'_, Inner> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write(lock: &RwLock<Inner>) -> Result<RwLockWriteGuard<'_, Inner>, MemtableError> {
    lock.write().map_err(|_| MemtableError::Poisoned)
}

impl Inner {
    pub(crate) fn version(&self) -> u64 {
        u64::try_from(self.ops.len()).unwrap_or(u64::MAX)
    }

    pub(crate) fn doc_count(&self) -> u32 {
        // At most MAX_DOCS documents: `put` checks.
        u32::try_from(self.killed_at.len()).unwrap_or(u32::MAX)
    }

    fn next_version(&self) -> Result<u64, SegmentError> {
        // Op `v` is found as `ops[v - 1]` through `u32` arena ids; more than `u32::MAX` would not fit `prev` chains
        // anyway long before memory runs out.
        u32::try_from(self.ops.len())
            .ok()
            .and_then(|n| n.checked_add(1))
            .map(u64::from)
            .ok_or(SegmentError::TooLarge { what: "operations" })
    }

    fn intern(&mut self, id: &str) -> Result<u32, SegmentError> {
        let (id, new) = self
            .ids
            .intern(id.as_bytes())
            .map_err(|_| SegmentError::TooLarge {
                what: "document ids",
            })?;
        if new {
            self.latest.push(0);
        }
        Ok(id)
    }

    /// Records operation `version` on id `id`, which added `docid` ([`NONE`] for a delete), replacing the id's
    /// previous document; returns whether there was a live one. Last, so a view never sees half an operation.
    fn record(&mut self, id: u32, docid: u32, version: u64) -> bool {
        let prev = self.latest.get(id as usize).copied().unwrap_or(0);
        let previous = prev
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| self.ops.get(i))
            .map(|op| op.docid)
            .filter(|&d| d != NONE);
        let replaced = previous.is_some_and(|d| self.kill(d, version));
        self.ops.push(Op { docid, prev });
        if let Some(l) = self.latest.get_mut(id as usize) {
            *l = version;
        }
        replaced
    }

    /// Marks `docid` dead from `version` on and takes it out of the live statistics; false if it already was.
    fn kill(&mut self, docid: u32, version: u64) -> bool {
        let Some(k) = self
            .killed_at
            .get_mut(docid as usize)
            .filter(|k| **k == ALIVE)
        else {
            return false;
        };
        *k = version;
        self.live = self.live.saturating_sub(1);
        for (field, stats) in self.fields.iter().zip(&mut self.stats) {
            let (Some(index), Some(stats)) = (field.index(), stats) else {
                continue;
            };
            let len = index.length(docid);
            if len > 0 {
                stats.docs_with_field = stats.docs_with_field.saturating_sub(1);
                stats.sum_field_len = stats.sum_field_len.saturating_sub(u64::from(len));
            }
        }
        true
    }

    fn marks(&self) -> Vec<Option<Mark>> {
        self.fields
            .iter()
            .map(|f| f.index().map(|i| i.inverted.mark()))
            .collect()
    }

    /// Undoes a document that failed before [`Self::link`] and before its id was interned: its postings were never
    /// chained.
    fn rollback(&mut self, marks: &[Option<Mark>]) {
        for (field, mark) in self.fields.iter_mut().zip(marks) {
            if let (Some(index), Some(mark)) = (field.index_mut(), mark) {
                index.inverted.rollback(mark);
            }
        }
    }

    /// Indexes the `text` and `keyword` values of `doc` as document `docid`.
    fn index(
        &mut self,
        docid: u32,
        doc: &Document,
        tokens: &mut TokenStream,
        occurrences: &mut Vec<(u32, u32)>,
    ) -> Result<(), SegmentError> {
        for (field, value) in doc.fields() {
            let Some(memfield) = usize::try_from(field.get())
                .ok()
                .and_then(|i| self.fields.get_mut(i))
            else {
                continue;
            };
            match (memfield, value) {
                (
                    MemField::Text {
                        analyzer, index, ..
                    },
                    Value::String(s),
                ) => index_text(
                    analyzer,
                    (field, &mut index.inverted),
                    docid,
                    [s.as_str()],
                    u32::MAX,
                    tokens,
                    occurrences,
                )?,
                (
                    MemField::Text {
                        analyzer, index, ..
                    },
                    Value::Strings(list),
                ) => index_text(
                    analyzer,
                    (field, &mut index.inverted),
                    docid,
                    list.iter().map(String::as_str),
                    u32::MAX,
                    tokens,
                    occurrences,
                )?,
                (MemField::Keyword { index }, Value::String(s)) => {
                    index_keywords(&mut index.inverted, docid, [s.as_str()], occurrences)?;
                }
                (MemField::Keyword { index }, Value::Strings(list)) => index_keywords(
                    &mut index.inverted,
                    docid,
                    list.iter().map(String::as_str),
                    occurrences,
                )?,
                // Validation rules out any other pairing for these fields; other fields have no index.
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether every field's postings, those of the document just indexed included, have an index below [`NONE`].
    fn check_postings(&self) -> Result<(), SegmentError> {
        let fits = self
            .fields
            .iter()
            .filter_map(MemField::index)
            .all(|i| u32::try_from(i.inverted.postings.len()).is_ok_and(|n| n < NONE));
        if fits {
            Ok(())
        } else {
            Err(SegmentError::TooLarge { what: "postings" })
        }
    }

    /// Chains the new postings of document `docid`, records its norms and adds it to the live statistics.
    fn link(&mut self, docid: u32, marks: &[Option<Mark>]) {
        for ((field, mark), stats) in self.fields.iter_mut().zip(marks).zip(&mut self.stats) {
            let Some(mark) = mark else {
                continue;
            };
            let len = match field {
                MemField::Text { index, norms, .. } => {
                    index.link(mark.postings());
                    let len = index.length(docid);
                    norms.push(norms::encode(len));
                    len
                }
                MemField::Keyword { index } => {
                    index.link(mark.postings());
                    index.length(docid)
                }
                MemField::Other => continue,
            };
            if let Some(stats) = stats
                && len > 0
            {
                stats.docs_with_field = stats.docs_with_field.saturating_add(1);
                stats.sum_field_len = stats.sum_field_len.saturating_add(u64::from(len));
            }
        }
    }

    fn size_in_bytes(&self) -> usize {
        let fields = self.fields.iter().fold(0_usize, |sum, f| {
            let bytes = match f {
                MemField::Text { index, norms, .. } => {
                    index.size_in_bytes().saturating_add(norms.capacity())
                }
                MemField::Keyword { index } => index.size_in_bytes(),
                MemField::Other => 0,
            };
            sum.saturating_add(bytes)
        });
        fields
            .saturating_add(self.docs.capacity())
            .saturating_add(self.doc_ends.capacity().saturating_mul(size_of::<usize>()))
            .saturating_add(self.killed_at.capacity().saturating_mul(size_of::<u64>()))
            .saturating_add(self.ids.size_in_bytes())
            .saturating_add(self.latest.capacity().saturating_mul(size_of::<u64>()))
            .saturating_add(self.ops.capacity().saturating_mul(size_of::<Op>()))
    }
}

#[cfg(test)]
mod tests;
