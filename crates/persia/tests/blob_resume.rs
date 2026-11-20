//! Resumed uploads through `put_blob` (SPEC §9.4, §9.5; ROADMAP 9.3.4, 9.3.6; ADR-0050, ADR-0053), over the remote layout
//! on a `MemoryStorage`, with faults injected at the store.
//!
//! Invariants:
//! 1. A `put_blob` on the remote layout that fails after staging (a store error, the reader) fails with
//!    `UploadInterrupted`; resumed with its token and the whole blob, it binds the `BlobRef` an uninterrupted
//!    `put_blob` binds, stages only the chunks after the kept run, under the same upload, and reads back.
//! 2. Not wrapped: a reader that fails before anything is staged, a bind whose outcome is unknown, a caller's error
//!    (a reader that is not the staged blob), and every error of a store that cannot resume.
//! 3. A chunk staged with other bytes restarts the upload under a new upload id, pinned before the copy; a resume
//!    that fails then returns the new upload's token, which resumes.
//! 4. The token's upload is pinned before it is listed, every chunk write is pinned, and every pin drops with the
//!    error or the returned guard.
//! 5. A token chunk size below the floor fails before reading, and the token's chunk size wins over the options'.
//! 6. A store that cannot resume fails a resume with `ResumeUnsupported` before reading.
//! 7. A resume that writes no chunk its listing did not show, of an upload older than the grace period, moves the
//!    upload to a new upload id before binding: a sweep that listed the manifests and uploads before the bind, and
//!    checks the old upload once the resume's pins dropped, deletes the old upload and keeps the blob (ROADMAP 9.3.6).
//! 8. A resume that writes a new chunk before the sweep lists the uploads keeps its upload: it is young.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::assert_is_empty
)] // Tests: inputs are fixed.

#[path = "support/remote.rs"]
mod remote;

use core::num::NonZeroUsize;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::BlobDatabase;
use persia::internal::gc::DEFAULT_GRACE;
use persia::internal::pins::{PinSet, Pinned};
use persia::internal::retry::ManualRetryClock;
use persia_blob::gc::{GcObject, PinCheck, PinGate, Sweep, SweepOptions};
use persia_blob::remote::{RemoteBlobStore, RemoteKey};
use persia_blob::testing::{CorruptGet, ListOrder, MisorderedList};
use persia_blob::{BlobRef, BlobStore, ChunkSize, Error, MemoryBlobStore, ResumeToken, UploadId};
use persia_engine::db::{Database, DatabaseOptions, SegmentIds};
use persia_storage::MemoryStorage;
use persia_storage::retry::{Clock, ManualClock};
use remote::{Event, Fault, Watched};

const CHUNK: u32 = ChunkSize::MIN;
const CHUNK_LEN: usize = CHUNK as usize;

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ ((i >> 13) as u8))
        .collect()
}

fn options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(CHUNK).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn resume(token: ResumeToken) -> PutBlobOptions {
    PutBlobOptions {
        resume: Some(token),
        ..options()
    }
}

fn remote() -> (Arc<Watched>, Blobs<Watched>) {
    let store = Watched::new();
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(5))
        .with_retry_clock(Arc::new(ManualRetryClock::new()));
    store.watch(blobs.pin_set());
    (store, blobs)
}

/// The reference an uninterrupted `put_blob` returns, on a store of its own.
fn uninterrupted(data: &[u8]) -> BlobRef {
    remote().1.put_blob(data, &options()).unwrap().0
}

fn interruption(e: Error) -> (ResumeToken, Error) {
    match e {
        Error::UploadInterrupted { token, source } => (token, *source),
        other => panic!("not an interrupted upload: {other:?}"),
    }
}

/// Fails the first call `matches` picks, with `fault`; later ones pass.
fn once(
    fault: Fault,
    matches: impl Fn(&Event) -> bool + Send + 'static,
) -> impl FnMut(&Event) -> Option<Fault> + Send + 'static {
    let mut fired = false;
    move |event| {
        if !fired && matches(event) {
            fired = true;
            Some(fault)
        } else {
            None
        }
    }
}

