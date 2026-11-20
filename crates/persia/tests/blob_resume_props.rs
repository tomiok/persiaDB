//! Resumed uploads through `put_blob`, as properties and as a table (SPEC §9.4, §9.5; ROADMAP 9.3.4, 9.3.6;
//! ADR-0050, ADR-0051), over the remote layout on a `MemoryStorage` with faults injected at the store.
//!
//! Invariants:
//! 1. Any blob (empty, small, at the small-blob threshold, an exact multiple of the chunk size, one byte over or
//!    under), at any chunk size at or above the floor, cut by a reader error at any byte or a store fault at any
//!    step (the k-th chunk write failed or its response lost, the bind, the resume's listing, a restage), then
//!    resumed up to three times (each resume cut again), binds the `BlobRef` an uninterrupted `put_blob` binds and
//!    reads back the same bytes. No chunk is written twice under one upload: the kept run is never uploaded again,
//!    and a chunk reaches a second upload only by a restage. Every chunk write, listing, restage and bind is pinned,
//!    and every pin drops with the error.
//! 2. `UploadInterrupted` wraps exactly the classes `put_blob`'s docs list, on a store that can resume: a reader
//!    failure or a store error (`Storage`, `StagedChunkConflict`, `Corrupt`) after the upload staged chunks or while
//!    resuming, and a bind's store error with a known outcome. The caller's errors, an unknown bind outcome, a corrupt
//!    bound manifest, and everything before a chunk write are not wrapped; on the pack and memory stores nothing is.
//! 3. A listing out of index order (outside SPEC §10.1's sorted contract) still resumes to the original blob.
//!
//! Seeds: fixed per proptest block (printed on failure); `PROPTEST_CASES` overrides the case count.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::assert_is_empty,
    missing_docs
)] // Tests.

#[path = "support/remote.rs"]
mod remote;

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use core::num::NonZeroUsize;

use persia::internal::blob::{Blobs, MAX_BIND_BUDGET, PutBlobOptions};
use persia::internal::retry::ManualRetryClock;
use persia_blob::remote::{RemoteBlobStore, ResumedUpload};
use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobRef, BlobStore, ChunkSize, Error, MemoryBlobStore,
    PackMode, PackStore, Result, ResumeToken, SMALL_BLOB_MAX, UploadId, blob_id_of,
};
use persia_engine::db::SegmentIds;
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::{MemoryStorage, Op};
use proptest::prelude::*;
use remote::{Event, Fault, Watched};

const MIN: u32 = ChunkSize::MIN;
const SMALL: usize = SMALL_BLOB_MAX as usize;

fn bytes(len: usize, seed: u8) -> Vec<u8> {
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

fn options(chunk: u32) -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(chunk).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn resume(chunk: u32, token: ResumeToken) -> PutBlobOptions {
    PutBlobOptions {
        resume: Some(token),
        ..options(chunk)
    }
}

fn watched(seed: u64) -> (Arc<Watched>, Blobs<Watched>) {
    let store = Watched::new();
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(seed))
        .with_retry_clock(Arc::new(ManualRetryClock::new()));
    store.watch(blobs.pin_set());
    (store, blobs)
}

/// The reference an uninterrupted `put_blob` returns, on a store of its own.
fn uninterrupted(data: &[u8], chunk: u32) -> BlobRef {
    watched(1).1.put_blob(data, &options(chunk)).unwrap().0
}

/// A reader of `data` that yields at most `step` bytes per call and fails once it has yielded `fail_at` bytes (at
/// `data.len()` or later: it fails where it would have ended).
struct Cutting<'a> {
    data: &'a [u8],
    pos: usize,
    fail_at: Option<usize>,
    step: usize,
}

impl Read for Cutting<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.fail_at.is_some_and(|at| self.pos >= at) {
            return Err(io::Error::other("the reader failed"));
        }
        let end = self
            .data
            .len()
            .min(self.fail_at.unwrap_or(usize::MAX))
            .min(self.pos + out.len().min(self.step));
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

