//! The remote layout (SPEC §9.4, ADR-0050): a [`BlobStore`] over any [`Storage`] with `conditional_put` (S3, GCS,
//! Azure, the local backend, memory).
//!
//! Staged chunks are the raw chunk bytes at `blobs/uploads/<upload_id>/<chunk_idx>`, blob manifests are at
//! `blobs/<xx>/<id>` ([`keys`]); both are created with `put_if_absent` and never overwritten. A read gets the blob
//! manifest, checks it names the id it was looked up by, and the callers ([`read_range`](crate::read_range)) check
//! it against the `BlobRef` and every chunk's length and CRC32C against it: every remote object is hostile input
//! (SPEC §17).
//!
//! ```
//! use persia_blob::remote::RemoteBlobStore;
//! use persia_blob::{BindOutcome, BlobManifest, BlobStore, ChunkSize, UploadId, blob_id_of, read_range};
//! use persia_format::BlobRef;
//! use persia_format::checksum::crc32c;
//! use persia_storage::MemoryStorage;
//!
//! let store = RemoteBlobStore::new(MemoryStorage::new());
//! let data = b"remote bytes";
//! let upload = UploadId::new(42);
//! store.stage_chunk(upload, 0, data)?;
//! let m = BlobManifest::new(blob_id_of(data), 12, ChunkSize::DEFAULT, upload, vec![crc32c(data)])?;
//! assert_eq!(store.bind(&m)?, BindOutcome::Bound);
//! let r = BlobRef::new(m.blob_id(), 12, "text/plain", m.chunk_size().get(), Default::default())?;
//! assert_eq!(read_range(&store, &r, 7..12)?, b"bytes");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod keys;
mod parallel;
mod resume;

use core::num::NonZeroUsize;
use core::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use persia_format::BlobId;
use persia_format::widen::usize_from_u32;
use persia_storage::{ErrorKind, Key, PutOutcome, Storage};

use crate::error::{Error, Result};
use crate::manifest::{BlobManifest, UploadId};
use crate::store::{BindOutcome, BlobStore};

pub use keys::RemoteKey;
pub use resume::{ResumeToken, ResumedUpload};

/// The encoding named by errors about a chunk object.
const CHUNK: &str = "blob chunk";
/// The encoding named by errors a backend reports as corruption.
const OBJECT: &str = "blob storage object";
/// How often `bind` writes the manifest again when the bound manifest it was told about is gone before it reads it
/// (deleted by a sweep in between): the outcome is then known (not bound by us), so this is not a blind retry.
const BIND_ATTEMPTS: u32 = 3;
/// Requests a store keeps in flight in a parallel transfer unless told otherwise ([`RemoteBlobStore::with_parallelism`]).
pub const DEFAULT_PARALLELISM: NonZeroUsize = match NonZeroUsize::new(8) {
    Some(n) => n,
    None => NonZeroUsize::MIN,
};

/// A [`BlobStore`] in the remote layout over a [`Storage`] (see the module docs).
pub struct RemoteBlobStore<S = Arc<dyn Storage>> {
    storage: S,
    parallelism: NonZeroUsize,
}

impl<S> core::fmt::Debug for RemoteBlobStore<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RemoteBlobStore")
            .field("parallelism", &self.parallelism)
            .finish_non_exhaustive()
    }
}

impl<S: Storage> RemoteBlobStore<S> {
    /// A store over `storage`, whose root holds `blobs/` (SPEC §10.4). Reads work on any backend; staging and binding
    /// need [`Capabilities::conditional_put`](persia_storage::Capabilities::conditional_put).
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            parallelism: DEFAULT_PARALLELISM,
        }
    }

    /// The same store with at most `parallelism` requests in flight in [`BlobStore::stage_chunks`],
    /// [`BlobStore::read_chunks`], [`RemoteBlobStore::resume`] and [`RemoteBlobStore::restage`] (ROADMAP 9.3.2), one
    /// per thread; 1 runs them in the caller's thread, one at a time.
    #[must_use]
    pub fn with_parallelism(mut self, parallelism: NonZeroUsize) -> Self {
        self.parallelism = parallelism;
        self
    }

    /// The most requests a parallel transfer keeps in flight.
    pub const fn parallelism(&self) -> NonZeroUsize {
        self.parallelism
    }

    /// The backend.
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// The chunks staged under `upload_id`, by index, with their lengths. Keys under the upload's prefix that Persia
    /// did not write are skipped.
    ///
    /// # Errors
    /// The backend's errors.
    pub fn staged_chunks(&self, upload_id: UploadId) -> Result<Vec<StagedChunk>> {
        let listed = self
            .storage
            .list(&keys::upload_prefix(upload_id)?)
            .map_err(storage)?;
        // Sorted by key bytes, and the index is zero-padded, so the chunks come in index order.
        Ok(listed
            .into_iter()
            .filter_map(|meta| match RemoteKey::parse(&meta.key)? {
                RemoteKey::Chunk {
                    upload_id: u,
                    index,
                } if u == upload_id => Some(StagedChunk {
                    index,
                    size: meta.size,
                }),
                _ => None,
            })
            .collect())
    }

    fn require_conditional_put(&self) -> Result<()> {
        if self.storage.capabilities().conditional_put {
            Ok(())
        } else {
            Err(Error::ConditionalPutUnsupported)
        }
    }

    fn get(&self, key: &Key) -> Result<Option<Bytes>> {
        match self.storage.get(key) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(persia_storage::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(storage(e)),
        }
    }
}