fn stage_of(index: u32) -> impl Fn(&Event) -> bool + Send + 'static {
    move |e| matches!(e, Event::Stage { index: i, .. } if *i == index)
}

/// The chunk writes in `events`: `(upload, index)`, each checked to have been pinned.
fn stages(events: &[Event]) -> Vec<(UploadId, u32)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Stage {
                upload,
                index,
                pinned,
            } => {
                assert!(pinned, "chunk {index} of {upload} written unpinned");
                Some((*upload, *index))
            }
            _ => None,
        })
        .collect()
}

fn upload_of(blobs: &Blobs<Watched>, r: &BlobRef) -> UploadId {
    blobs
        .store()
        .inner()
        .manifest(r.blob_id())
        .unwrap()
        .unwrap()
        .upload_id()
}

/// A reader of `data` that fails once it has yielded `fail_at` bytes.
struct FailAfter<'a> {
    data: &'a [u8],
    pos: usize,
    fail_at: usize,
}

impl Read for FailAfter<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.fail_at {
            return Err(io::Error::other("the reader failed"));
        }
        let end = self.fail_at.min(self.data.len()).min(self.pos + out.len());
        let n = end - self.pos;
        out[..n].copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(n)
    }
}

/// A reader that must not be read.
struct Unread;

impl Read for Unread {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("the reader was read");
    }
}

#[test]
fn an_interrupted_upload_resumes_to_the_blob_an_uninterrupted_one_binds() {
    // Five full chunks and a short one, cut at every chunk write.
    for fail_at in 0..6_u32 {
        let (store, blobs) = remote();
        let data = bytes(5 * CHUNK_LEN + 100, fail_at as u8);
        store.fail_when(once(Fault::Fail, stage_of(fail_at)));
        let err = blobs.put_blob(&data[..], &options()).unwrap_err();
        let (token, source) = interruption(err);
        assert!(matches!(source, Error::Storage(_)), "{source:?}");
        assert_eq!(token.chunk_size.get(), CHUNK);
        assert!(
            blobs.pin_set().pinned().is_empty(),
            "the pins drop with the error"
        );
        assert_eq!(
            store.chunks(token.upload_id),
            (0..fail_at).collect::<Vec<_>>()
        );

        store.clear();
        let (r, pins) = blobs.put_blob(&data[..], &resume(token)).unwrap();
        assert_eq!(r, uninterrupted(&data), "cut at chunk {fail_at}");
        assert_eq!(blobs.get_blob_range(&r, 0..r.size()).unwrap(), data);
        let events = store.events();
        assert_eq!(
            events.first(),
            Some(&Event::Resume {
                upload: token.upload_id,
                pinned: true
            }),
            "the token's upload is pinned before it is listed"
        );
        // Only the chunks after the kept run are written, under the same upload.
        let written: Vec<(UploadId, u32)> = (fail_at..6).map(|i| (token.upload_id, i)).collect();
        assert_eq!(stages(&events), written);
        assert_eq!(upload_of(&blobs, &r), token.upload_id);
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Bind {
                pinned: (true, true),
                ..
            }
        )));
        assert_eq!(
            pins.ids(),
            [Pinned::Upload(token.upload_id), Pinned::Blob(r.blob_id())]
        );
        drop(pins);
        assert!(blobs.pin_set().pinned().is_empty());
    }
}

