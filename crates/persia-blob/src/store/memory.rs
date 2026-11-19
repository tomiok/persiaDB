//! An in-memory [`BlobStore`] for tests (feature `testing`): what code above the trait (`put_blob`, the pin set,
//! GC) is tested against before, and beside, the pack file and the remote layout.
//!
//! It keeps manifests encoded and decodes them on every read, so the codec's checks run as they would on stored
//! bytes. Nothing it holds survives the value, so it says nothing about crash safety.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use persia_format::BlobId;

use super::{BindOutcome, BlobStore};
use crate::error::{Error, Result};
use crate::manifest::{BlobManifest, UploadId};

/// An in-memory blob store (see the module docs).
#[derive(Debug, Default)]
pub struct MemoryBlobStore {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    chunks: BTreeMap<(UploadId, u32), Vec<u8>>,
    manifests: BTreeMap<BlobId, Vec<u8>>,
}

impl MemoryBlobStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many chunks are staged under `upload_id`.
    pub fn staged_chunk_count(&self, upload_id: UploadId) -> usize {
        self.lock()
            .chunks
            .keys()
            .filter(|(upload, _)| *upload == upload_id)
            .count()
    }

    /// Replaces the bytes staged as chunk `index` of `upload_id`, which no [`BlobStore`] method does: how a test
    /// stands for storage that damaged a chunk after it was bound, to exercise the read-side checks.
    pub fn damage_staged_chunk(&self, upload_id: UploadId, index: u32, bytes: &[u8]) {
        self.lock()
            .chunks
            .insert((upload_id, index), bytes.to_vec());
    }

    /// The ids bound so far, in id order.
    pub fn bound_ids(&self) -> Vec<BlobId> {
        self.lock().manifests.keys().copied().collect()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves the maps consistent: every change is one insert or one retain.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl BlobStore for MemoryBlobStore {
    fn stage_chunk(&self, upload_id: UploadId, index: u32, bytes: &[u8]) -> Result<()> {
        let mut inner = self.lock();
        match inner.chunks.get(&(upload_id, index)) {
            Some(staged) if staged.as_slice() == bytes => Ok(()),
            Some(_) => Err(Error::StagedChunkConflict { upload_id, index }),
            None => {
                inner.chunks.insert((upload_id, index), bytes.to_vec());
                Ok(())
            }
        }
    }

    fn bind(&self, manifest: &BlobManifest) -> Result<BindOutcome> {
        let mut inner = self.lock();
        if let Some(bound) = inner.manifests.get(&manifest.blob_id()) {
            let bound = BlobManifest::decode(bound)?;
            bound.check_blob_id(manifest.blob_id())?;
            return Ok(BindOutcome::AlreadyBound(bound));
        }
        // As the pack and the remote layout do (SPEC §9.2: a manifest is written only after every chunk it names was
        // staged), so code above the trait cannot pass here by binding what it never staged.
        for index in 0..manifest.chunk_count() {
            let staged = inner.chunks.get(&(manifest.upload_id(), index));
            let expected = manifest.chunk_range(index).zip(
                manifest
                    .chunk_crcs()
                    .get(persia_format::widen::usize_from_u32(index)),
            );
            let matches = match (staged, expected) {
                (Some(bytes), Some((range, &crc))) => {
                    range.end.checked_sub(range.start)
                        == Some(persia_format::widen::u64_from_usize(bytes.len()))
                        && persia_format::checksum::crc32c(bytes) == crc
                }
                _ => false,
            };
            if !matches {
                return Err(Error::InvalidManifest {
                    what: "a chunk the manifest names is not staged as it describes",
                });
            }
        }
        inner
            .manifests
            .insert(manifest.blob_id(), manifest.encode());
        Ok(BindOutcome::Bound)
    }

    fn manifest(&self, blob_id: BlobId) -> Result<Option<BlobManifest>> {
        let Some(bytes) = self.lock().manifests.get(&blob_id).cloned() else {
            return Ok(None);
        };
        let manifest = BlobManifest::decode(&bytes)?;
        manifest.check_blob_id(blob_id)?;
        Ok(Some(manifest))
    }

    fn read_chunk(&self, manifest: &BlobManifest, index: u32) -> Result<Vec<u8>> {
        self.lock()
            .chunks
            .get(&(manifest.upload_id(), index))
            .cloned()
            .ok_or(Error::Corrupt {
                encoding: super::CHUNK,
                what: "a chunk the manifest names is not staged",
            })
    }

    fn discard_upload(&self, upload_id: UploadId) -> Result<()> {
        self.lock()
            .chunks
            .retain(|(upload, _), _| *upload != upload_id);
        Ok(())
    }
}