/// One chunk object staged under an upload, as listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StagedChunk {
    /// The chunk's index.
    pub index: u32,
    /// The object's length in bytes, as the listing reports it.
    pub size: u64,
}

impl<S: Storage> BlobStore for RemoteBlobStore<S> {
    fn stage_chunk(&self, upload_id: UploadId, index: u32, bytes: &[u8]) -> Result<()> {
        self.require_conditional_put()?;
        let key = RemoteKey::Chunk { upload_id, index }.key()?;
        // The backend resolved `AlreadyExists` against these bytes (SPEC §10.1): it means other bytes are there.
        match self
            .storage
            .put_if_absent(&key, Bytes::copy_from_slice(bytes))
        {
            Ok(PutOutcome::Created) => Ok(()),
            Ok(PutOutcome::AlreadyExists) => Err(Error::StagedChunkConflict { upload_id, index }),
            Err(e) if e.kind() == ErrorKind::Unsupported => Err(Error::ConditionalPutUnsupported),
            Err(e) => Err(storage(e)),
        }
    }

    fn stage_chunks(&self, upload_id: UploadId, first_index: u32, chunks: &[&[u8]]) -> Result<()> {
        parallel::run(chunks.len(), self.parallelism, |i| {
            let index = u32::try_from(i)
                .ok()
                .and_then(|i| first_index.checked_add(i))
                .ok_or(Error::InvalidManifest {
                    what: "more than u32::MAX chunks",
                })?;
            let bytes = chunks.get(i).copied().unwrap_or_default();
            self.stage_chunk(upload_id, index, bytes)
        })
        .map(drop)
    }

    fn read_chunks(&self, manifest: &BlobManifest, indices: Range<u32>) -> Result<Vec<Vec<u8>>> {
        let count = usize_from_u32(indices.end.saturating_sub(indices.start));
        parallel::run(count, self.parallelism, |i| {
            let index = u32::try_from(i)
                .ok()
                .and_then(|i| indices.start.checked_add(i))
                .ok_or_else(|| corrupt("a chunk index does not fit in a u32"))?;
            self.read_chunk(manifest, index)
        })
    }

    fn bind(&self, manifest: &BlobManifest) -> Result<BindOutcome> {
        self.require_conditional_put()?;
        let key = RemoteKey::Manifest(manifest.blob_id()).key()?;
        let bytes = Bytes::from(manifest.encode());
        let mut gone = None;
        for _ in 0..BIND_ATTEMPTS {
            match self.storage.put_if_absent(&key, bytes.clone()) {
                Ok(PutOutcome::Created) => return Ok(BindOutcome::Bound),
                Ok(PutOutcome::AlreadyExists) => match self.storage.get(&key) {
                    Ok(found) => {
                        let bound = BlobManifest::decode(&found)?;
                        bound.check_blob_id(manifest.blob_id())?;
                        return Ok(BindOutcome::AlreadyBound(bound));
                    }
                    Err(e @ persia_storage::Error::NotFound { .. }) => gone = Some(e),
                    Err(e) => return Err(storage(e)),
                },
                // Nothing was written: the backend refuses conditional writes outright.
                Err(e) if e.kind() == ErrorKind::Unsupported => {
                    return Err(Error::ConditionalPutUnsupported);
                }
                // SPEC §9.4: the write may still land; the caller binds again and keeps the chunks.
                Err(e) => return Err(Error::BindOutcomeUnknown(Box::new(e))),
            }
        }
        Err(gone.map_or(Error::ConditionalPutUnsupported, storage))
    }

    fn manifest(&self, blob_id: BlobId) -> Result<Option<BlobManifest>> {
        let Some(bytes) = self.get(&RemoteKey::Manifest(blob_id).key()?)? else {
            return Ok(None);
        };
        let manifest = BlobManifest::decode(&bytes)?;
        manifest.check_blob_id(blob_id)?;
        Ok(Some(manifest))
    }

    fn read_chunk(&self, manifest: &BlobManifest, index: u32) -> Result<Vec<u8>> {
        if index >= manifest.chunk_count() {
            return Err(corrupt("a chunk past the blob's last one was asked for"));
        }
        let key = RemoteKey::Chunk {
            upload_id: manifest.upload_id(),
            index,
        }
        .key()?;
        match self.get(&key)? {
            Some(bytes) => Ok(bytes.to_vec()),
            None => Err(corrupt("a chunk the manifest names is not staged")),
        }
    }

    fn discard_upload(&self, upload_id: UploadId) -> Result<()> {
        // Best-effort: every chunk is tried, the first error is reported (GC is the backstop).
        let mut first = None;
        for chunk in self.staged_chunks(upload_id)? {
            let deleted = RemoteKey::Chunk {
                upload_id,
                index: chunk.index,
            }
            .key()
            .and_then(|key| self.storage.delete(&key).map_err(storage));
            if let Err(e) = deleted {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }
}

/// A backend error as this crate reports it: corruption the backend detected stays corruption.
fn storage(e: persia_storage::Error) -> Error {
    if e.is_corruption() {
        Error::Corrupt {
            encoding: OBJECT,
            what: "the storage backend reported a corrupt object",
        }
    } else {
        Error::Storage(e)
    }
}

const fn corrupt(what: &'static str) -> Error {
    Error::Corrupt {
        encoding: CHUNK,
        what,
    }
}