#[test]
fn a_reader_that_fails_after_a_window_was_staged_interrupts_the_upload() {
    // Window of 64 chunks at 64 KiB: the first is staged before the reader fails inside chunk 64.
    let (store, blobs) = remote();
    let data = bytes(64 * CHUNK_LEN + 1000, 7);
    let reader = FailAfter {
        data: &data,
        pos: 0,
        fail_at: 64 * CHUNK_LEN + 500,
    };
    let (token, source) = interruption(blobs.put_blob(reader, &options()).unwrap_err());
    assert!(matches!(source, Error::Read(_)), "{source:?}");
    assert_eq!(store.chunks(token.upload_id).len(), 64);

    store.clear();
    let (r, _pins) = blobs.put_blob(&data[..], &resume(token)).unwrap();
    assert_eq!(r, uninterrupted(&data));
    assert_eq!(stages(&store.events()), [(token.upload_id, 64)]);
    assert_eq!(blobs.get_blob_range(&r, 0..r.size()).unwrap(), data);
}

#[test]
fn what_cannot_be_resumed_is_not_wrapped() {
    let (store, blobs) = remote();
    // The reader fails before any chunk write: large, then small (nothing is ever staged for a small blob).
    let data = bytes(5 * CHUNK_LEN, 1);
    for fail_at in [3 * CHUNK_LEN, 100] {
        let reader = FailAfter {
            data: &data,
            pos: 0,
            fail_at,
        };
        let err = blobs.put_blob(reader, &options()).unwrap_err();
        assert!(matches!(err, Error::Read(_)), "{err:?}");
    }
    assert!(stages(&store.events()).is_empty());

    // A bind whose outcome is unknown: the blob may be bound later.
    store.fail_when(|e| matches!(e, Event::Bind { .. }).then_some(Fault::Unknown));
    let zero = PutBlobOptions {
        bind_budget: Duration::ZERO,
        ..options()
    };
    let err = blobs.put_blob(&data[..], &zero).unwrap_err();
    assert!(err.is_bind_outcome_unknown(), "{err:?}");

    // A store that cannot resume never wraps: its errors are its own.
    let memory = Blobs::new(Arc::new(MemoryBlobStore::new()));
    let reader = FailAfter {
        data: &bytes(70 * CHUNK_LEN, 2),
        pos: 0,
        fail_at: 65 * CHUNK_LEN,
    };
    let err = memory.put_blob(reader, &options()).unwrap_err();
    assert!(matches!(err, Error::Read(_)), "{err:?}");
}

#[test]
fn a_bind_that_failed_with_a_known_outcome_resumes() {
    let (store, blobs) = remote();
    let data = bytes(3 * CHUNK_LEN + 9, 3);
    store.fail_when(once(Fault::Fail, |e| matches!(e, Event::Bind { .. })));
    let (token, source) = interruption(blobs.put_blob(&data[..], &options()).unwrap_err());
    assert!(matches!(source, Error::Storage(_)), "{source:?}");
    assert_eq!(store.chunks(token.upload_id), [0, 1, 2, 3]);
    let (r, _pins) = blobs.put_blob(&data[..], &resume(token)).unwrap();
    assert_eq!(r, uninterrupted(&data));
    assert_eq!(blobs.get_blob_range(&r, 0..r.size()).unwrap(), data);
}

/// Chunks 0 and 1 staged, then chunk 3 staged by someone else with other bytes: the resume conflicts on it.
fn conflicted(store: &Watched, blobs: &Blobs<Watched>, data: &[u8]) -> ResumeToken {
    store.fail_when(once(Fault::Fail, stage_of(2)));
    let (token, _) = interruption(blobs.put_blob(data, &options()).unwrap_err());
    store
        .inner()
        .stage_chunk(token.upload_id, 3, &bytes(CHUNK_LEN, 99))
        .unwrap();
    store.clear();
    token
}

