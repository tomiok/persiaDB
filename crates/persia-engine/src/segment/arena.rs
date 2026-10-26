//! Byte-string interning without a heap allocation per string (ROADMAP 5.1.1.1).
//!
//! Every interned string is appended to one byte buffer and named by a dense `u32` id in insertion order; an
//! open-addressing table of ids finds an existing string. A segment builder interns every term of a field (and every
//! document id) here, so indexing a token that was seen before allocates nothing, and a new one at most grows
//! three vectors.

use persia_format::hash::xxh3_64;

/// Marks an empty slot of the table.
const EMPTY: u32 = u32::MAX;
/// Slots in a new table; a power of two.
const INITIAL_SLOTS: usize = 16;

/// Interned byte strings with dense ids `0..len()`, in insertion order.
#[derive(Debug, Clone)]
pub(crate) struct Arena {
    bytes: Vec<u8>,
    /// `ends[id]` is the end of string `id` in `bytes`; it starts at the previous end (or 0).
    ends: Vec<usize>,
    hashes: Vec<u64>,
    /// Ids by hash, linear probing; at most half full, so a probe always reaches an empty slot.
    slots: Vec<u32>,
}

impl Default for Arena {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            ends: Vec::new(),
            hashes: Vec::new(),
            slots: vec![EMPTY; INITIAL_SLOTS],
        }
    }
}

/// More strings than ids: an arena holds at most `u32::MAX` strings (the last id is the empty-slot mark).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArenaFull;

impl Arena {
    /// Number of strings.
    pub(crate) fn len(&self) -> usize {
        self.ends.len()
    }

    /// The id of `key`, if interned.
    pub(crate) fn get(&self, key: &[u8]) -> Option<u32> {
        let hash = xxh3_64(key);
        match self.slots.get(self.find(key, hash)) {
            Some(&id) if id != EMPTY => Some(id),
            _ => None,
        }
    }

    /// The id of `key`, interning it first if it is new; the flag tells whether it was.
    pub(crate) fn intern(&mut self, key: &[u8]) -> Result<(u32, bool), ArenaFull> {
        let hash = xxh3_64(key);
        let slot = self.find(key, hash);
        if let Some(&id) = self.slots.get(slot).filter(|&&id| id != EMPTY) {
            return Ok((id, false));
        }
        let id = u32::try_from(self.ends.len())
            .ok()
            .filter(|&id| id != EMPTY)
            .ok_or(ArenaFull)?;
        self.bytes.extend_from_slice(key);
        self.ends.push(self.bytes.len());
        self.hashes.push(hash);
        if let Some(s) = self.slots.get_mut(slot) {
            *s = id;
        }
        // Keep the load at most 1/2.
        if self.ends.len().saturating_mul(2) > self.slots.len() {
            self.grow();
        }
        Ok((id, true))
    }

    /// Forgets every string with an id at or above `len` (a failed document's new terms). Rebuilds the table, which
    /// costs its size: an error path only.
    pub(crate) fn truncate(&mut self, len: usize) {
        if len >= self.ends.len() {
            return;
        }
        let end = len
            .checked_sub(1)
            .and_then(|last| self.ends.get(last))
            .copied()
            .unwrap_or(0);
        self.bytes.truncate(end);
        self.ends.truncate(len);
        self.hashes.truncate(len);
        self.slots.fill(EMPTY);
        self.rehash_into_slots();
    }

    /// The string with id `id` (empty for an id that was never returned).
    pub(crate) fn key(&self, id: u32) -> &[u8] {
        let Ok(i) = usize::try_from(id) else {
            return &[];
        };
        let start = match i.checked_sub(1) {
            Some(prev) => self.ends.get(prev).copied().unwrap_or(0),
            None => 0,
        };
        let end = self.ends.get(i).copied().unwrap_or(start);
        self.bytes.get(start..end).unwrap_or_default()
    }

    /// The strings, concatenated in id order, and the end of each.
    pub(crate) fn records(&self) -> (&[u8], &[usize]) {
        (&self.bytes, &self.ends)
    }

    /// Every id, ordered by the bytes of its string.
    pub(crate) fn sorted_ids(&self) -> Vec<u32> {
        // Ids fit in u32 by construction (`intern`).
        let mut ids: Vec<u32> = (0..u32::try_from(self.len()).unwrap_or(u32::MAX)).collect();
        ids.sort_unstable_by(|&a, &b| self.key(a).cmp(self.key(b)));
        ids
    }

