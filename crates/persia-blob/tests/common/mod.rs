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