#[test]
fn a_conflicting_chunk_restarts_under_a_new_upload_pinned_before_the_copy() {
    let (store, blobs) = remote();
    let data = bytes(5 * CHUNK_LEN + 100, 4);
    let token = conflicted(&store, &blobs, &data);
    let (r, pins) = blobs.put_blob(&data[..], &resume(token)).unwrap();
    assert_eq!(r, uninterrupted(&data));
    assert_eq!(blobs.get_blob_range(&r, 0..r.size()).unwrap(), data);
    let moved = upload_of(&blobs, &r);
    assert_ne!(moved, token.upload_id);

    let events = store.events();
    let restage = events
        .iter()
        .find(|e| matches!(e, Event::Restage { .. }))
        .unwrap();
    assert_eq!(
        restage,
        &Event::Restage {
            from: token.upload_id,
            count: 2,
            to: moved,
            pinned: true
        },
        "the chunks before the conflicting window are copied to a pinned upload"
    );
    // Chunk 2 lands under the token's upload, 3 conflicts there; then the window goes to the new upload.
    let mut expected = vec![(token.upload_id, 2), (token.upload_id, 3)];
    expected.extend((2..6).map(|i| (moved, i)));
    assert_eq!(stages(&events), expected);
    assert_eq!(store.chunks(moved), (0..6).collect::<Vec<_>>());
    assert_eq!(
        pins.ids(),
        [
            Pinned::Upload(token.upload_id),
            Pinned::Upload(moved),
            Pinned::Blob(r.blob_id())
        ]
    );
}

#[test]
fn a_resume_that_fails_again_returns_the_upload_it_was_staging() {
    let (store, blobs) = remote();
    let data = bytes(5 * CHUNK_LEN + 100, 5);
    let token = conflicted(&store, &blobs, &data);
    // After the restage, the write of chunk 4 under the new upload fails.
    let first = token.upload_id;
    store.fail_when(once(
        Fault::Fail,
        move |e| matches!(e, Event::Stage { upload, index: 4, .. } if *upload != first),
    ));
    let (again, source) = interruption(blobs.put_blob(&data[..], &resume(token)).unwrap_err());
    assert!(matches!(source, Error::Storage(_)), "{source:?}");
    assert_ne!(again.upload_id, token.upload_id);
    assert_eq!(again.chunk_size, token.chunk_size);
    assert_eq!(store.chunks(again.upload_id), [0, 1, 2, 3]);
    assert!(blobs.pin_set().pinned().is_empty());

    store.clear();
    let (r, _pins) = blobs.put_blob(&data[..], &resume(again)).unwrap();
    assert_eq!(r, uninterrupted(&data));
    assert_eq!(upload_of(&blobs, &r), again.upload_id);
    assert_eq!(
        stages(&store.events()),
        [(again.upload_id, 4), (again.upload_id, 5)]
    );
}

#[test]
fn a_reader_that_is_not_the_staged_blob_is_a_callers_error() {
    let (store, blobs) = remote();
    let data = bytes(5 * CHUNK_LEN + 100, 6);
    store.fail_when(once(Fault::Fail, stage_of(3)));
    let (token, _) = interruption(blobs.put_blob(&data[..], &options()).unwrap_err());
    store.clear();
    let mut other = data.clone();
    other[CHUNK_LEN + 7] ^= 1;
    for reader in [&other[..], &data[..2 * CHUNK_LEN + 3]] {
        let err = blobs.put_blob(reader, &resume(token)).unwrap_err();
        assert!(matches!(err, Error::InvalidResume { .. }), "{err:?}");
        assert!(blobs.pin_set().pinned().is_empty());
    }
    assert!(stages(&store.events()).is_empty(), "nothing is staged");
    // The token still resumes with the blob.
    let (r, _pins) = blobs.put_blob(&data[..], &resume(token)).unwrap();
    assert_eq!(r, uninterrupted(&data));
}

#[test]
fn a_token_chunk_size_below_the_floor_fails_before_reading() {
    let (store, blobs) = remote();
    let token = ResumeToken {
        upload_id: UploadId::new(1),
        chunk_size: ChunkSize::from_encoding(4096).unwrap(),
    };
    let err = blobs.put_blob(Unread, &resume(token)).unwrap_err();
    assert!(
        matches!(
            err,
            Error::ChunkSizeBelowMin {
                chunk_size: 4096,
                min: CHUNK
            }
        ),
        "{err:?}"
    );
    assert!(store.events().is_empty());
    assert!(blobs.pin_set().pinned().is_empty());
}

