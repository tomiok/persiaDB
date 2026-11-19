//! Helpers shared by the remote layout's tests.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    missing_docs
)]

use core::ops::Range;
use std::collections::BTreeMap;

use bytes::Bytes;
use persia_blob::{BindOutcome, BlobManifest, BlobStore, ChunkSize, UploadId, chunk_all};
use persia_format::{BlobRef, ByteSource};
use persia_storage::{Capabilities, Key, KeyPrefix, ObjectMeta, Op, PutOutcome, Result, Storage};

/// Deterministic bytes: `len` of them from `seed`.
pub fn data_of(len: usize, seed: u8) -> Vec<u8> {
    let mut x = u64::from(seed).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect()
}

/// Chunks `data`, stages every chunk under `upload`, and returns the (unbound) manifest.
pub fn staged_manifest<S: BlobStore + ?Sized>(
    store: &S,
    data: &[u8],
    chunk: u32,
    upload: u128,
) -> BlobManifest {
    let upload_id = UploadId::new(upload);
    let mut crcs = Vec::new();
    let blob = chunk_all(data, ChunkSize::from_encoding(chunk).unwrap(), |c| {
        crcs.push(c.crc32c());
        store.stage_chunk(upload_id, c.index(), c.bytes())
    })
    .unwrap();
    BlobManifest::new(blob.blob_id, blob.size, blob.chunk_size, upload_id, crcs).unwrap()
}

/// [`staged_manifest`], then binds it (`Bound` expected).
pub fn stage_and_bind<S: BlobStore + ?Sized>(
    store: &S,
    data: &[u8],
    chunk: u32,
    upload: u128,
) -> BlobManifest {
    let m = staged_manifest(store, data, chunk, upload);
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    m
}

/// The manifest of `data` without staging anything.
pub fn manifest_of(data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
    let mut crcs = Vec::new();
    let blob = chunk_all(data, ChunkSize::from_encoding(chunk).unwrap(), |c| {
        crcs.push(c.crc32c());
        Ok(())
    })
    .unwrap();
    BlobManifest::new(
        blob.blob_id,
        blob.size,
        blob.chunk_size,
        UploadId::new(upload),
        crcs,
    )
    .unwrap()
}

/// A reference to the blob `m` describes.
pub fn blob_ref(m: &BlobManifest) -> BlobRef {
    BlobRef::new(
        m.blob_id(),
        m.size(),
        "application/octet-stream",
        m.chunk_size().get(),
        BTreeMap::new(),
    )
    .unwrap()
}

/// Every key in `storage`, sorted.
pub fn keys_of<S: Storage + ?Sized>(storage: &S) -> Vec<String> {
    storage
        .list(&KeyPrefix::default())
        .unwrap()
        .into_iter()
        .map(|m| m.key.as_str().to_owned())
        .collect()
}

/// A backend that claims no `conditional_put`, over another.
#[derive(Debug)]
pub struct NoConditionalPut<S>(pub S);

impl<S: Storage> Storage for NoConditionalPut<S> {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        self.0.get_range(key, range)
    }
    fn get(&self, key: &Key) -> Result<Bytes> {
        self.0.get(key)
    }
    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.0.put(key, data)
    }
    fn put_if_absent(&self, _: &Key, _: Bytes) -> Result<PutOutcome> {
        Err(persia_storage::Error::Unsupported {
            op: Op::PutIfAbsent,
        })
    }
    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        self.0.list(prefix)
    }
    fn delete(&self, key: &Key) -> Result<()> {
        self.0.delete(key)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            conditional_put: false,
            ..self.0.capabilities()
        }
    }
}

/// A backend whose every `get` reports corruption (a short read).
#[derive(Debug)]
pub struct CorruptGets<S>(pub S);

impl<S: Storage> Storage for CorruptGets<S> {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        self.0.get_range(key, range)
    }
    fn get(&self, key: &Key) -> Result<Bytes> {
        Err(persia_storage::Error::ShortRead {
            key: key.clone(),
            requested: 0..1,
            returned: 0,
        })
    }
    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.0.put(key, data)
    }
    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        self.0.put_if_absent(key, data)
    }
    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        self.0.list(prefix)
    }
    fn delete(&self, key: &Key) -> Result<()> {
        self.0.delete(key)
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
}

/// A backend whose `list` ignores the prefix (a broken or hostile listing).
#[derive(Debug)]
pub struct ListsEverything<S>(pub S);

impl<S: Storage> Storage for ListsEverything<S> {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        self.0.get_range(key, range)
    }
    fn get(&self, key: &Key) -> Result<Bytes> {
        self.0.get(key)
    }
    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.0.put(key, data)
    }
    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        self.0.put_if_absent(key, data)
    }
    fn list(&self, _: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        self.0.list(&KeyPrefix::default())
    }
    fn delete(&self, key: &Key) -> Result<()> {
        self.0.delete(key)
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
}