// -- 1. Interrupted and resumed, any size, any cut --

/// Where one attempt is cut.
#[derive(Debug, Clone, Copy)]
enum Cut {
    /// The reader fails once it has yielded this many bytes (taken modulo the blob's length plus one).
    Reader(usize),
    /// The reader fails at this chunk boundary (a multiple of the chunk size, clamped to the blob's length).
    ReaderAtChunk(usize),
    /// The `nth` chunk write of the attempt fails, or is applied and its response lost.
    Stage { nth: usize, lost: bool },
    /// The bind fails with a backend error: nothing bound, the outcome known.
    Bind,
    /// The resume's listing fails.
    Listing,
    /// A restage fails, or is applied and its response lost.
    Restage { lost: bool },
}

fn arb_cut() -> impl Strategy<Value = Cut> {
    prop_oneof![
        3 => any::<usize>().prop_map(Cut::Reader),
        1 => (0_usize..6).prop_map(Cut::ReaderAtChunk),
        4 => ((0_usize..6), any::<bool>()).prop_map(|(nth, lost)| Cut::Stage { nth, lost }),
        1 => Just(Cut::Bind),
        1 => Just(Cut::Listing),
        1 => any::<bool>().prop_map(|lost| Cut::Restage { lost }),
    ]
}

/// A chunk size at or above the floor: the floor itself, or up to three times it (not a power of two).
fn arb_chunk() -> impl Strategy<Value = u32> {
    prop_oneof![Just(MIN), MIN..=3 * MIN]
}

/// A blob length for `chunk`: empty, small (up to the threshold, inclusive), the threshold plus one, an exact
/// multiple of the chunk size, one over or one under one, or anything up to four chunks.
fn arb_len(chunk: u32) -> impl Strategy<Value = usize> {
    let c = chunk as usize;
    prop_oneof![
        Just(0),
        1..=SMALL,
        Just(SMALL),
        Just(SMALL + 1),
        (1_usize..=4).prop_map(move |k| k * c),
        (1_usize..=4).prop_map(move |k| k * c + 1),
        (2_usize..=4).prop_map(move |k| k * c - 1),
        0..=4 * c + 1,
    ]
}

/// What the picker saw in one case: every call in order, with whether it was applied.
#[derive(Debug, Default)]
struct Log {
    calls: Vec<(Event, bool)>,
}

/// Fails the first call of this attempt that `cut` names; logs every call and whether it was applied.
fn picker(
    cut: Option<Cut>,
    log: Arc<Mutex<Log>>,
) -> impl FnMut(&Event) -> Option<Fault> + Send + 'static {
    let mut stages = 0_usize;
    let mut fired = false;
    move |event| {
        let fault = match (cut, event) {
            _ if fired => None,
            (Some(Cut::Stage { nth, lost }), Event::Stage { .. }) => {
                stages += 1;
                (stages - 1 == nth).then_some(if lost { Fault::Lost } else { Fault::Fail })
            }
            (Some(Cut::Bind), Event::Bind { .. }) | (Some(Cut::Listing), Event::Resume { .. }) => {
                Some(Fault::Fail)
            }
            (Some(Cut::Restage { lost }), Event::Restage { .. }) => {
                Some(if lost { Fault::Lost } else { Fault::Fail })
            }
            _ => None,
        };
        fired |= fault.is_some();
        log.lock()
            .unwrap()
            .calls
            .push((event.clone(), !matches!(fault, Some(Fault::Fail))));
        fault
    }
}

