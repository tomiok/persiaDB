//! A `BlobStore` that wraps `MemoryBlobStore`, records every call in order, injects failures, and checks the pin set
//! at each write: the failure-injection layer of the `put_blob` and `put(doc, blob)` tests.
#![allow(dead_code, clippy::cast_possible_truncation, clippy::indexing_slicing)] // Each test binary uses part of it.

use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use persia::internal::pins::{PinSet, Pinned};
use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobStore, Error, MemoryBlobStore, Result, UploadId,
};

/// A call the store received, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `stage_chunk` (or one chunk of `stage_chunks`), with whether its upload id was pinned then.
    Stage {
        upload: UploadId,
        index: u32,
        pinned: bool,
    },
    /// `stage_chunks` of this many chunks.
    StageMany {
        upload: UploadId,
        count: usize,
    },
    /// `bind` or `bind_small`, with whether the upload id and the blob id were pinned then.
    Bind {
        blob: BlobId,
        upload: UploadId,
        small: bool,
        pinned: (bool, bool),
    },
    Discard(UploadId),
    Manifest(BlobId),
    ReadChunks(BlobId, Range<u32>),
}

/// What to break.
#[derive(Debug, Default, Clone)]
pub struct Faults {
    /// The next this many binds fail with an unknown outcome.
    pub unknown_binds: u32,
    /// Whether a bind whose outcome is reported unknown binds all the same (the write landed).
    pub unknown_lands: bool,
    /// The chunk write (counted from 0, over every upload) that fails with a store error.
    pub fail_stage: Option<usize>,
    /// The store call (counted from 0, over `stage`, `bind`, `manifest`) that fails, and every one after it: a
    /// crashed store.
    pub crash_at: Option<usize>,
}

#[derive(Debug, Default)]
struct State {
    calls: Vec<Call>,
    faults: Faults,
    stages: usize,
    steps: usize,
}

/// The recording, failing store.
#[derive(Debug, Default)]
pub struct FaultyStore {
    pub inner: MemoryBlobStore,
    state: Mutex<State>,
    pins: OnceLock<Arc<PinSet>>,
}

fn failed(what: &str) -> Error {
    Error::Read(std::io::Error::other(what.to_owned()))
}

impl FaultyStore {
    pub fn new(faults: Faults) -> Arc<Self> {
        let store = Self::default();
        store.lock().faults = faults;
        Arc::new(store)
    }

    /// The pin set to check at each write (set once the `Blobs` over this store exists).
    pub fn watch(&self, pins: &Arc<PinSet>) {
        let _ = self.pins.set(Arc::clone(pins));
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    pub fn set_faults(&self, faults: Faults) {
        let mut state = self.lock();
        state.faults = faults;
        state.stages = 0;
        state.steps = 0;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pinned(&self, id: Pinned) -> bool {
        self.pins.get().is_some_and(|p| p.is_pinned(id))
    }

    /// Counts a store step; `Err` once the store has crashed.
    fn step(&self) -> Result<()> {
        let mut state = self.lock();
        let step = state.steps;
        state.steps += 1;
        if state.faults.crash_at.is_some_and(|at| step >= at) {
            return Err(failed("the store crashed"));
        }
        Ok(())
    }

    fn bind_with(
        &self,
        manifest: &BlobManifest,
        small: bool,
        bind: impl FnOnce() -> Result<BindOutcome>,
    ) -> Result<BindOutcome> {
        let pinned = (
            self.pinned(Pinned::Upload(manifest.upload_id())),
            self.pinned(Pinned::Blob(manifest.blob_id())),
        );
        self.lock().calls.push(Call::Bind {
            blob: manifest.blob_id(),
            upload: manifest.upload_id(),
            small,
            pinned,
        });
        self.step()?;
        let unknown = {
            let mut state = self.lock();
            let unknown = state.faults.unknown_binds > 0;
            if unknown {
                state.faults.unknown_binds -= 1;
            }
            unknown.then_some(state.faults.unknown_lands)
        };
        match unknown {
            None => bind(),
            Some(lands) => {
                if lands {
                    let _ = bind()?;
                }
                Err(Error::BindOutcomeUnknown("injected".into()))
            }
        }
    }
}

impl BlobStore for FaultyStore {
    fn stage_chunk(&self, upload_id: UploadId, index: u32, bytes: &[u8]) -> Result<()> {
        let pinned = self.pinned(Pinned::Upload(upload_id));
        {
            let mut state = self.lock();
            state.calls.push(Call::Stage {
                upload: upload_id,
                index,
                pinned,
            });
            let n = state.stages;
            state.stages += 1;
            if state.faults.fail_stage == Some(n) {
                return Err(failed("injected stage failure"));
            }
        }
        self.step()?;
        self.inner.stage_chunk(upload_id, index, bytes)
    }

    fn stage_chunks(&self, upload_id: UploadId, first_index: u32, chunks: &[&[u8]]) -> Result<()> {
        self.lock().calls.push(Call::StageMany {
            upload: upload_id,
            count: chunks.len(),
        });
        for (i, bytes) in chunks.iter().enumerate() {
            self.stage_chunk(upload_id, first_index + i as u32, bytes)?;
        }
        Ok(())
    }

    fn bind(&self, manifest: &BlobManifest) -> Result<BindOutcome> {
        self.bind_with(manifest, false, || self.inner.bind(manifest))
    }

    fn bind_small(&self, manifest: &BlobManifest, bytes: &[u8]) -> Result<BindOutcome> {
        self.bind_with(manifest, true, || self.inner.bind_small(manifest, bytes))
    }

    fn manifest(&self, blob_id: BlobId) -> Result<Option<BlobManifest>> {
        self.lock().calls.push(Call::Manifest(blob_id));
        self.step()?;
        self.inner.manifest(blob_id)
    }

    fn read_chunk(&self, manifest: &BlobManifest, index: u32) -> Result<Vec<u8>> {
        self.inner.read_chunk(manifest, index)
    }

    fn read_chunks(&self, manifest: &BlobManifest, indices: Range<u32>) -> Result<Vec<Vec<u8>>> {
        self.lock()
            .calls
            .push(Call::ReadChunks(manifest.blob_id(), indices.clone()));
        self.inner.read_chunks(manifest, indices)
    }

    fn discard_upload(&self, upload_id: UploadId) -> Result<()> {
        self.lock().calls.push(Call::Discard(upload_id));
        self.inner.discard_upload(upload_id)
    }
}
