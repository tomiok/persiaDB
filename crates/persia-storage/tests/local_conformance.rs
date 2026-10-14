//! `LocalStorage` passes the `Storage` conformance suite (ROADMAP 2.3.5).

// Test-only code: unwraps are the point.
#![allow(clippy::unwrap_used)]

#[macro_use]
mod conformance;

use core::ops::Range;

use bytes::Bytes;
use persia_format::ByteSource;
use persia_storage::{
    Capabilities, Key, KeyPrefix, LocalStorage, ObjectMeta, PutOutcome, Result, Storage,
};
use tempfile::TempDir;

/// A store in its own temporary directory, removed with it (the suite builds one per test and model case).
struct TempStore {
    store: LocalStorage,
    _dir: TempDir,
}

impl TempStore {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStorage::open(dir.path().join("store")).unwrap();
        Self { store, _dir: dir }
    }
}

impl Storage for TempStore {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        self.store.get_range(key, range)
    }
    fn get(&self, key: &Key) -> Result<Bytes> {
        self.store.get(key)
    }
    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.store.put(key, data)
    }
    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        self.store.put_if_absent(key, data)
    }
    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        self.store.list(prefix)
    }
    fn delete(&self, key: &Key) -> Result<()> {
        self.store.delete(key)
    }
    fn capabilities(&self) -> Capabilities {
        self.store.capabilities()
    }
}

// Every model case creates a directory tree and fsyncs it: fewer cases than the in-memory backend.
storage_conformance!(TempStore::new(), cases = conformance::MODEL_CASES / 4);