/// Invariant 1's write and pin rules over every call of a case: a chunk is written at most once per upload (a
/// restage's copies count as writes), and every call that writes or lists is pinned.
fn check_writes(log: &Log) -> std::result::Result<(), TestCaseError> {
    let mut written: BTreeSet<(UploadId, u32)> = BTreeSet::new();
    for (event, applied) in &log.calls {
        match *event {
            Event::Stage {
                upload,
                index,
                pinned,
            } => {
                prop_assert!(pinned, "chunk {} of {} written unpinned", index, upload);
                prop_assert!(
                    !written.contains(&(upload, index)),
                    "chunk {} of {} uploaded twice: {:?}",
                    index,
                    upload,
                    log.calls
                );
                if *applied {
                    written.insert((upload, index));
                }
            }
            Event::Restage {
                count, to, pinned, ..
            } => {
                prop_assert!(pinned, "restage to {} unpinned", to);
                if *applied {
                    for index in 0..count as u32 {
                        prop_assert!(
                            written.insert((to, index)),
                            "chunk {} restaged onto {} twice",
                            index,
                            to
                        );
                    }
                }
            }
            Event::Resume { upload, pinned } => {
                prop_assert!(pinned, "{} listed unpinned", upload);
            }
            Event::Bind { pinned, .. } => prop_assert_eq!(pinned, (true, true)),
            Event::Manifest(_) | Event::Discard(_) => {}
        }
    }
    Ok(())
}

fn run_case(
    len: usize,
    chunk: u32,
    cuts: &[Cut],
    step: usize,
    seed: u8,
) -> std::result::Result<(), TestCaseError> {
    let data = bytes(len, seed);
    let (store, blobs) = watched(u64::from(seed));
    let log = Arc::new(Mutex::new(Log::default()));
    let mut token: Option<ResumeToken> = None;
    let mut bound = None;
    for attempt in 0..=cuts.len() {
        let cut = cuts.get(attempt).copied();
        store.fail_when(picker(cut, Arc::clone(&log)));
        let fail_at = match cut {
            Some(Cut::Reader(at)) => Some(at % (len + 1)),
            Some(Cut::ReaderAtChunk(k)) => Some((k * chunk as usize).min(len)),
            _ => None,
        };
        let reader = Cutting {
            data: &data,
            pos: 0,
            fail_at,
            step,
        };
        let opts = token.map_or_else(|| options(chunk), |t| resume(chunk, t));
        let resuming = token.is_some();
        match blobs.put_blob(reader, &opts) {
            Ok((r, pins)) => {
                drop(pins);
                bound = Some(r);
                break;
            }
            Err(Error::UploadInterrupted { token: t, source }) => {
                prop_assert_eq!(t.chunk_size.get(), chunk);
                prop_assert!(len > SMALL, "a small blob stages nothing: {:?}", source);
                if fail_at.is_some() {
                    prop_assert!(matches!(*source, Error::Read(_)), "{:?}", source);
                } else {
                    prop_assert!(matches!(*source, Error::Storage(_)), "{:?}", source);
                }
                token = Some(t);
            }
            Err(e) => {
                // Not resumable: only before any chunk write of a new upload (a reader failure before the first
                // window, which every blob here is under, or a small blob's one write).
                prop_assert!(
                    !resuming,
                    "a resume failed unwrapped: {:?} (cut {:?})",
                    e,
                    cut
                );
                match e {
                    Error::Read(_) => prop_assert!(fail_at.is_some()),
                    Error::Storage(_) => prop_assert!(len <= SMALL, "{:?}", cut),
                    other => prop_assert!(false, "unexpected {:?} (cut {:?})", other, cut),
                }
                token = None;
            }
        }
        prop_assert!(
            blobs.pin_set().pinned().is_empty(),
            "the pins drop with the error"
        );
    }
    store.heal();
    let r = bound.expect("the last attempt is not cut");
    prop_assert_eq!(&r, &uninterrupted(&data, chunk));
    prop_assert_eq!(blobs.get_blob_range(&r, 0..r.size()).unwrap(), &data[..]);
    if len > SMALL {
        let upload = blobs
            .store()
            .inner()
            .manifest(r.blob_id())
            .unwrap()
            .unwrap()
            .upload_id();
        let n = len.div_ceil(chunk as usize) as u32;
        prop_assert_eq!(store.chunks(upload), (0..n).collect::<Vec<_>>());
    }
    check_writes(&log.lock().unwrap())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if cfg!(feature = "slow") { 512 } else { 32 },
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x7265_7375_6d65_0934),
        ..ProptestConfig::default()
    })]

    #[test]
    fn an_upload_cut_anywhere_and_resumed_binds_the_uninterrupted_blob(
        (chunk, len) in arb_chunk().prop_flat_map(|c| (Just(c), arb_len(c))),
        cuts in prop::collection::vec(arb_cut(), 0..=3),
        step in prop_oneof![Just(usize::MAX), 1_usize..=70_000],
        seed in any::<u8>(),
    ) {
        run_case(len, chunk, &cuts, step, seed)?;
    }
}

