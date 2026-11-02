//! A point-in-time view of a memtable, searchable like a segment (ROADMAP 6.1.2).

use std::sync::{Arc, OnceLock, RwLock};

use core::ops::{Bound, RangeBounds};

use fst::Automaton;
use roaring::RoaringBitmap;

use super::columns::{keywords, posting_chunk, term_bytes, used_terms};
use super::{FieldIndex, Inner, MemField, MemtableKeys, MemtableKeywords, NONE, read};
use crate::bitmap::Ascending;
use crate::document::Document;
use crate::schema::{CollectionSchema, FieldId, FieldType};
use crate::score::FieldHit;
use crate::segment::builder::inverted::Inverted;
use crate::segment::{BlockMax, FieldStats, ReadError};
use crate::view::{NormReader, PostingsIterator, SearchView, TermEnum};

/// How many items (terms, column keys, kill versions, lengths, postings, norms) a view copies under one read lock: a
/// copy of a large memtable is taken in chunks, so a writer waits for one chunk, not the whole copy, and a search's
/// interrupts are read between them (ROADMAP 7.6.5, 7.6.6).
const COPY_CHUNK: usize = 1024;

/// What the memtable knew of an id when a view was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdState {
    /// Its latest operation was a `put`: the live document has this docid, and older segments' versions of the id
    /// are hidden (an upsert, SPEC §8.3).
    Live(u32),
    /// Its latest operation was a `delete`: no version of it is live, in the memtable or in older segments
    /// (SPEC §8.3).
    Deleted,
}

/// The writes a [`super::Memtable`] had applied when the view was taken, and none after (SPEC §8.4).
///
/// Cheap to clone and `Send + Sync`; it keeps the memtable's memory alive, flushed or not. A call takes the read
/// lock while it copies out what it needs (a term's postings, a document), never across calls.
#[derive(Debug, Clone)]
pub struct MemtableView {
    shared: Arc<RwLock<Inner>>,
    schema: Arc<CollectionSchema>,
    /// Documents `0..docs` existed.
    docs: u32,
    /// Operations `1..=version` were applied.
    version: u64,
    /// Live statistics by field id, as of `version`.
    stats: Vec<Option<FieldStats>>,
    live: u32,
    /// Norms by field id, copied out on first use.
    norms: Vec<OnceLock<Arc<[u8]>>>,
}

impl MemtableView {
    pub(super) fn new(shared: Arc<RwLock<Inner>>, inner: &Inner) -> Self {
        let (docs, version, stats, live) = match &inner.held {
            Some(held) => (held.docs, held.version, held.stats.clone(), held.live),
            None => (
                inner.doc_count(),
                inner.version(),
                inner.stats.clone(),
                inner.live,
            ),
        };
        Self {
            docs,
            version,
            stats,
            live,
            norms: inner.fields.iter().map(|_| OnceLock::new()).collect(),
            schema: Arc::clone(&inner.schema),
            shared,
        }
    }

    /// The operations applied before the view: the memtable's [`super::Memtable::version`] then.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Live documents in the view.
    pub fn live_count(&self) -> u32 {
        self.live
    }