#[test]
fn the_tokens_chunk_size_wins_over_the_options() {
    let (store, blobs) = remote();
    let data = bytes(3 * CHUNK_LEN + 5, 8);
    store.fail_when(once(Fault::Fail, stage_of(1)));
    let (token, _) = interruption(blobs.put_blob(&data[..], &options()).unwrap_err());
    // Even a chunk size the options could not be stored with: the token's is the one checked.
    for chunk_size in [ChunkSize::DEFAULT, ChunkSize::from_encoding(1).unwrap()] {
        let theirs = PutBlobOptions {
            chunk_size,
            ..resume(token)
        };
        let (r, _pins) = blobs.put_blob(&data[..], &theirs).unwrap();
        assert_eq!(r.chunk_size(), CHUNK);
        assert_eq!(r, uninterrupted(&data));
    }
}

#[test]
fn a_store_that_cannot_resume_fails_a_resume_before_reading() {
    let store = Arc::new(MemoryBlobStore::new());
    let blobs = Blobs::new(Arc::clone(&store));
    let token = ResumeToken {
        upload_id: UploadId::new(1),
        chunk_size: ChunkSize::new(CHUNK).unwrap(),
    };
    let err = blobs.put_blob(Unread, &resume(token)).unwrap_err();
    assert!(matches!(err, Error::ResumeUnsupported), "{err:?}");
    assert!(blobs.pin_set().pinned().is_empty());
    assert!(store.bound_ids().is_empty());
}

// -- 9.3.6: a resume that writes nothing new, and a sweep that runs across its bind --

/// The writer's `PinSet`, with `hook` run before the sweep's first pin check, outside the set's lock: as a
/// `put_blob` on another thread would run between the sweep's listings and its checks.
struct Hooked<'a> {
    set: &'a PinSet,
    hook: Mutex<Option<Box<dyn FnOnce() + 'a>>>,
}

impl PinGate for Hooked<'_> {
    fn pinned(&self) -> Vec<GcObject> {
        PinGate::pinned(self.set)
    }

    fn unless_pinned<T>(
        &self,
        object: GcObject,
        delete: impl FnOnce(&dyn PinCheck) -> T,
    ) -> Option<T> {
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        PinGate::unless_pinned(self.set, object, delete)
    }
}

fn database(dir: &std::path::Path) -> (Arc<Watched>, BlobDatabase<Watched>) {
    let (store, blobs) = remote();
    let log = Database::create(dir.join("app.persia"), DatabaseOptions::default()).unwrap();
    (store, BlobDatabase::new(log, blobs))
}

/// Runs a sweep of `db`'s store whose first pin check first resumes `token` with `data` and drops its pins; returns
/// the resumed reference and the sweep's report.
fn sweep_across_resume(
    store: &Watched,
    db: &BlobDatabase<Watched>,
    token: ResumeToken,
    data: &[u8],
) -> (BlobRef, persia_blob::SweepReport) {
    sweep_across_resume_on(store.inner(), store.now(), db, token, data)
}

/// [`sweep_across_resume`] with `sweeper` (the store under `db`) and the time `now`.
fn sweep_across_resume_on<S: BlobStore + ?Sized, W: Sweep>(
    sweeper: &W,
    now: std::time::SystemTime,
    db: &BlobDatabase<S>,
    token: ResumeToken,
    data: &[u8],
) -> (BlobRef, persia_blob::SweepReport) {
    let mark = db.mark().unwrap();
    let resumed = Mutex::new(None);
    let hooked = Hooked {
        set: db.blobs().pin_set(),
        hook: Mutex::new(Some(Box::new(|| {
            let (r, pins) = db.put_blob(data, &resume(token)).unwrap();
            // Nothing references it but its pins (no document is logged during a GC): once they drop, only the
            // store's own state keeps it.
            drop(pins);
            *resumed.lock().unwrap() = Some(r);
        }))),
    };
    let options = SweepOptions {
        grace: DEFAULT_GRACE,
        now,
        rewrite_ratio: 0.5,
    };
    let report = sweeper.sweep(&mark, &hooked, &options).unwrap();
    let r = resumed
        .lock()
        .unwrap()
        .take()
        .expect("the sweep checked a pin");
    (r, report)
}