#[test]
fn a_resume_cut_at_every_step_of_a_three_chunk_blob_binds_the_uninterrupted_blob() {
    // The table behind the property: every first cut, then every second cut, of a blob of two full chunks and one
    // byte (the short last chunk one byte over a multiple).
    let len = 2 * MIN as usize + 1;
    let firsts = [
        Cut::Stage {
            nth: 0,
            lost: false,
        },
        Cut::Stage {
            nth: 1,
            lost: false,
        },
        Cut::Stage {
            nth: 2,
            lost: false,
        },
        Cut::Stage { nth: 2, lost: true },
        Cut::Bind,
    ];
    let seconds = [
        Cut::Reader(0),
        Cut::Reader(MIN as usize + 5),
        Cut::Reader(len),
        Cut::Stage {
            nth: 0,
            lost: false,
        },
        Cut::Stage { nth: 0, lost: true },
        Cut::Bind,
        Cut::Listing,
        Cut::Restage { lost: false },
        Cut::Restage { lost: true },
    ];
    for first in firsts {
        for second in seconds {
            run_case(len, MIN, &[first, second], usize::MAX, 3)
                .unwrap_or_else(|e| panic!("cuts {first:?}, {second:?}: {e}"));
        }
    }
}

// -- 2. The `UploadInterrupted` table --

/// A call an [`Inject`] store may fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Stage(u32),
    Bind,
    BindSmall,
    Resume,
    Restage,
}

type Injector = Box<dyn FnMut(Call) -> Option<Error> + Send>;

/// A store over `S` that fails the calls `inject` picks with the error it returns, before applying them. Whether it
/// can resume is `S`'s.
struct Inject<S> {
    inner: S,
    inject: Mutex<Option<Injector>>,
}

impl<S> Inject<S> {
    fn new(inner: S) -> Arc<Self> {
        Arc::new(Self {
            inner,
            inject: Mutex::new(None),
        })
    }

    /// Fails the first call `which` matches with `error()`; nothing else.
    fn once(&self, which: Call, error: fn() -> Error) {
        let mut fired = false;
        *self.inject.lock().unwrap() = Some(Box::new(move |call| {
            (!fired && call == which).then(|| {
                fired = true;
                error()
            })
        }));
    }

    fn check(&self, call: Call) -> Result<()> {
        match self.inject.lock().unwrap().as_mut().and_then(|f| f(call)) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl<S> std::fmt::Debug for Inject<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inject").finish_non_exhaustive()
    }
}

impl<S: BlobStore> BlobStore for Inject<S> {
    fn stage_chunk(&self, upload_id: UploadId, index: u32, bytes: &[u8]) -> Result<()> {
        self.check(Call::Stage(index))?;
        self.inner.stage_chunk(upload_id, index, bytes)
    }

    fn stage_chunks(&self, upload_id: UploadId, first_index: u32, chunks: &[&[u8]]) -> Result<()> {
        for (i, bytes) in (first_index..).zip(chunks) {
            self.stage_chunk(upload_id, i, bytes)?;
        }
        Ok(())
    }

    fn bind(&self, manifest: &BlobManifest) -> Result<BindOutcome> {
        self.check(Call::Bind)?;
        self.inner.bind(manifest)
    }