    /// The state of id `id` in the view; `None` if no operation before it named the id.
    pub fn lookup(&self, id: &str) -> Option<IdState> {
        let inner = read(&self.shared);
        let index = inner.ids.get(id.as_bytes())?;
        let mut version = inner.latest.get(index as usize).copied()?;
        // Walk back past the operations after the view.
        loop {
            let op = version
                .checked_sub(1)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| inner.ops.get(i))?;
            if version <= self.version {
                return Some(match op.docid {
                    NONE => IdState::Deleted,
                    docid => IdState::Live(docid),
                });
            }
            version = op.prev;
        }
    }

    /// The ids whose first operation, put or delete, is one of `after + 1..=` [`Self::version`], each with that
    /// operation's version, in operation order; none when `after` is not below the version. An id hides its older
    /// segment copies from its first operation on (SPEC §8.3, ROADMAP 7.4.6), so these are what this view hides beyond
    /// what a view at version `after` does, and an id upserted many times is listed once.
    pub fn ids_first_named_after(&self, after: u64) -> Vec<(u64, String)> {
        let inner = read(&self.shared);
        let ops = usize::try_from(after)
            .ok()
            .zip(usize::try_from(self.version).ok())
            .and_then(|(from, to)| inner.ops.get(from..to))
            .unwrap_or_default();
        (after.saturating_add(1)..)
            .zip(ops)
            // No earlier operation on the id: its first.
            .filter(|(_, op)| op.prev == 0)
            // Ids are interned from `&str` only (`put` and `delete`), so every key is UTF-8.
            .filter_map(|(version, op)| {
                Some((
                    version,
                    std::str::from_utf8(inner.ids.key(op.id)).ok()?.to_owned(),
                ))
            })
            .collect()
    }

    /// Whether `other` sees what this view sees: a view of the same memtable at the same cutoff, so its counts are
    /// this view's.
    pub(crate) fn is_same_view(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
            && self.version == other.version
            && self.docs == other.docs
    }

    /// Whether this is a view of the memtable `reader` reads.
    pub(crate) fn is_of(&self, reader: &super::MemtableReader) -> bool {
        Arc::ptr_eq(&self.shared, reader.shared())
    }

    /// The live documents of the view holding `term` in a `text` or `keyword` field: the `df` of
    /// [`SearchView::postings`], counted without copying the postings (SPEC §7.2).
    ///
    /// The chain is walked under the read lock on every call, and the count is not cached. It is this view's
    /// `(docs, version)` cutoff, not the memtable's current live set, so a counter stored on the term would be
    /// wrong for an older view. A cache keyed by that cutoff would grow with every version and every term, and a
    /// search asks each term once, when it builds a scorer (ROADMAP 6.1.7).
    ///
    /// # Errors
    /// As [`SearchView::postings`].
    pub fn df(&self, field: FieldId, term: &[u8]) -> Result<u32, ReadError> {
        let inner = read(&self.shared);
        let Some(index) = indexed(&inner, field, false, "terms")?.index() else {
            return Ok(0);
        };
        Ok(index
            .inverted
            .terms
            .get(term)
            .map_or(0, |t| self.live_df(&inner, index, t)))
    }

    /// The live documents of the view on the chain of term `term` (its id in `index`).
    fn live_df(&self, inner: &Inner, index: &FieldIndex, term: u32) -> u32 {
        // A term interned by a document not linked yet has no chain.
        let mut at = index.heads.get(term as usize).copied().unwrap_or(NONE);
        let mut df = 0_u32;
        while let Some(p) = index.inverted.postings.get(at as usize) {
            // A chain is in docid order: the rest came after the view.
            if p.doc >= self.docs {
                break;
            }
            if self.visible(inner, p.doc) {
                df = df.saturating_add(1);
            }
            at = index.next.get(at as usize).copied().unwrap_or(NONE);
        }
        df
    }

    fn visible(&self, inner: &Inner, docid: u32) -> bool {
        docid < self.docs
            && inner
                .killed_at
                .get(docid as usize)
                .is_some_and(|&k| k > self.version)
    }

    /// Copies items out of the memtable [`COPY_CHUNK`] at a time, each chunk under its own read lock:
    /// `copy(inner, from)` returns the items from index `from` on, at most [`COPY_CHUNK`] and fewer only at the end
    /// of what the view sees. `each`, and `step` once per item, run without the lock, so a writer waits for one
    /// chunk and a search's interrupts stop a copy of a large memtable (ROADMAP 7.6.5, 7.6.6). What a view sees of a
    /// field is a prefix that later writes do not change, so the chunks hold what one copy under one lock would.
    fn copy_chunked<T>(
        &self,
        step: &dyn Fn() -> Result<(), ReadError>,
        mut copy: impl FnMut(&Inner, usize) -> Result<Vec<T>, ReadError>,
        mut each: impl FnMut(T) -> Result<(), ReadError>,
    ) -> Result<(), ReadError> {
        let mut from = 0_usize;
        loop {
            let chunk = copy(&read(&self.shared), from)?;
            let n = chunk.len();
            for item in chunk {
                step()?;
                each(item)?;
            }
            if n < COPY_CHUNK {
                return Ok(());
            }
            from = from.saturating_add(n);
        }
    }

    /// The keys of column `field` from index `from` in the view, at most [`COPY_CHUNK`] ([`Self::copy_chunked`]).
    fn column_chunk(
        &self,
        inner: &Inner,
        field: FieldId,
        from: usize,
    ) -> Result<Vec<(u32, u64)>, ReadError> {
        match field_of(inner, field)? {
            (_, MemField::Column { keys }) => Ok(key_chunk(keys, from, self.docs)),
            (field_type, _) => Err(ReadError::FieldType {
                field,
                field_type,
                what: "column",
            }),
        }
    }
}

