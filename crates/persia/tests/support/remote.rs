//! A `BlobStore` over a `RemoteBlobStore` on a clocked `MemoryStorage` that records every call, each with whether the
//! pin set held the uploads or blob it names then, and fails the calls a test picks: the failure-injection layer of
//! the resumed `put_blob` tests (ROADMAP 9.3.4, 9.3.6). Faults are injected at the store, where the remote layout
//! reports them: a backend error is `Error::Storage`, a lost bind response `Error::BindOutcomeUnknown`.
#![allow(dead_code)] // Each test binary uses part of it.

use std::io;
use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use core::num::NonZeroUsize;

use persia::internal::pins::{PinSet, Pinned};
use persia_blob::remote::{RemoteBlobStore, ResumedUpload};
use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobStore, Error, Result, ResumeToken, UploadId,
};
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::{MemoryStorage, Op};

/// The clock's start, after the epoch.
pub const START: Duration = Duration::from_hours(1_000);

/// A call the store received, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `stage_chunk` (`stage_chunks` is recorded chunk by chunk), with whether its upload was pinned then.
    Stage {
        upload: UploadId,
        index: u32,
        pinned: bool,
    },
    /// `resume` (the listing), with whether the token's upload was pinned then.
    Resume { upload: UploadId, pinned: bool },
    /// `restage` of `count` chunks, with whether the upload copied to was pinned then.
    Restage {
        from: UploadId,
        count: usize,
        to: UploadId,
        pinned: bool,
    },
    /// `bind` or `bind_small`, with whether the upload and the blob were pinned then.
    Bind {
        blob: BlobId,
        upload: UploadId,
        pinned: (bool, bool),
    },
    /// `manifest`.
    Manifest(BlobId),
    /// `discard_upload`.
    Discard(UploadId),
}

/// What an injected fault does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Nothing is applied; the call fails with a backend error (`Error::Storage`).
    Fail,
    /// The call is applied, then fails with a backend error: its response was lost.
    Lost,
    /// `bind` only: the call is applied, then fails with an unknown outcome.
    Unknown,
}

type Picker = Box<dyn FnMut(&Event) -> Option<Fault> + Send>;

/// The recording, failing store.
pub struct Watched {
    inner: RemoteBlobStore<MemoryStorage>,
    clock: Arc<ManualClock>,
    pins: OnceLock<Arc<PinSet>>,
    events: Mutex<Vec<Event>>,
    pick: Mutex<Option<Picker>>,
}

impl std::fmt::Debug for Watched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watched").finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn injected() -> Error {
    Error::Storage(persia_storage::Error::Io {
        op: Op::PutIfAbsent,
        target: "injected".to_owned(),
        source: io::Error::other("injected"),
    })
}

impl Watched {
    /// An empty store, one request at a time, whose clock reads [`START`] after the epoch.
    pub fn new() -> Arc<Self> {
        let clock = Arc::new(ManualClock::new(UNIX_EPOCH + START));
        let storage = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        Arc::new(Self {
            inner: RemoteBlobStore::new(storage).with_parallelism(NonZeroUsize::MIN),
            clock,
            pins: OnceLock::new(),
            events: Mutex::new(Vec::new()),
            pick: Mutex::new(None),
        })
    }

    /// The pin set to check at each call (set once the `Blobs` over this store exists).
    pub fn watch(&self, pins: &Arc<PinSet>) {
        assert!(self.pins.set(Arc::clone(pins)).is_ok(), "watched twice");
    }

    /// The remote store, past the recording and the faults (a test's setup, another writer, GC's sweep).
    pub fn inner(&self) -> &RemoteBlobStore<MemoryStorage> {
        &self.inner
    }

    pub fn clock(&self) -> &ManualClock {
        &self.clock
    }

    pub fn now(&self) -> SystemTime {
        self.clock.now()
    }

    /// Every call since the last [`Watched::clear`], in order.
    pub fn events(&self) -> Vec<Event> {
        lock(&self.events).clone()
    }

    pub fn clear(&self) {
        lock(&self.events).clear();
    }

    /// Fails each call `pick` returns a fault for, from now on (replacing the previous picker).
    pub fn fail_when(&self, pick: impl FnMut(&Event) -> Option<Fault> + Send + 'static) {
        *lock(&self.pick) = Some(Box::new(pick));
    }