    fn bind_small(&self, manifest: &BlobManifest, bytes: &[u8]) -> Result<BindOutcome> {
        self.check(Call::BindSmall)?;
        self.inner.bind_small(manifest, bytes)
    }

    fn manifest(&self, blob_id: BlobId) -> Result<Option<BlobManifest>> {
        self.inner.manifest(blob_id)
    }

    fn read_chunk(&self, manifest: &BlobManifest, index: u32) -> Result<Vec<u8>> {
        self.inner.read_chunk(manifest, index)
    }

    fn read_chunks(&self, manifest: &BlobManifest, indices: Range<u32>) -> Result<Vec<Vec<u8>>> {
        self.inner.read_chunks(manifest, indices)
    }

    fn discard_upload(&self, upload_id: UploadId) -> Result<()> {
        self.inner.discard_upload(upload_id)
    }

    fn resumable(&self) -> bool {
        self.inner.resumable()
    }

    fn resume(&self, token: ResumeToken) -> Result<ResumedUpload> {
        self.check(Call::Resume)?;
        self.inner.resume(token)
    }

    fn restage(&self, from: ResumeToken, chunk_crcs: &[u32], to: UploadId) -> Result<()> {
        self.check(Call::Restage)?;
        self.inner.restage(from, chunk_crcs, to)
    }
}

fn storage_error() -> Error {
    Error::Storage(persia_storage::Error::Io {
        op: Op::PutIfAbsent,
        target: "injected".to_owned(),
        source: io::Error::other("injected"),
    })
}

fn conflict() -> Error {
    Error::StagedChunkConflict {
        upload_id: UploadId::new(0xdead),
        index: 1,
    }
}

fn corrupt() -> Error {
    Error::Corrupt {
        encoding: "blob chunk",
        what: "injected",
    }
}

fn unknown() -> Error {
    Error::BindOutcomeUnknown("injected".into())
}

type Remote = Inject<RemoteBlobStore<MemoryStorage>>;

fn remote_store() -> (Arc<Remote>, Blobs<Remote>) {
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_hours(1_000)));
    let storage = MemoryStorage::with_clock(clock as Arc<dyn Clock>);
    let store = Inject::new(RemoteBlobStore::new(storage).with_parallelism(NonZeroUsize::MIN));
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(21))
        .with_retry_clock(Arc::new(ManualRetryClock::new()));
    (store, blobs)
}

/// Three full chunks and a short one at the floor.
fn chunked() -> Vec<u8> {
    bytes(3 * MIN as usize + 17, 40)
}

fn wrapped(e: Error) -> (ResumeToken, Error) {
    match e {
        Error::UploadInterrupted { token, source } => (token, *source),
        other => panic!("not wrapped: {other:?}"),
    }
}

/// `e` is not an `UploadInterrupted`, at any depth of its sources either.
fn not_wrapped(e: &Error) {
    assert!(
        !matches!(e, Error::UploadInterrupted { .. }),
        "wrapped: {e:?}"
    );
}

/// An interrupted upload of `chunked()` on `blobs`, cut at its second chunk write: its token.
fn interrupted_upload(store: &Remote, blobs: &Blobs<Remote>, data: &[u8]) -> ResumeToken {
    store.once(Call::Stage(1), storage_error);
    wrapped(blobs.put_blob(data, &options(MIN)).unwrap_err()).0
}