/// `items[from..]`, at most [`COPY_CHUNK`] of them, up to the first `in_view` rejects: a view's part of an
/// append-only, docid-ordered list.
fn prefix_chunk<T: Copy>(items: &[T], from: usize, in_view: impl Fn(&T) -> bool) -> Vec<T> {
    items
        .get(from..)
        .unwrap_or_default()
        .iter()
        .take(COPY_CHUNK)
        .take_while(|item| in_view(item))
        .copied()
        .collect()
}

/// The keys of a column from index `from` whose documents are below `docs` (keys are appended in docid order).
fn key_chunk(keys: &[(u32, u64)], from: usize, docs: u32) -> Vec<(u32, u64)> {
    prefix_chunk(keys, from, |&(docid, _)| docid < docs)
}

/// The field `field` of `inner`'s schema, and its type.
pub(super) fn field_of(inner: &Inner, field: FieldId) -> Result<(FieldType, &MemField), ReadError> {
    let f = inner
        .schema
        .field_by_id(field)
        .ok_or(ReadError::UnknownField { field })?;
    usize::try_from(field.get())
        .ok()
        .and_then(|i| inner.fields.get(i))
        .map(|m| (f.field_type(), m))
        .ok_or(ReadError::UnknownField { field })
}

/// Whether `automaton` accepts `term`, as an FST search walks it: byte by byte from its start state.
fn accepts<A: Automaton>(automaton: &A, term: &[u8]) -> bool {
    let mut state = automaton.start();
    for &b in term {
        if !automaton.can_match(&state) {
            return false;
        }
        state = automaton.accept(&state, b);
    }
    automaton.is_match(&state)
}

/// The field `field` of `inner`'s schema, which must be a `text` or `keyword` field (`text` only with `text_only`).
fn indexed<'i>(
    inner: &'i Inner,
    field: FieldId,
    text_only: bool,
    what: &'static str,
) -> Result<&'i MemField, ReadError> {
    let f = inner
        .schema
        .field_by_id(field)
        .ok_or(ReadError::UnknownField { field })?;
    let ok = match f.field_type() {
        FieldType::Text => true,
        FieldType::Keyword => !text_only,
        _ => false,
    };
    if !ok {
        return Err(ReadError::FieldType {
            field,
            field_type: f.field_type(),
            what,
        });
    }
    usize::try_from(field.get())
        .ok()
        .and_then(|i| inner.fields.get(i))
        .ok_or(ReadError::UnknownField { field })
}

impl SearchView for MemtableView {
    type Norms<'a> = MemtableNorms;

    type Keys<'a> = MemtableKeys;

    type Keywords<'a> = MemtableKeywords;

    type Postings<'a>
        = MemtablePostings<'a>
    where
        Self: 'a;

    type Terms<'a, A>
        = MemtableTerms
    where
        Self: 'a,
        A: Automaton + 'a;

    fn doc_count(&self) -> u32 {
        self.docs
    }

    fn as_memtable(&self) -> Option<&MemtableView> {
        Some(self)
    }

    fn is_live(&self, docid: u32) -> bool {
        self.visible(&read(&self.shared), docid)
    }

    fn field_stats(&self, field: FieldId) -> Option<FieldStats> {
        usize::try_from(field.get())
            .ok()
            .and_then(|i| self.stats.get(i).copied().flatten())
    }