    /// Fails nothing from now on.
    pub fn heal(&self) {
        *lock(&self.pick) = None;
    }

    /// The chunk indices staged under `upload`, as listed.
    pub fn chunks(&self, upload: UploadId) -> Vec<u32> {
        self.inner
            .staged_chunks(upload)
            .unwrap()
            .iter()
            .map(|c| c.index)
            .collect()
    }

    fn pinned(&self, id: Pinned) -> bool {
        self.pins.get().is_some_and(|p| p.is_pinned(id))
    }

    /// Records `event`, then runs `apply` unless the picker fails the call first, or fails it after it ran.
    fn call<T>(&self, event: Event, apply: impl FnOnce() -> Result<T>) -> Result<T> {
        let fault = lock(&self.pick).as_mut().and_then(|pick| pick(&event));
        lock(&self.events).push(event);
        match fault {
            None => apply(),
            Some(Fault::Fail) => Err(injected()),
            Some(Fault::Lost) => {
                apply()?;
                Err(injected())
            }
            Some(Fault::Unknown) => {
                apply()?;
                Err(Error::BindOutcomeUnknown("injected".into()))
            }
        }
    }

    fn bind_event(&self, manifest: &BlobManifest) -> Event {
        Event::Bind {
            blob: manifest.blob_id(),
            upload: manifest.upload_id(),
            pinned: (
                self.pinned(Pinned::Upload(manifest.upload_id())),
                self.pinned(Pinned::Blob(manifest.blob_id())),
            ),
        }
    }
}

impl BlobStore for Watched {
    fn stage_chunk(&self, upload_id: UploadId, index: u32, bytes: &[u8]) -> Result<()> {
        let event = Event::Stage {
            upload: upload_id,
            index,
            pinned: self.pinned(Pinned::Upload(upload_id)),
        };
        self.call(event, || self.inner.stage_chunk(upload_id, index, bytes))
    }

    fn stage_chunks(&self, upload_id: UploadId, first_index: u32, chunks: &[&[u8]]) -> Result<()> {
        for (i, bytes) in (first_index..).zip(chunks) {
            self.stage_chunk(upload_id, i, bytes)?;
        }
        Ok(())
    }

    fn bind(&self, manifest: &BlobManifest) -> Result<BindOutcome> {
        self.call(self.bind_event(manifest), || self.inner.bind(manifest))
    }

    fn bind_small(&self, manifest: &BlobManifest, bytes: &[u8]) -> Result<BindOutcome> {
        self.call(self.bind_event(manifest), || {
            self.inner.bind_small(manifest, bytes)
        })
    }

    fn manifest(&self, blob_id: BlobId) -> Result<Option<BlobManifest>> {
        self.call(Event::Manifest(blob_id), || self.inner.manifest(blob_id))
    }

    fn read_chunk(&self, manifest: &BlobManifest, index: u32) -> Result<Vec<u8>> {
        self.inner.read_chunk(manifest, index)
    }

    fn read_chunks(&self, manifest: &BlobManifest, indices: Range<u32>) -> Result<Vec<Vec<u8>>> {
        self.inner.read_chunks(manifest, indices)
    }

    fn discard_upload(&self, upload_id: UploadId) -> Result<()> {
        self.call(Event::Discard(upload_id), || {
            self.inner.discard_upload(upload_id)
        })
    }

    fn resumable(&self) -> bool {
        self.inner.resumable()
    }

    fn resume(&self, token: ResumeToken) -> Result<ResumedUpload> {
        let event = Event::Resume {
            upload: token.upload_id,
            pinned: self.pinned(Pinned::Upload(token.upload_id)),
        };
        self.call(event, || BlobStore::resume(&self.inner, token))
    }

    fn restage(&self, from: ResumeToken, chunk_crcs: &[u32], to: UploadId) -> Result<()> {
        let event = Event::Restage {
            from: from.upload_id,
            count: chunk_crcs.len(),
            to,
            pinned: self.pinned(Pinned::Upload(to)),
        };
        self.call(event, || {
            BlobStore::restage(&self.inner, from, chunk_crcs, to)
        })
    }
}
