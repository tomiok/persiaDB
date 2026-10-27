//! A point-in-time view of a memtable, searchable like a segment (ROADMAP 6.1.2).

use std::sync::{Arc, RwLock};

use super::{Inner, MemField, NONE, read};
use crate::document::Document;
use crate::schema::{FieldId, FieldType};
use crate::segment::{BlockMax, FieldStats, ReadError};
use crate::view::{PostingsIterator, SearchView};

/// What the memtable knew of an id when a view was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdState {
    /// Its latest operation was a `put`: the live document has this docid.
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
    /// Documents `0..docs` existed.
    docs: u32,
    /// Operations `1..=version` were applied.
    version: u64,
    /// Live statistics by field id, as of `version`.
    stats: Vec<Option<FieldStats>>,
    live: u32,
}

impl MemtableView {
    pub(super) fn new(shared: Arc<RwLock<Inner>>, inner: &Inner) -> Self {
        Self {
            docs: inner.doc_count(),
            version: inner.version(),
            stats: inner.stats.clone(),
            live: inner.live,
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

    fn visible(&self, inner: &Inner, docid: u32) -> bool {
        docid < self.docs
            && inner
                .killed_at
                .get(docid as usize)
                .is_some_and(|&k| k > self.version)
    }
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
    type Postings<'a>
        = MemtablePostings<'a>
    where
        Self: 'a;

    fn doc_count(&self) -> u32 {
        self.docs
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
            MemField::Other => return Ok(None),
        };
        let Some(mut at) = index
            .inverted
            .terms
            .get(term)
            .and_then(|t| index.heads.get(t as usize).copied())
        else {
            return Ok(None);
        };
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
        Ok(Some(MemtablePostings {
            view: self,
            field,
            has_positions: index.inverted.positions.is_some(),
            entries,
            cur: None,
            block,
            positions: Vec::new(),
        }))
    }

    fn norm(&self, field: FieldId, docid: u32) -> Result<u8, ReadError> {
        let inner = read(&self.shared);
        match indexed(&inner, field, true, "norms")? {
            MemField::Text { norms, .. } if docid < self.docs => {
                Ok(norms.get(docid as usize).copied().unwrap_or(0))
            }
            _ => Ok(0),
        }
    }

    fn document(&self, docid: u32) -> Result<Document, ReadError> {
        if docid >= self.docs {
            return Err(ReadError::DocOutOfRange {
                docid,
                doc_count: self.docs,
            });
        }
        let inner = read(&self.shared);
        let i = docid as usize;
        let start = i
            .checked_sub(1)
            .and_then(|p| inner.doc_ends.get(p))
            .copied()
            .unwrap_or(0);
        let end = inner.doc_ends.get(i).copied().unwrap_or(start);
        let bytes = inner.docs.get(start..end).unwrap_or_default();
        let mut doc = Document::decode(bytes, &inner.schema)
            .map_err(|source| ReadError::Document { docid, source })?;
        // As a segment's `DocStore` row: stored fields only (SPEC §6.4.2).
        for field in inner.schema.fields() {
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
}