    fn postings(
        &self,
        field: FieldId,
        term: &[u8],
    ) -> Result<Option<MemtablePostings<'_>>, ReadError> {
        let inner = read(&self.shared);
        let memfield = indexed(&inner, field, false, "terms")?;
        let (index, norms) = match memfield {
            MemField::Text { index, norms, .. } => (index, Some(norms)),
            MemField::Keyword { index } => (index, None),
            MemField::Column { .. } | MemField::Other => return Ok(None),
        };
        let Some(mut at) = index
            .inverted
            .terms
            .get(term)
            .and_then(|t| index.heads.get(t as usize).copied())
        else {
            return Ok(None);
        };
        // Copy this term's chain under the read lock. The caller iterates the copy; the guard ends with this call.
        let mut entries = Vec::new();
        let (mut max_tf, mut min_norm) = (0_u32, u8::MAX);
        while let Some(p) = index.inverted.postings.get(at as usize) {
            // A chain is in docid order: the rest came after the view.
            if p.doc >= self.docs {
                break;
            }
            if self.visible(&inner, p.doc) {
                entries.push(Entry {
                    doc: p.doc,
                    tf: p.tf,
                    pos_start: p.pos_start,
                });
                max_tf = max_tf.max(p.tf);
                let norm = norms.and_then(|n| n.get(p.doc as usize).copied());
                min_norm = min_norm.min(norm.unwrap_or(0));
            }
            at = index.next.get(at as usize).copied().unwrap_or(NONE);
        }
        let Some(last) = entries.last() else {
            return Ok(None);
        };
        let block = BlockMax {
            last_docid: last.doc,
            max_tf,
            min_norm,
        };
        let has_positions = index.inverted.positions.is_some();
        drop(inner);
        Ok(Some(MemtablePostings {
            view: self,
            field,
            has_positions,
            entries,
            cur: None,
            block,
            positions: Vec::new(),
        }))
    }

    fn terms<'a, A: Automaton + 'a>(
        &'a self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        automaton: A,
    ) -> Result<MemtableTerms, ReadError> {
        self.terms_stepped(field, lower, upper, automaton, &|| Ok(()))
    }

    fn terms_stepped<'a, A: Automaton + 'a>(
        &'a self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        automaton: A,
        step: &dyn Fn() -> Result<(), ReadError>,
    ) -> Result<MemtableTerms, ReadError> {
        // Copy the terms in range [`COPY_CHUNK`] at a time, each chunk under its own read lock, and run `automaton`
        // and `step` without the lock. `df` is counted afterward, still at this view's `(docs, version)` cutoff, so
        // a write between two locks cannot add or hide a document the view should see. A term with a document in
        // the view was interned before it and keeps its id (a failed `put` truncates only the ids it interned), so
        // the ids below the arena's length at the first lock hold every such term; the ones interned later are not
        // read, however many writers add.
        let mut matched: Vec<Vec<u8>> = Vec::new();
        let mut limit = None;
        let mut from = 0_usize;
        loop {
            let mut in_range = Vec::new();
            let end = {
                let inner = read(&self.shared);
                let Some(index) = indexed(&inner, field, false, "terms")?.index() else {
                    break;
                };
                // The term arena is a hash table: every term is tried, then the matches are sorted.
                let arena = &index.inverted.terms;
                let limit = *limit.get_or_insert(arena.len());
                let end = arena.len().min(limit).min(from.saturating_add(COPY_CHUNK));
                for id in (0..=u32::MAX).skip(from).take(end.saturating_sub(from)) {
                    let term = arena.key(id);
                    if RangeBounds::<[u8]>::contains(&(lower, upper), term) {
                        in_range.push(term.to_vec());
                    }
                }
                end
            };
            for _ in from..end {
                step()?;
            }
            matched.extend(
                in_range
                    .into_iter()
                    .filter(|term| accepts(&automaton, term)),
            );
            if end <= from {
                break;
            }
            from = end;
        }
        // A truncated id interned again may list a term twice.
        matched.sort_unstable();
        matched.dedup();
        // Consume each chunk by value and move a matched term into `terms`. Cloning it would allocate again
        // per term. A step still runs once per term of the chunk, outside the lock; a missing index stops
        // before that step, as a borrowed chunk did.
        let mut terms = Vec::new();
        while !matched.is_empty() {
            let n = COPY_CHUNK.min(matched.len());
            let chunk: Vec<Vec<u8>> = matched.drain(..n).collect();
            {
                let inner = read(&self.shared);
                let Some(index) = indexed(&inner, field, false, "terms")?.index() else {
                    break;
                };
                for term in chunk {
                    let Some(id) = index.inverted.terms.get(&term) else {
                        continue;
                    };
                    let df = self.live_df(&inner, index, id);
                    if df > 0 {
                        terms.push((term, df));
                    }
                }
            }
            for _ in 0..n {
                step()?;
            }
        }
        Ok(MemtableTerms { terms, next: 0 })
    }

    fn deleted(&self) -> Result<RoaringBitmap, ReadError> {
        self.deleted_stepped(&|| Ok(()))
    }

    fn deleted_stepped(
        &self,
        step: &dyn Fn() -> Result<(), ReadError>,
    ) -> Result<RoaringBitmap, ReadError> {
        // Same rule as `visible`: a document is live when its killing operation is after the view. A kill version
        // only changes from `ALIVE` to an operation's, which is after the view too, so chunks agree.
        let n = usize::try_from(self.docs).unwrap_or(0);
        let mut deleted = Ascending::default();
        let mut docid = 0_u32;
        self.copy_chunked(
            step,
            |inner, from| {
                let end = n.min(from.saturating_add(COPY_CHUNK));
                Ok(inner
                    .killed_at
                    .get(from..end)
                    .map(<[u64]>::to_vec)
                    .unwrap_or_default())
            },
            |killed| {
                if killed <= self.version {
                    deleted.push(docid);
                }
                docid = docid.saturating_add(1);
                Ok(())
            },
        )?;
        // A document with no kill version is not live (as `visible`).
        for missing in docid..self.docs {
            deleted.push(missing);
        }
        Ok(deleted.finish())
    }

    fn trues(&self, field: FieldId) -> Result<RoaringBitmap, ReadError> {
        self.trues_stepped(field, &|| Ok(()))
    }

    fn trues_stepped(
        &self,
        field: FieldId,
        step: &dyn Fn() -> Result<(), ReadError>,
    ) -> Result<RoaringBitmap, ReadError> {
        let mut trues = Ascending::default();
        self.copy_chunked(
            step,
            |inner, from| match field_of(inner, field)? {
                (FieldType::Bool, MemField::Column { keys }) => {
                    Ok(key_chunk(keys, from, self.docs))
                }
                (field_type, _) => Err(ReadError::FieldType {
                    field,
                    field_type,
                    what: "bool column",
                }),
            },
            |(docid, key)| {
                if key == 1 {
                    trues.push(docid);
                }
                Ok(())
            },
        )?;
        Ok(trues.finish())
    }

    fn present(&self, field: FieldId) -> Result<RoaringBitmap, ReadError> {
        self.present_stepped(field, &|| Ok(()))
    }

    fn present_stepped(
        &self,
        field: FieldId,
        step: &dyn Fn() -> Result<(), ReadError>,
    ) -> Result<RoaringBitmap, ReadError> {
        let mut present = Ascending::default();
        self.copy_chunked(
            step,
            |inner, from| {
                Ok(match field_of(inner, field)? {
                    // Lengths are recorded only for documents with a token or value, in docid order.
                    (_, MemField::Text { index, .. } | MemField::Keyword { index }) => {
                        prefix_chunk(&index.inverted.lengths, from, |&(docid, _)| {
                            docid < self.docs
                        })
                        .into_iter()
                        .map(|(docid, _)| docid)
                        .collect()
                    }
                    (_, MemField::Column { keys }) => key_chunk(keys, from, self.docs)
                        .into_iter()
                        .map(|(docid, _)| docid)
                        .collect(),
                    (field_type, MemField::Other) => {
                        return Err(ReadError::FieldType {
                            field,
                            field_type,
                            what: "values",
                        });
                    }
                })
            },
            |docid| {
                present.push(docid);
                Ok(())
            },
        )?;
        Ok(present.finish())
    }

    fn try_column_keys(
        &self,
        field: FieldId,
        mut f: impl FnMut(u32, u64) -> Result<(), ReadError>,
    ) -> Result<(), ReadError> {
        // `f` runs without the lock, and a search's interrupts, which `f` reads, stop a scan of a large column
        // (ROADMAP 7.6.5).
        self.copy_chunked(
            &|| Ok(()),
            |inner, from| self.column_chunk(inner, field, from),
            |(docid, key)| f(docid, key),
        )
    }

    fn keys(&self, field: FieldId) -> Result<MemtableKeys, ReadError> {
        self.keys_stepped(field, &|| Ok(()))
    }

    fn keys_stepped(
        &self,
        field: FieldId,
        step: &dyn Fn() -> Result<(), ReadError>,
    ) -> Result<MemtableKeys, ReadError> {
        let mut keys = Vec::new();
        self.copy_chunked(
            step,
            |inner, from| self.column_chunk(inner, field, from),
            |key| {
                keys.push(key);
                Ok(())
            },
        )?;
        Ok(MemtableKeys::new(keys))
    }

    fn keywords(&self, field: FieldId) -> Result<MemtableKeywords, ReadError> {
        self.keywords_stepped(field, &|| Ok(()))
    }

    fn keywords_stepped(
        &self,
        field: FieldId,
        step: &dyn Fn() -> Result<(), ReadError>,
    ) -> Result<MemtableKeywords, ReadError> {
        // The view's postings, then the bytes of the terms they use, each copied in chunks; the dictionary and the
        // ords are built without the lock.
        let mut postings = Vec::new();
        self.copy_chunked(
            step,
            |inner, from| {
                Ok(posting_chunk(
                    keyword_column(inner, field)?,
                    self.docs,
                    from,
                    COPY_CHUNK,
                ))
            },
            |posting| {
                postings.push(posting);
                Ok(())
            },
        )?;
        let used = used_terms(&postings);
        let mut bytes = Vec::with_capacity(used.len());
        self.copy_chunked(
            step,
            |inner, from| {
                let rest = used.get(from..).unwrap_or_default();
                let ids = rest.get(..COPY_CHUNK.min(rest.len())).unwrap_or_default();
                term_bytes(keyword_column(inner, field)?, field, ids)
            },
            |term| {
                bytes.push(term);
                Ok(())
            },
        )?;
        keywords(field, self.docs, &postings, &used, bytes)
    }

    fn norms(&self, field: FieldId) -> Result<MemtableNorms, ReadError> {
        self.norms_stepped(field, &|| Ok(()))
    }

    fn norms_stepped(
        &self,
        field: FieldId,
        step: &dyn Fn() -> Result<(), ReadError>,
    ) -> Result<MemtableNorms, ReadError> {
        let slot = usize::try_from(field.get())
            .ok()
            .and_then(|i| self.norms.get(i));
        if let Some(norms) = slot.and_then(OnceLock::get) {
            return Ok(MemtableNorms(Arc::clone(norms)));
        }
        // A norm is pushed once its document is linked, so the view's are a prefix. Copied [`COPY_CHUNK`] at a time,
        // one step per chunk: a step costs more than copying a byte.
        let docs = self.docs as usize;
        let mut copied = Vec::new();
        let mut from = 0_usize;
        loop {
            let chunk = {
                let inner = read(&self.shared);
                match indexed(&inner, field, true, "norms")? {
                    MemField::Text { norms, .. } => {
                        let end = docs.min(norms.len()).min(from.saturating_add(COPY_CHUNK));
                        norms.get(from..end).unwrap_or_default().to_vec()
                    }
                    _ => Vec::new(),
                }
            };
            let n = chunk.len();
            copied.extend(chunk);
            if n < COPY_CHUNK {
                break;
            }
            step()?;
            from = from.saturating_add(n);
        }
        let copied: Arc<[u8]> = copied.into();
        Ok(MemtableNorms(match slot {
            Some(cell) => Arc::clone(cell.get_or_init(|| copied)),
            None => copied,
        }))
    }

    fn document(&self, docid: u32) -> Result<Document, ReadError> {
        if docid >= self.docs {
            return Err(ReadError::DocOutOfRange {
                docid,
                doc_count: self.docs,
            });
        }
        // Copy the encoding out; decode without the lock.
        let bytes = {
            let inner = read(&self.shared);
            let i = docid as usize;
            let start = i
                .checked_sub(1)
                .and_then(|p| inner.doc_ends.get(p))
                .copied()
                .unwrap_or(0);
            let end = inner.doc_ends.get(i).copied().unwrap_or(start);
            inner.docs.get(start..end).unwrap_or_default().to_vec()
        };
        let mut doc = Document::decode(&bytes, &self.schema)
            .map_err(|source| ReadError::Document { docid, source })?;
        // As a segment's `DocStore` row: stored fields only (SPEC §6.4.2).
        for field in self.schema.fields() {
            if !field.kind().stored() {
                doc.remove(field.id());
            }
        }
        Ok(doc)
    }

    fn docid_of(&self, id: &str) -> Result<Option<u32>, ReadError> {
        Ok(match self.lookup(id) {
            Some(IdState::Live(docid)) => Some(docid),
            Some(IdState::Deleted) | None => None,
        })
    }
}