/// A backend that fails chosen calls by key, whatever order parallel workers reach it in, and counts the requests in
/// flight at once.
#[derive(Debug, Default)]
pub struct KeyFaults<S> {
    pub inner: S,
    /// `(op, key)` → how many more calls fail, and with a transient (`true`) or an I/O (`false`) error.
    pub faults: std::sync::Mutex<BTreeMap<(String, String), (u32, bool)>>,
    pub in_flight: std::sync::atomic::AtomicUsize,
    pub max_in_flight: std::sync::atomic::AtomicUsize,
    /// `list` reports this length for every object instead of its own (a stale or lying listing).
    pub list_size: Option<u64>,
}

impl<S: Storage> KeyFaults<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            faults: std::sync::Mutex::default(),
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            max_in_flight: std::sync::atomic::AtomicUsize::new(0),
            list_size: None,
        }
    }

    pub fn fail(&self, op: Op, key: &str, times: u32, transient: bool) {
        self.faults
            .lock()
            .unwrap()
            .insert((op.to_string(), key.to_owned()), (times, transient));
    }

    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn call<T>(&self, op: Op, key: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.max_in_flight.fetch_max(now, SeqCst);
        // Give other workers a chance to overlap with this request.
        std::thread::yield_now();
        let fault = {
            let mut faults = self.faults.lock().unwrap();
            match faults.get_mut(&(op.to_string(), key.to_owned())) {
                Some((n, transient)) if *n > 0 => {
                    *n -= 1;
                    Some(*transient)
                }
                _ => None,
            }
        };
        let out = match fault {
            Some(true) => Err(persia_storage::Error::Transient {
                op,
                target: key.to_owned(),
                source: "injected".into(),
            }),
            Some(false) => Err(persia_storage::Error::Io {
                op,
                target: key.to_owned(),
                source: std::io::Error::other("injected"),
            }),
            None => f(),
        };
        self.in_flight.fetch_sub(1, SeqCst);
        out
    }
}

impl<S: Storage> Storage for KeyFaults<S> {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        self.call(Op::GetRange, key.as_str(), || {
            self.inner.get_range(key, range)
        })
    }
    fn get(&self, key: &Key) -> Result<Bytes> {
        self.call(Op::Get, key.as_str(), || self.inner.get(key))
    }
    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.call(Op::Put, key.as_str(), || self.inner.put(key, data))
    }
    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        self.call(Op::PutIfAbsent, key.as_str(), || {
            self.inner.put_if_absent(key, data)
        })
    }
    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        let mut listed = self.call(Op::List, prefix.as_str(), || self.inner.list(prefix))?;
        if let Some(size) = self.list_size {
            for meta in &mut listed {
                meta.size = size;
            }
        }
        Ok(listed)
    }
    fn delete(&self, key: &Key) -> Result<()> {
        self.call(Op::Delete, key.as_str(), || self.inner.delete(key))
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

/// A backend whose answers a test scripts: the next `put_if_absent` answers come from `puts` (then the inner
/// backend's), the next `gets_not_found` gets answer `NotFound`, and with `unsupported` every `put_if_absent` is
/// `Unsupported` although `conditional_put` is claimed (a backend that lies about its capabilities).
#[derive(Debug, Default)]
pub struct Scripted<S> {
    pub inner: S,
    pub puts: std::sync::Mutex<std::collections::VecDeque<PutOutcome>>,
    pub gets_not_found: std::sync::atomic::AtomicU32,
    pub unsupported: bool,
}

impl<S: Storage> Storage for Scripted<S> {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        self.inner.get_range(key, range)
    }
    fn get(&self, key: &Key) -> Result<Bytes> {
        use std::sync::atomic::Ordering::SeqCst;
        // One atomic step, so two concurrent gets never both take the same scripted answer.
        if self
            .gets_not_found
            .try_update(SeqCst, SeqCst, |left| left.checked_sub(1))
            .is_ok()
        {
            return Err(persia_storage::Error::NotFound { key: key.clone() });
        }
        self.inner.get(key)
    }
    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.inner.put(key, data)
    }
    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        if self.unsupported {
            return Err(persia_storage::Error::Unsupported {
                op: Op::PutIfAbsent,
            });
        }
        if let Some(answer) = self.puts.lock().unwrap().pop_front() {
            return Ok(answer);
        }
        self.inner.put_if_absent(key, data)
    }
    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        self.inner.list(prefix)
    }
    fn delete(&self, key: &Key) -> Result<()> {
        self.inner.delete(key)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            conditional_put: true,
            ..self.inner.capabilities()
        }
    }
}