#[test]
fn store_errors_while_staging_a_new_upload_are_wrapped_with_its_token() {
    for (error, is) in [
        (storage_error as fn() -> Error, "storage"),
        (conflict, "conflict"),
        (corrupt, "corrupt"),
    ] {
        // The first chunk write (nothing staged yet: the write was attempted) and the last.
        for index in [0, 3] {
            let (store, blobs) = remote_store();
            let data = chunked();
            store.once(Call::Stage(index), error);
            let (token, source) = wrapped(blobs.put_blob(&data[..], &options(MIN)).unwrap_err());
            match (is, &source) {
                ("storage", Error::Storage(_))
                | ("conflict", Error::StagedChunkConflict { .. })
                | ("corrupt", Error::Corrupt { .. }) => {}
                _ => panic!("{is} at {index}: {source:?}"),
            }
            assert_eq!(token.chunk_size.get(), MIN);
            // The token resumes to the blob.
            let (r, _pins) = blobs.put_blob(&data[..], &resume(MIN, token)).unwrap();
            assert_eq!(r, uninterrupted(&data, MIN), "{is} at {index}");
        }
    }
}

#[test]
fn errors_while_resuming_are_wrapped_with_the_upload_being_staged() {
    let data = chunked();
    // The listing fails (a backend error, or a chunk gone or changed): the same token.
    for error in [storage_error as fn() -> Error, corrupt] {
        let (store, blobs) = remote_store();
        let token = interrupted_upload(&store, &blobs, &data);
        store.once(Call::Resume, error);
        let (again, _) = wrapped(blobs.put_blob(&data[..], &resume(MIN, token)).unwrap_err());
        assert_eq!(again, token);
    }
    // The reader fails inside the kept run (while skipping it), and after it (while cutting the rest).
    for fail_at in [10, MIN as usize + 3, 2 * MIN as usize + 1, data.len()] {
        let (store, blobs) = remote_store();
        let token = interrupted_upload(&store, &blobs, &data);
        let reader = Cutting {
            data: &data,
            pos: 0,
            fail_at: Some(fail_at),
            step: usize::MAX,
        };
        let (again, source) = wrapped(blobs.put_blob(reader, &resume(MIN, token)).unwrap_err());
        assert!(matches!(source, Error::Read(_)), "{source:?}");
        assert_eq!(again, token, "at {fail_at}");
    }
    // A chunk write after the kept run fails (any store error): the same token.
    for error in [storage_error as fn() -> Error, corrupt] {
        let (store, blobs) = remote_store();
        let token = interrupted_upload(&store, &blobs, &data);
        store.once(Call::Stage(2), error);
        let (again, _) = wrapped(blobs.put_blob(&data[..], &resume(MIN, token)).unwrap_err());
        assert_eq!(again, token);
    }
}

#[test]
fn a_failed_restage_is_wrapped_with_the_upload_that_holds_the_chunks() {
    let data = chunked();
    // ROADMAP 9.3.6: every chunk staged (the bind failed), so the resume restages before binding. A failed copy
    // resumes from the token's upload, which is whole.
    for error in [storage_error as fn() -> Error, corrupt, conflict] {
        let (store, blobs) = remote_store();
        store.once(Call::Bind, storage_error);
        let (token, source) = wrapped(blobs.put_blob(&data[..], &options(MIN)).unwrap_err());
        assert!(matches!(source, Error::Storage(_)), "{source:?}");
        store.once(Call::Restage, error);
        let (again, _) = wrapped(blobs.put_blob(&data[..], &resume(MIN, token)).unwrap_err());
        assert_eq!(again, token, "the token's upload is whole");
        let (r, _pins) = blobs.put_blob(&data[..], &resume(MIN, again)).unwrap();
        assert_eq!(r, uninterrupted(&data, MIN));
    }
    // A conflict on the token's upload: the copy goes to a new upload, and a failed copy resumes from that one.
    let (store, blobs) = remote_store();
    let token = interrupted_upload(&store, &blobs, &data);
    store
        .inner
        .stage_chunk(token.upload_id, 2, &bytes(MIN as usize, 99))
        .unwrap();
    store.once(Call::Restage, corrupt);
    let (again, source) = wrapped(blobs.put_blob(&data[..], &resume(MIN, token)).unwrap_err());
    assert!(matches!(source, Error::Corrupt { .. }), "{source:?}");
    assert_ne!(again.upload_id, token.upload_id, "the new upload");
    assert!(blobs.pin_set().pinned().is_empty());
}