#[test]
fn a_resume_that_writes_nothing_new_moves_to_a_young_upload_a_late_sweep_keeps() {
    // With a short last chunk, and without one (then the last window is empty and every chunk is copied).
    for len in [5 * CHUNK_LEN + 100, 5 * CHUNK_LEN] {
        let dir = tempfile::tempdir().unwrap();
        let (store, db) = database(dir.path());
        let data = bytes(len, 9);
        // Every chunk staged, the last write's response lost: the resume has nothing left to write.
        let last = (len.div_ceil(CHUNK_LEN) - 1) as u32;
        store.fail_when(once(Fault::Lost, stage_of(last)));
        let (token, _) = interruption(db.blobs().put_blob(&data[..], &options()).unwrap_err());
        assert_eq!(
            store.chunks(token.upload_id),
            (0..=last).collect::<Vec<_>>()
        );
        // The upload is older than the grace period when it is resumed.
        store
            .clock()
            .advance(DEFAULT_GRACE + Duration::from_hours(1));
        store.clear();

        // The sweep lists the manifests (none) and the uploads (the token's, expired), then the resume binds and
        // drops its pins, then the sweep checks the token's upload: unpinned, named by no listed manifest.
        let (r, report) = sweep_across_resume(&store, &db, token, &data);
        let moved = upload_of(db.blobs(), &r);
        assert_ne!(moved, token.upload_id, "len {len}");
        assert_eq!(report.uploads_deleted, 1, "{report:?}");
        assert!(store.chunks(token.upload_id).is_empty());
        assert_eq!(store.chunks(moved), (0..=last).collect::<Vec<_>>());
        assert_eq!(db.get_blob_range(&r, 0..r.size()).unwrap(), data);
        assert_eq!(r, uninterrupted(&data));
        // The copy went to the new upload, pinned first, before the last window and the bind.
        let events = store.events();
        let restaged = events
            .iter()
            .position(|e| {
                matches!(e, Event::Restage { from, to, pinned: true, .. }
                    if *from == token.upload_id && *to == moved)
            })
            .unwrap();
        let bound = events
            .iter()
            .position(|e| matches!(e, Event::Bind { upload, .. } if *upload == moved))
            .unwrap();
        assert!(restaged < bound);
    }
}

#[test]
fn a_resume_that_writes_a_new_chunk_before_the_upload_listing_stays_young() {
    let dir = tempfile::tempdir().unwrap();
    let (store, db) = database(dir.path());
    let data = bytes(5 * CHUNK_LEN + 100, 10);
    store.fail_when(once(Fault::Fail, stage_of(2)));
    let (token, _) = interruption(db.blobs().put_blob(&data[..], &options()).unwrap_err());
    // An orphan small blob whose expired manifest the sweep checks first, before it lists the uploads.
    let (decoy, pins) = db.put_blob(&b"decoy"[..], &options()).unwrap();
    drop(pins);
    store
        .clock()
        .advance(DEFAULT_GRACE + Duration::from_hours(1));

    let (r, report) = sweep_across_resume(&store, &db, token, &data);
    assert_eq!(upload_of(db.blobs(), &r), token.upload_id);
    assert_eq!(report.blobs_deleted, 1, "{report:?}");
    assert_eq!(report.uploads_deleted, 0, "{report:?}");
    assert!(!db.blobs().store().contains(&decoy).unwrap());
    assert_eq!(db.get_blob_range(&r, 0..r.size()).unwrap(), data);
}

