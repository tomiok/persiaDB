//! The in-memory backend (`memory://`, SPEC §10.2): every guarantee of SPEC §10.1, nothing persisted.
//!
//! The reference backend for tests: the conformance suite and [`FaultyStorage`](crate::faulty::FaultyStorage) run
//! on it, and the engine's tests use it where durability is not under test.

use core::ops::Range;
use std::collections::BTreeMap;
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use bytes::Bytes;
use persia_format::ByteSource;

use crate::error::{Error, Result};
use crate::key::{Key, KeyPrefix};
use crate::meta::{Capabilities, ObjectMeta, PutOutcome};
use crate::storage::{Storage, check_range, range_source};

/// Objects held in a map in memory; dropped with the value.
///
/// Every operation is atomic (one lock), so `put_if_absent` is a true compare-and-set and `list` sees every
/// completed `put`.
///
/// ```
/// use bytes::Bytes;
/// use persia_storage::memory::MemoryStorage;
/// use persia_storage::{Key, PutOutcome, Storage};
///
/// let storage = MemoryStorage::new();
/// let key = Key::new("manifests/00000001")?;
/// assert_eq!(storage.put_if_absent(&key, Bytes::from_static(b"m1"))?, PutOutcome::Created);
/// assert_eq!(storage.put_if_absent(&key, Bytes::from_static(b"m2"))?, PutOutcome::AlreadyExists);
/// assert_eq!(&*storage.get_range(&key, 1..2)?, b"1");
/// # Ok::<(), persia_storage::Error>(())
/// ```
#[derive(Debug, Default)]
pub struct MemoryStorage {
    objects: RwLock<BTreeMap<Key, Bytes>>,
}

impl MemoryStorage {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    // Every mutation is a single map insert or remove, so a panic in another thread cannot leave the map
    // half-updated: a poisoned lock is safe to keep using.
    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<Key, Bytes>> {
        self.objects.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, BTreeMap<Key, Bytes>> {
        self.objects.write().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Storage for MemoryStorage {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        check_range(key, &range)?;
        let object = self.get(key)?;
        let len = u64::try_from(object.len()).unwrap_or(u64::MAX);
        if range.end > len {
            return Err(Error::ShortRead {
                key: key.clone(),
                returned: len.saturating_sub(range.start),
                requested: range,
            });
        }
        // Both bounds are <= the object's length, so they fit in usize.
        let start = usize::try_from(range.start).unwrap_or(usize::MAX);
        let end = usize::try_from(range.end).unwrap_or(usize::MAX);
        range_source(key, range, object.slice(start..end))
    }

    fn get(&self, key: &Key) -> Result<Bytes> {
        self.read()
            .get(key)
            .cloned()
            .ok_or_else(|| Error::NotFound { key: key.clone() })
    }

    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.write().insert(key.clone(), data);
        Ok(())
    }

    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        let mut objects = self.write();
        match objects.get(key) {
            // SPEC §10.1: an existing object equal to `data` may be this caller's own earlier attempt.
            Some(existing) if *existing == data => Ok(PutOutcome::Created),
            Some(_) => Ok(PutOutcome::AlreadyExists),
            None => {
                objects.insert(key.clone(), data);
                Ok(PutOutcome::Created)
            }
        }
    }

    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        // `Key`'s order is its string's byte order, so the map iterates sorted by key bytes.
        Ok(self
            .read()
            .range::<str, _>((
                core::ops::Bound::Included(prefix.as_str()),
                core::ops::Bound::Unbounded,
            ))
            .take_while(|(key, _)| prefix.matches(key))
            .map(|(key, data)| ObjectMeta {
                key: key.clone(),
                size: u64::try_from(data.len()).unwrap_or(u64::MAX),
            })
            .collect())
    }

    fn delete(&self, key: &Key) -> Result<()> {
        self.write().remove(key);
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            conditional_put: true,
            strong_list: true,
            append: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    fn key(s: &str) -> Key {
        Key::new(s).unwrap()
    }

    #[test]
    fn list_uses_the_ordered_map_from_the_prefix_on() {
        let s = MemoryStorage::new();
        for k in ["a", "b/1", "b/2", "b0", "c"] {
            s.put(&key(k), Bytes::from_static(b"x")).unwrap();
        }
        let keys = |p: &str| -> Vec<String> {
            s.list(&KeyPrefix::new(p).unwrap())
                .unwrap()
                .into_iter()
                .map(|m| m.key.as_str().to_owned())
                .collect()
        };
        assert_eq!(keys("b"), ["b/1", "b/2", "b0"]);
        assert_eq!(keys("b/"), ["b/1", "b/2"]);
        assert_eq!(keys(""), ["a", "b/1", "b/2", "b0", "c"]);
        assert_eq!(keys("d"), Vec::<String>::new());
    }

    #[test]
    fn a_range_starting_past_the_end_is_a_short_read() {
        let s = MemoryStorage::new();
        s.put(&key("k"), Bytes::from_static(b"0123456789")).unwrap();
        let err = s.get_range(&key("k"), 20..20).unwrap_err();
        assert!(matches!(err, Error::ShortRead { returned: 0, .. }), "{err}");
        let err = s.get_range(&key("k"), 8..12).unwrap_err();
        assert!(matches!(err, Error::ShortRead { returned: 2, .. }), "{err}");
        assert_eq!(err.kind(), ErrorKind::Corrupt);
    }
}
