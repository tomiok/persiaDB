//! Test helpers for crates that reach `persia-storage` only through this one (`testing` feature; ROADMAP 9.3.6).

use core::ops::Range;

use bytes::Bytes;
use persia_format::ByteSource;
use persia_storage::{Capabilities, Key, KeyPrefix, ObjectMeta, PutOutcome, Result, Storage};

/// A [`Storage`] whose [`Storage::get`] of `bad` reports corruption (`ShortRead`, which is corruption).
/// Everything else is `inner`. A resume treats that chunk as missing (SPEC §9.4).
#[derive(Debug)]
pub struct CorruptGet<S> {
    /// The backend.
    pub inner: S,
    /// The object whose `get` fails.
    pub bad: Key,
}

impl<S: Storage> Storage for CorruptGet<S> {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        self.inner.get_range(key, range)
    }

    fn get(&self, key: &Key) -> Result<Bytes> {
        if key == &self.bad {
            return Err(persia_storage::Error::ShortRead {
                key: key.clone(),
                requested: 0..1,
                returned: 0,
            });
        }
        self.inner.get(key)
    }

    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.inner.put(key, data)
    }

    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        self.inner.put_if_absent(key, data)
    }

    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        self.inner.list(prefix)
    }

    fn delete(&self, key: &Key) -> Result<()> {
        self.inner.delete(key)
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

/// How [`MisorderedList`] lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOrder {
    /// Every object, in reverse key order.
    Reversed,
    /// Every object twice: all in key order, then all again.
    Duplicated,
}

/// A [`Storage`] that lists what `inner` lists out of key order or with duplicates, as a backend may (SPEC §10.1
/// promises neither order nor uniqueness of a listing): everything else is `inner`'s.
#[derive(Debug)]
pub struct MisorderedList<S> {
    /// The backend.
    pub inner: S,
    /// How it lists.
    pub order: ListOrder,
}

impl<S: Storage> Storage for MisorderedList<S> {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        self.inner.get_range(key, range)
    }

    fn get(&self, key: &Key) -> Result<Bytes> {
        self.inner.get(key)
    }

    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.inner.put(key, data)
    }

    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        self.inner.put_if_absent(key, data)
    }

    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        let mut listed = self.inner.list(prefix)?;
        match self.order {
            ListOrder::Reversed => listed.reverse(),
            ListOrder::Duplicated => {
                let again = listed.clone();
                listed.extend(again);
            }
        }
        Ok(listed)
    }

    fn delete(&self, key: &Key) -> Result<()> {
        self.inner.delete(key)
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}