#[test]
fn a_resume_that_wrote_a_new_chunk_in_an_earlier_window_keeps_its_upload() {
    // Chunks 0 and 1 are staged, and so is the short chunk 66 (an earlier attempt's). The resume's two windows are
    // chunks 2..66, all new, which make the upload young, then chunk 66, which the listing showed.
    let (store, blobs) = remote();
    let data = bytes(66 * CHUNK_LEN + 100, 11);
    store.fail_when(once(Fault::Fail, stage_of(2)));
    let (token, _) = interruption(blobs.put_blob(&data[..], &options()).unwrap_err());
    store
        .inner()
        .stage_chunk(token.upload_id, 66, &data[66 * CHUNK_LEN..])
        .unwrap();
    store.clear();
    let (r, _pins) = blobs.put_blob(&data[..], &resume(token)).unwrap();
    assert_eq!(r, uninterrupted(&data));
    assert_eq!(upload_of(&blobs, &r), token.upload_id, "no restage");
    assert!(
        !store
            .events()
            .iter()
            .any(|e| matches!(e, Event::Restage { .. }))
    );
}

#[test]
fn a_misordered_listing_of_a_fully_staged_expired_upload_still_restages() {
    // A backend may list out of key order, or twice (SPEC §10.1 promises neither): the resume must still see that
    // every chunk it stages is there already, and move the blob to a young upload (ROADMAP 9.3.6).
    for order in [ListOrder::Reversed, ListOrder::Duplicated] {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new(UNIX_EPOCH + remote::START));
        let storage = MisorderedList {
            inner: MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>),
            order,
        };
        let store = Arc::new(RemoteBlobStore::new(storage).with_parallelism(NonZeroUsize::MIN));
        let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(5))
            .with_retry_clock(Arc::new(ManualRetryClock::new()));
        let log =
            Database::create(dir.path().join("app.persia"), DatabaseOptions::default()).unwrap();
        let db = BlobDatabase::new(log, blobs);
        let data = bytes(5 * CHUNK_LEN + 100, 12);
        // A writer staged every chunk and died before its bind; this is the token it recorded.
        let token = ResumeToken {
            upload_id: UploadId::new(77),
            chunk_size: ChunkSize::new(CHUNK).unwrap(),
        };
        for (i, chunk) in (0_u32..).zip(data.chunks(CHUNK_LEN)) {
            store.stage_chunk(token.upload_id, i, chunk).unwrap();
        }
        clock.advance(DEFAULT_GRACE + Duration::from_hours(1));

        let kept = BlobStore::resume(&*store, token).unwrap();
        let full = data
            .chunks(CHUNK_LEN)
            .filter(|c| c.len() == CHUNK_LEN)
            .count() as u32;
        assert_eq!(kept.next_index(), full, "{order:?}: the kept run");

        let (r, report) = sweep_across_resume_on(&*store, clock.now(), &db, token, &data);
        let moved = store.manifest(r.blob_id()).unwrap().unwrap().upload_id();
        assert_ne!(moved, token.upload_id, "{order:?}: no restage");
        assert_eq!(report.uploads_deleted, 1, "{order:?}: {report:?}");
        assert_eq!(
            db.get_blob_range(&r, 0..r.size()).unwrap(),
            data,
            "{order:?}"
        );
        assert_eq!(r, uninterrupted(&data));
    }
}

/// A store holding `z` (three chunks and a bit, bound under its own upload) and `x`, its first two chunks, bound under
/// the upload the returned token names: a token replayed after its upload was bound (ADR-0053).
fn replayed() -> (
    Arc<Watched>,
    Blobs<Watched>,
    ResumeToken,
    BlobRef,
    Vec<u8>,
    Vec<u8>,
) {
    let (store, blobs) = remote();
    let x = bytes(2 * CHUNK_LEN, 13);
    let mut z = x.clone();
    z.extend(bytes(CHUNK_LEN + 50, 14));
    drop(blobs.put_blob(&z[..], &options()).unwrap());
    let (rx, pins) = blobs.put_blob(&x[..], &options()).unwrap();
    drop(pins);
    let token = ResumeToken {
        upload_id: upload_of(&blobs, &rx),
        chunk_size: ChunkSize::new(CHUNK).unwrap(),
    };
    store.clear();
    (store, blobs, token, rx, x, z)
}

