//! Test helpers for crates that reach `persia-storage` only through this one (`testing` feature; ROADMAP 9.3.6).

use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

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

/// A [`Storage`] without `strong_list` (SPEC §10.1: a just-written object may be missing from a listing): until
/// [`LaggingList::catch_up`], every listing misses the objects written after `since`. Everything else is `inner`'s.
/// A sweep's first listing that misses a fresh chunk is ROADMAP 9.5.10.
#[derive(Debug)]
pub struct LaggingList<S> {
    inner: S,
    since: SystemTime,
    lagging: AtomicBool,
    hidden: AtomicU64,
}

impl<S> LaggingList<S> {
    /// `inner`, lagging behind every write after `since` until [`LaggingList::catch_up`].
    pub fn new(inner: S, since: SystemTime) -> Self {
        Self {
            inner,
            since,
            lagging: AtomicBool::new(true),
            hidden: AtomicU64::new(0),
        }
    }

    /// From now on, listings show every object.
    pub fn catch_up(&self) {
        self.lagging.store(false, Ordering::SeqCst);
    }

    /// How many listed objects were left out so far.
    pub fn hidden(&self) -> u64 {
        self.hidden.load(Ordering::SeqCst)
    }
}

impl<S: Storage> Storage for LaggingList<S> {
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
        if self.lagging.load(Ordering::SeqCst) {
            let before = listed.len();
            listed.retain(|meta| meta.last_modified.is_none_or(|t| t <= self.since));
            let hidden = u64::try_from(before.saturating_sub(listed.len())).unwrap_or(u64::MAX);
            self.hidden.fetch_add(hidden, Ordering::SeqCst);
        }
        Ok(listed)
    }

    fn delete(&self, key: &Key) -> Result<()> {
        self.inner.delete(key)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            strong_list: false,
            ..self.inner.capabilities()
        }
    }
}