/// One posting of a [`MemtablePostings`].
#[derive(Debug, Clone, Copy)]
struct Entry {
    doc: u32,
    tf: u32,
    pos_start: u32,
}

/// The postings of one term in a [`MemtableView`], live documents only; one block for block-max WAND.
///
/// Holds a copy of the term's postings, taken under the read lock; positions are copied when asked for.
#[derive(Debug)]
pub struct MemtablePostings<'v> {
    view: &'v MemtableView,
    field: FieldId,
    has_positions: bool,
    entries: Vec<Entry>,
    /// The current entry; `None` before the first, `entries.len()` after the last.
    cur: Option<usize>,
    block: BlockMax,
    positions: Vec<u32>,
}

impl MemtablePostings<'_> {
    fn current(&self) -> Option<&Entry> {
        self.cur.and_then(|c| self.entries.get(c))
    }
}

impl PostingsIterator for MemtablePostings<'_> {
    fn df(&self) -> u32 {
        // At most one posting per document, and docids are u32.
        u32::try_from(self.entries.len()).unwrap_or(u32::MAX)
    }

    fn doc(&self) -> Option<u32> {
        self.current().map(|e| e.doc)
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        let next = match self.cur {
            None => 0,
            Some(c) => c.saturating_add(1).min(self.entries.len()),
        };
        self.cur = Some(next);
        self.positions.clear();
        Ok(self.doc())
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        let from = self.cur.unwrap_or(0);
        if let Some(e) = self.entries.get(from)
            && e.doc >= target
        {
            self.cur = Some(from);
            return Ok(Some(e.doc));
        }
        let rest = self.entries.get(from..).unwrap_or_default();
        let at = from.saturating_add(rest.partition_point(|e| e.doc < target));
        if self.cur != Some(at) {
            self.positions.clear();
        }
        self.cur = Some(at);
        Ok(self.doc())
    }

    fn freq(&mut self) -> Result<u32, ReadError> {
        Ok(self.current().map_or(0, |e| e.tf))
    }

    fn positions(&mut self) -> Result<&[u32], ReadError> {
        let Some(&entry) = self.current() else {
            return Ok(&[]);
        };
        if !self.has_positions {
            return Ok(&[]);
        }
        if self.positions.is_empty() {
            let inner = read(&self.view.shared);
            let positions = usize::try_from(self.field.get())
                .ok()
                .and_then(|i| inner.fields.get(i))
                .and_then(MemField::index)
                .and_then(|index| index.inverted.positions.as_ref());
            let start = entry.pos_start as usize;
            let run = positions
                .and_then(|p| p.get(start..start.saturating_add(entry.tf as usize)))
                .unwrap_or_default();
            self.positions.extend_from_slice(run);
        }
        Ok(&self.positions)
    }

    fn block_max(&self) -> Option<BlockMax> {
        self.current().map(|_| self.block)
    }

    fn shallow_advance(&mut self, target: u32) -> Result<Option<BlockMax>, ReadError> {
        Ok((self.block.last_docid >= target).then_some(self.block))
    }

    fn term_bound(&self) -> FieldHit {
        FieldHit::from(self.block)
    }
}