#[test]
fn a_resume_never_discards_the_tokens_upload_which_may_be_bound() {
    // The resume of z keeps x's two chunks and stages the rest under x's upload (new chunks: no restage). Then it
    // fails its expected id, or finds z bound already: neither may discard x's upload.
    let (store, blobs, token, rx, x, z) = replayed();
    let mismatch = PutBlobOptions {
        expected_id: Some(persia_blob::blob_id_of(b"another blob")),
        ..resume(token)
    };
    let err = blobs.put_blob(&z[..], &mismatch).unwrap_err();
    assert!(matches!(err, Error::BlobIdMismatch { .. }), "{err:?}");
    assert!(!store.events().contains(&Event::Discard(token.upload_id)));
    assert_eq!(blobs.get_blob_range(&rx, 0..rx.size()).unwrap(), x);

    let (store, blobs, token, rx, x, z) = replayed();
    let (rz, _pins) = blobs.put_blob(&z[..], &resume(token)).unwrap();
    assert_eq!(rz, uninterrupted(&z));
    assert_ne!(
        upload_of(&blobs, &rz),
        token.upload_id,
        "z was bound before"
    );
    assert!(
        !store
            .events()
            .iter()
            .any(|e| matches!(e, Event::Restage { .. })),
        "the resume staged new chunks under the token's upload"
    );
    assert!(!store.events().contains(&Event::Discard(token.upload_id)));
    assert_eq!(blobs.get_blob_range(&rx, 0..rx.size()).unwrap(), x);
    assert_eq!(blobs.get_blob_range(&rz, 0..rz.size()).unwrap(), z);
}

/// Chunk 1's read reports corruption. Resume must not return the same token forever: the run ends at that
/// chunk, and the upload that binds is not the one whose chunk cannot be read (SPEC §9.4).
#[test]
fn a_corrupt_kept_chunk_resumes_without_the_same_token_forever() {
    let data = bytes(3 * CHUNK_LEN, 15);
    let token = ResumeToken {
        upload_id: UploadId::new(9),
        chunk_size: ChunkSize::new(CHUNK).unwrap(),
    };
    let bad = RemoteKey::Chunk {
        upload_id: token.upload_id,
        index: 1,
    }
    .key()
    .unwrap();
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + remote::START));
    let storage = CorruptGet {
        inner: MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>),
        bad,
    };
    let store = Arc::new(RemoteBlobStore::new(storage).with_parallelism(NonZeroUsize::MIN));
    for (index, chunk) in (0_u32..).zip(data.chunks(CHUNK_LEN)) {
        store.stage_chunk(token.upload_id, index, chunk).unwrap();
    }

    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(9))
        .with_retry_clock(Arc::new(ManualRetryClock::new()));
    let mut options = resume(token);
    let mut previous = token;
    let mut attempts = 0_u32;
    let bound = loop {
        match blobs.put_blob(&data[..], &options) {
            Ok(bound) => break bound,
            Err(Error::UploadInterrupted { token: next, .. }) => {
                attempts += 1;
                assert!(attempts < 3, "still failing after {attempts} resumes");
                assert_ne!(next, previous, "the same token came back");
                previous = next;
                options = resume(next);
            }
            Err(e) => panic!("not a resume: {e:?}"),
        }
    };
    let (r, _pins) = bound;
    assert_eq!(r, uninterrupted(&data));
    assert_eq!(blobs.get_blob_range(&r, 0..r.size()).unwrap(), data);
    let upload = blobs
        .store()
        .manifest(r.blob_id())
        .unwrap()
        .unwrap()
        .upload_id();
    assert_ne!(upload, token.upload_id, "the corrupt upload was not reused");
}