    /// Heap bytes held (capacities, not lengths: what the allocator gave out).
    pub(crate) fn size_in_bytes(&self) -> usize {
        self.bytes
            .capacity()
            .saturating_add(self.ends.capacity().saturating_mul(size_of::<usize>()))
            .saturating_add(self.hashes.capacity().saturating_mul(size_of::<u64>()))
            .saturating_add(self.slots.capacity().saturating_mul(size_of::<u32>()))
    }

    /// The slot holding `key`, or the empty slot where it would go.
    fn find(&self, key: &[u8], hash: u64) -> usize {
        let mask = self.slots.len().wrapping_sub(1);
        // Truncation keeps the low bits, which the mask selects anyway.
        #[allow(clippy::cast_possible_truncation)]
        let mut slot = (hash as usize) & mask;
        loop {
            match self.slots.get(slot) {
                Some(&id) if id != EMPTY => {
                    let same = self.hashes.get(id as usize) == Some(&hash) && self.key(id) == key;
                    if same {
                        return slot;
                    }
                }
                _ => return slot,
            }
            slot = slot.wrapping_add(1) & mask;
        }
    }

    fn grow(&mut self) {
        self.slots = vec![EMPTY; self.slots.len().saturating_mul(2)];
        self.rehash_into_slots();
    }

    /// Places every id in `slots`, which must be empty and large enough.
    fn rehash_into_slots(&mut self) {
        let mask = self.slots.len().wrapping_sub(1);
        for (id, &hash) in (0_u32..).zip(&self.hashes) {
            #[allow(clippy::cast_possible_truncation)]
            let mut slot = (hash as usize) & mask;
            while let Some(s) = self.slots.get_mut(slot) {
                if *s == EMPTY {
                    *s = id;
                    break;
                }
                slot = slot.wrapping_add(1) & mask;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn interns_in_insertion_order() {
        let mut a = Arena::default();
        assert_eq!(a.intern(b"b"), Ok((0, true)));
        assert_eq!(a.intern(b"a"), Ok((1, true)));
        assert_eq!(a.intern(b"b"), Ok((0, false)));
        assert_eq!(a.intern(b""), Ok((2, true)));
        assert_eq!(a.len(), 3);
        assert_eq!(
            (a.key(0), a.key(1), a.key(2)),
            (&b"b"[..], &b"a"[..], &b""[..])
        );
        assert_eq!(a.get(b"a"), Some(1));
        assert_eq!(a.get(b"c"), None);
        assert_eq!(a.sorted_ids(), [2, 1, 0]);
        assert_eq!(a.key(99), b"");
    }

    #[test]
    fn grows_past_many_keys() {
        let mut a = Arena::default();
        for i in 0..10_000_u32 {
            assert_eq!(a.intern(&i.to_le_bytes()), Ok((i, true)));
        }
        for i in 0..10_000_u32 {
            assert_eq!(a.get(&i.to_le_bytes()), Some(i));
        }
        assert!(a.slots.len() >= 2 * a.len());
        assert!(a.size_in_bytes() >= a.bytes.len() + a.slots.len() * 4);
    }

    #[test]
    fn truncate_forgets_later_keys() {
        let mut a = Arena::default();
        for i in 0..100_u32 {
            a.intern(&i.to_le_bytes()).unwrap();
        }
        a.truncate(40);
        assert_eq!(a.len(), 40);
        assert_eq!(a.get(&39_u32.to_le_bytes()), Some(39));
        assert_eq!(a.get(&40_u32.to_le_bytes()), None);
        assert_eq!(a.intern(&99_u32.to_le_bytes()), Ok((40, true)));
        assert_eq!(a.key(40), 99_u32.to_le_bytes());
        a.truncate(0);
        assert_eq!((a.len(), a.get(&0_u32.to_le_bytes())), (0, None));
    }

    proptest! {
        /// The arena agrees with a `HashMap` model on every sequence of keys.
        #[test]
        fn matches_hash_map(keys in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..6), 0..300)) {
            let mut a = Arena::default();
            let mut model: HashMap<Vec<u8>, u32> = HashMap::new();
            for key in &keys {
                let next = u32::try_from(model.len()).unwrap();
                let expected_id = *model.entry(key.clone()).or_insert(next);
                let (id, new) = a.intern(key).unwrap();
                prop_assert_eq!(id, expected_id);
                prop_assert_eq!(new, expected_id == next);
            }
            prop_assert_eq!(a.len(), model.len());
            for (key, &id) in &model {
                prop_assert_eq!(a.key(id), key.as_slice());
                prop_assert_eq!(a.get(key), Some(id));
            }
            let sorted: Vec<&[u8]> = a.sorted_ids().into_iter().map(|id| a.key(id)).collect();
            prop_assert!(sorted.windows(2).all(|w| w[0] < w[1]));
        }
    }
}