/// The norms of one `text` field of a [`MemtableView`], as of the view; shared by every reader of that view.
#[derive(Debug, Clone)]
pub struct MemtableNorms(Arc<[u8]>);

impl NormReader for MemtableNorms {
    fn get(&mut self, docid: u32) -> Result<u8, ReadError> {
        Ok(self.0.get(docid as usize).copied().unwrap_or(0))
    }
}

/// The terms of one field of a [`MemtableView`] that a [`SearchView::terms`] call selected, in byte order: copied out
/// under the read lock, each with its live `df` in the view.
#[derive(Debug, Clone)]
pub struct MemtableTerms {
    terms: Vec<(Vec<u8>, u32)>,
    next: usize,
}

impl TermEnum for MemtableTerms {
    fn next(&mut self) -> Result<Option<(&[u8], u32)>, ReadError> {
        let Some((term, df)) = self.terms.get(self.next) else {
            return Ok(None);
        };
        self.next = self.next.saturating_add(1);
        Ok(Some((term, *df)))
    }
}

/// The inverted index of `keyword` field `field`, which holds its column (SPEC §5.3.5).
fn keyword_column(inner: &Inner, field: FieldId) -> Result<&Inverted, ReadError> {
    match field_of(inner, field)? {
        (_, MemField::Keyword { index }) => Ok(&index.inverted),
        (field_type, _) => Err(ReadError::FieldType {
            field,
            field_type,
            what: "keyword column",
        }),
    }
}
