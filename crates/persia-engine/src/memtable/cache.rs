//! The column copies the views of one memtable share (ROADMAP 7.5.7): a field's column keys and keyword column, as
//! the latest view that copied them saw them.
//!
//! A view's copy of a column is the part of it below the view's document count (`docs`): keys and keyword postings
//! are appended in docid order and never rewritten (a failed `put` rolls back only what it appended, at docids not
//! in any view yet), and which of those documents are live is not in the copy (a view reads its kill versions
//! for that, [`super::MemtableView`]'s `is_live` and `deleted`). So two views with the same `docs` copy the same
//! bytes, whatever operations each saw, and the copy is keyed by `docs` alone. A view of other documents never
//! reads one.
//!
//! The cache lives in the memtable's shared state and is dropped with it: a flush starts a new memtable. It holds
//! at most one copy per field and kind, the one of the most documents copied so far: a view of fewer documents copies
//! its own and leaves the cached one, so readers still on an older view do not evict the one new requests share. Its
//! bytes count in [`super::Memtable::size_in_bytes`], so the flush threshold bounds it with the memtable.

use std::sync::{Mutex, PoisonError};

use super::columns::{MemtableKeys, MemtableKeywords};
use crate::schema::FieldId;

/// The shared copies, by field id.
#[derive(Debug)]
pub(crate) struct ColumnCache {
    slots: Mutex<Vec<Slot>>,
}

#[derive(Debug, Default, Clone)]
struct Slot {
    /// The keys of the first `docs` documents.
    keys: Option<(u32, MemtableKeys)>,
    /// A keyword column records its own `docs`.
    keywords: Option<MemtableKeywords>,
}

impl ColumnCache {
    /// An empty cache for `fields` fields.
    pub(crate) fn new(fields: usize) -> Self {
        Self {
            slots: Mutex::new(vec![Slot::default(); fields]),
        }
    }

    /// The slots. A panic while one was held left every slot either as it was or replaced whole, so a poisoned lock
    /// still guards consistent copies.
    fn slots(&self) -> std::sync::MutexGuard<'_, Vec<Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The keys of column `field` for documents `0..docs`, when they are the ones cached.
    pub(crate) fn keys(&self, field: FieldId, docs: u32) -> Option<MemtableKeys> {
        let slots = self.slots();
        let (cached, keys) = slot(&slots, field)?.keys.as_ref()?;
        (*cached == docs).then(|| keys.clone())
    }

    /// Keeps `keys`, the keys of column `field` for documents `0..docs`, unless a copy of as many documents or more is
    /// cached (of two views of the same documents copying at once, the first to finish is kept; the other reads its
    /// own until it drops). Returns the copy it replaced, for the caller to drop once it holds no lock: freeing a large
    /// copy is slow (`docs/benchmarks.md`, ROADMAP 7.5.7), and a writer must not wait for it.
    #[must_use = "drop the replaced copy outside the memtable's locks"]
    pub(crate) fn keep_keys(
        &self,
        field: FieldId,
        docs: u32,
        keys: &MemtableKeys,
    ) -> Option<(u32, MemtableKeys)> {
        let mut slots = self.slots();
        let slot = slot_mut(&mut slots, field)?;
        if slot.keys.as_ref().is_none_or(|(cached, _)| *cached < docs) {
            slot.keys.replace((docs, keys.clone()))
        } else {
            None
        }
    }

    /// The keyword column of `field` for documents `0..docs`, when it is the one cached.
    pub(crate) fn keywords(&self, field: FieldId, docs: u32) -> Option<MemtableKeywords> {
        let slots = self.slots();
        let keywords = slot(&slots, field)?.keywords.as_ref()?;
        (keywords.docs() == docs).then(|| keywords.clone())
    }

    /// Keeps `keywords`, a keyword column of `field`, unless a copy of as many documents or more is cached; returns
    /// the copy it replaced, as [`Self::keep_keys`].
    #[must_use = "drop the replaced copy outside the memtable's locks"]
    pub(crate) fn keep_keywords(
        &self,
        field: FieldId,
        keywords: &MemtableKeywords,
    ) -> Option<MemtableKeywords> {
        let mut slots = self.slots();
        let slot = slot_mut(&mut slots, field)?;
        if slot
            .keywords
            .as_ref()
            .is_none_or(|cached| cached.docs() < keywords.docs())
        {
            slot.keywords.replace(keywords.clone())
        } else {
            None
        }
    }

    /// Heap bytes of the cached copies. A copy a view still holds after the cache replaced it is the view's, not
    /// counted here.
    /// O(fields): a keyword column's count is stored with it, as the memtable reads this on every write.
    pub(crate) fn size_in_bytes(&self) -> usize {
        self.total(MemtableKeywords::size_in_bytes)
    }

    /// [`Self::size_in_bytes`], with each keyword column walked again instead of its stored count.
    #[cfg(test)]
    pub(crate) fn count_bytes(&self) -> usize {
        self.total(MemtableKeywords::count_bytes)
    }

    fn total(&self, words: fn(&MemtableKeywords) -> usize) -> usize {
        let slots = self.slots();
        let copies = slots.iter().fold(0_usize, |sum, s| {
            let keys = s.keys.as_ref().map_or(0, |(_, k)| k.size_in_bytes());
            let words = s.keywords.as_ref().map_or(0, words);
            sum.saturating_add(keys).saturating_add(words)
        });
        copies.saturating_add(slots.capacity().saturating_mul(size_of::<Slot>()))
    }
}

fn slot(slots: &[Slot], field: FieldId) -> Option<&Slot> {
    usize::try_from(field.get()).ok().and_then(|i| slots.get(i))
}

fn slot_mut(slots: &mut [Slot], field: FieldId) -> Option<&mut Slot> {
    usize::try_from(field.get())
        .ok()
        .and_then(|i| slots.get_mut(i))
}