#[test]
fn a_bind_error_is_wrapped_only_when_its_outcome_is_known() {
    let data = chunked();
    let (store, blobs) = remote_store();
    store.once(Call::Bind, storage_error);
    let (token, _) = wrapped(blobs.put_blob(&data[..], &options(MIN)).unwrap_err());
    // On a resume too.
    store.once(Call::Bind, storage_error);
    let (again, _) = wrapped(blobs.put_blob(&data[..], &resume(MIN, token)).unwrap_err());
    assert_ne!(
        again.upload_id, token.upload_id,
        "9.3.6: restaged before binding"
    );

    // An unknown outcome (the blob may be bound later) and a corrupt bound manifest (a resume finds it again).
    let zero = PutBlobOptions {
        bind_budget: Duration::ZERO,
        ..options(MIN)
    };
    for (error, is_unknown, resuming) in [
        (unknown as fn() -> Error, true, false),
        (unknown, true, true),
        (corrupt, false, false),
        (corrupt, false, true),
    ] {
        let (store, blobs) = remote_store();
        let opts = if resuming {
            let token = interrupted_upload(&store, &blobs, &data);
            PutBlobOptions {
                resume: Some(token),
                ..zero.clone()
            }
        } else {
            zero.clone()
        };
        store.once(Call::Bind, error);
        let e = blobs.put_blob(&data[..], &opts).unwrap_err();
        not_wrapped(&e);
        if is_unknown {
            assert!(e.is_bind_outcome_unknown(), "{e:?}");
        } else {
            assert!(matches!(e, Error::Corrupt { .. }), "{e:?}");
        }
        assert!(blobs.pin_set().pinned().is_empty());
    }
}

#[test]
fn a_small_blob_is_never_wrapped() {
    // Nothing is staged for a small blob (one write of its manifest): there is nothing to resume.
    for len in [0, 1, SMALL] {
        let data = bytes(len, 41);
        for error in [storage_error as fn() -> Error, corrupt] {
            let (store, blobs) = remote_store();
            store.once(Call::BindSmall, error);
            let e = blobs.put_blob(&data[..], &options(MIN)).unwrap_err();
            not_wrapped(&e);
        }
        let (_, blobs) = remote_store();
        let reader = Cutting {
            data: &data,
            pos: 0,
            fail_at: Some(len),
            step: usize::MAX,
        };
        let e = blobs.put_blob(reader, &options(MIN)).unwrap_err();
        assert!(matches!(e, Error::Read(_)), "{e:?}");
    }
}

#[test]
fn a_reader_that_fails_before_the_first_chunk_write_is_not_wrapped() {
    // Under one window (64 chunks at the floor): the chunks are buffered, nothing was written.
    let data = chunked();
    for fail_at in [0, SMALL, SMALL + 1, data.len()] {
        let (_, blobs) = remote_store();
        let reader = Cutting {
            data: &data,
            pos: 0,
            fail_at: Some(fail_at),
            step: usize::MAX,
        };
        let e = blobs.put_blob(reader, &options(MIN)).unwrap_err();
        assert!(matches!(e, Error::Read(_)), "{e:?}");
    }
}

#[test]
fn callers_errors_are_never_wrapped() {
    let data = chunked();
    let (store, blobs) = remote_store();
    let token = interrupted_upload(&store, &blobs, &data);
    for resuming in [false, true] {
        let base = || {
            if resuming {
                resume(MIN, token)
            } else {
                options(MIN)
            }
        };
        // Before reading: a chunk size below the floor, an invalid MIME type, a bind budget above the cap.
        let low = ChunkSize::from_encoding(4096).unwrap();
        let below = if resuming {
            PutBlobOptions {
                resume: Some(ResumeToken {
                    chunk_size: low,
                    ..token
                }),
                ..options(MIN)
            }
        } else {
            PutBlobOptions {
                chunk_size: low,
                ..options(MIN)
            }
        };
        let e = blobs.put_blob(Unread, &below).unwrap_err();
        assert!(matches!(e, Error::ChunkSizeBelowMin { .. }), "{e:?}");
        let e = blobs
            .put_blob(
                Unread,
                &PutBlobOptions {
                    mime: String::new(),
                    ..base()
                },
            )
            .unwrap_err();
        assert!(matches!(e, Error::InvalidBlobRef(_)), "{e:?}");
        let e = blobs
            .put_blob(
                Unread,
                &PutBlobOptions {
                    bind_budget: MAX_BIND_BUDGET + Duration::from_secs(1),
                    ..base()
                },
            )
            .unwrap_err();
        assert!(matches!(e, Error::BindBudgetAboveMax { .. }), "{e:?}");

        // After staging: the bytes do not hash to `expected_id`.
        let e = blobs
            .put_blob(
                &data[..],
                &PutBlobOptions {
                    expected_id: Some(blob_id_of(b"another blob")),
                    ..base()
                },
            )
            .unwrap_err();
        assert!(matches!(e, Error::BlobIdMismatch { .. }), "{e:?}");
        assert!(blobs.pin_set().pinned().is_empty());
    }
    // A resume whose reader is not the staged blob (the mismatch above discarded the token's upload: stage anew).
    let token = interrupted_upload(&store, &blobs, &data);
    let mut other = data.clone();
    other[0] ^= 1;
    let e = blobs.put_blob(&other[..], &resume(MIN, token)).unwrap_err();
    assert!(matches!(e, Error::InvalidResume { .. }), "{e:?}");
}

/// Every injected class on a store that cannot resume: never wrapped, and a resume is `ResumeUnsupported`.
fn never_wrapped_on<S: BlobStore + 'static>(inner: S) {
    let store = Inject::new(inner);
    assert!(!store.resumable());
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(22))
        .with_retry_clock(Arc::new(ManualRetryClock::new()));
    let data = chunked();
    let zero = PutBlobOptions {
        bind_budget: Duration::ZERO,
        ..options(MIN)
    };
    for (call, error) in [
        (Call::Stage(0), storage_error as fn() -> Error),
        (Call::Stage(2), storage_error),
        (Call::Stage(3), conflict),
        (Call::Stage(1), corrupt),
        (Call::Bind, storage_error),
        (Call::Bind, unknown),
        (Call::BindSmall, storage_error),
    ] {
        store.once(call, error);
        let small = bytes(10, 1);
        let input: &[u8] = if call == Call::BindSmall {
            &small
        } else {
            &data
        };
        let e = blobs.put_blob(input, &zero).unwrap_err();
        not_wrapped(&e);
        assert!(blobs.pin_set().pinned().is_empty());
    }
    let token = ResumeToken {
        upload_id: UploadId::new(1),
        chunk_size: ChunkSize::new(MIN).unwrap(),
    };
    let e = blobs.put_blob(Unread, &resume(MIN, token)).unwrap_err();
    assert!(matches!(e, Error::ResumeUnsupported), "{e:?}");
    // A reader that fails after a whole window was written (64 chunks at the floor).
    let big = bytes(65 * MIN as usize, 2);
    let reader = Cutting {
        data: &big,
        pos: 0,
        fail_at: Some(64 * MIN as usize + 9),
        step: usize::MAX,
    };
    let e = blobs.put_blob(reader, &options(MIN)).unwrap_err();
    assert!(matches!(e, Error::Read(_)), "{e:?}");
}

#[test]
fn nothing_is_wrapped_on_the_memory_store() {
    never_wrapped_on(MemoryBlobStore::new());
}

#[test]
fn nothing_is_wrapped_on_the_pack_store() {
    let dir = tempfile::tempdir().unwrap();
    let pack = PackStore::open(
        dir.path().join("app.persia.blobs"),
        PackMode::ReadWrite,
        [0; 16],
    )
    .unwrap();
    never_wrapped_on(pack);
}
