//! GC sweeps past the grace period interleaved with resumed uploads on the remote layout, under a seeded scheduler
//! (SPEC §9.4, §9.5, §10.1; ROADMAP 9.3.4, 9.3.6, 9.3.7, 9.5.9, 9.5.10; ADR-0051, ADR-0053). In-memory: a clocked
//! `MemoryStorage` (no cloud backend exists yet to point at an emulator, ROADMAP 2.x / 12.2.4), with `LaggingList`
//! (`persia_blob::testing`) in the lagging variant.
//!
//! Each round, the main thread leaves interrupted uploads behind (chunks staged straight into the store under a
//! token's upload id, as a writer that died would: a run, a run with a gap, every chunk, none, a short chunk 0), then
//! moves the injected clock past the grace period, so every one of them is expired. Then participants run on their
//! own threads, one at a time, in an order a seeded scheduler picks at every step point (each call into the blob
//! store, and each pin check of the sweep, which yields before it takes the pin set's lock):
//! - resumers: `put_blob` with a token (pin, list, read the kept run, stage, bind) and `expected_id` or
//!   `expected_size` (ROADMAP 9.3.7); half drop their pins right after the bind (nothing but the store's own state
//!   keeps the blob then: the 9.5.10 race), half read the blob back while pinned and drop the pins at the end; two
//!   resumers sometimes share one token;
//! - a fresh `put_blob` (no token) that drops its pins right after the bind;
//! - a reader that reads every blob a document references, whole, twice;
//! - GC: `mark`, then the store's sweep, twice, at the clock's time (every interrupted upload expired).
//!
//! After a round the main thread logs a document for every blob bound (`Blob::Stored`, which checks the store),
//! commits, and deletes a few older documents (garbage for the next rounds). At the end, past the grace period, two
//! GCs leave nothing but what the documents reference.
//!
//! Invariants:
//! 1. Every resume and fresh `put_blob` succeeds with the `BlobRef` of its bytes, and reads back whole while pinned.
//! 2. Every blob bound in a round, and every blob a document references, reads back whole: during the round (the
//!    reader), after it, and at the end. No GC's mark finds a referenced blob missing.
//! 3. Garbage is collected: at the end every manifest is a referenced blob's, and every staged upload is named by a
//!    referenced blob's manifest.
//!
//! In the lagging variant the store has no `strong_list` and lists nothing written after the round's start until the
//! sweep's first upload check, which catches it up (`LaggingList::catch_up`, as `blob_resume.rs` does): the sweep's
//! first listing can miss fresh chunks and manifests, its locked re-list does not (SPEC §9.5: a backend whose two
//! listings both miss a fresh chunk is not covered). Only the first round's first sweep lags (a catch-up is for good).
//!
//! Seeds: `PERSIA_TEST_SEED` (printed) is the base; each run prints its seed and, on a failure, the scheduler's trace.
//! The schedule, and so every outcome, is a function of the seed (repeated runs report the same coverage counts).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::manual_assert_eq
)] // Tests: inputs are generated; `assert!(a == b)` keeps blob bytes out of a failure message.

use std::cell::Cell;
use std::collections::{BTreeSet, HashSet};
use std::io::Read;
use std::ops::Range;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use core::num::NonZeroUsize;

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::{Blob, BlobDatabase};
use persia::internal::gc::{DEFAULT_GRACE, GcOptions};
use persia::internal::pins::PinSet;
use persia::internal::retry::ManualRetryClock;
use persia_blob::gc::{GcObject, PinCheck, PinGate, Sweep, SweepOptions, SweepReport};
use persia_blob::remote::{RemoteBlobStore, RemoteKey, ResumedUpload};
use persia_blob::testing::LaggingList;
use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobRef, BlobStore, ChunkSize, Mark, Result, ResumeToken,
    UploadId, blob_id_of,
};
use persia_engine::db::{Database, DatabaseOptions, SegmentIds};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::{KeyPrefix, MemoryStorage, Storage};

const CHUNK: u32 = ChunkSize::MIN;
const CHUNK_LEN: usize = CHUNK as usize;

#[cfg(not(feature = "slow"))]
const SEEDS: u64 = 6;
#[cfg(feature = "slow")]
const SEEDS: u64 = 64;

#[cfg(not(feature = "slow"))]
const ROUNDS: u64 = 3;
#[cfg(feature = "slow")]
const ROUNDS: u64 = 6;

/// Resumers per round.
const RESUMERS: u64 = 5;

/// Sweeps per round.
const SWEEPS: usize = 3;

/// How often the scheduler lets the last runner go on (see [`Scheduler::pick`]).
const BURST_PERCENT: u64 = 70;

fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x9c5e_0951_0010);
    eprintln!("PERSIA_TEST_SEED={seed}");
    seed
}

/// `SplitMix64`.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `len` bytes from `seed`.
fn bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(len);
    out
}

fn options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(CHUNK).unwrap(),
        ..PutBlobOptions::default()
    }
}

// -- the scheduler: one participant runs at a time, the next picked by a seeded RNG at every step point --

thread_local! {
    /// The participant this thread runs, if any; the main thread is none and never steps.
    static ME: Cell<Option<usize>> = const { Cell::new(None) };
}

#[derive(Debug)]
struct SchedState {
    rng: Rng,
    alive: BTreeSet<usize>,
    waiting: BTreeSet<usize>,
    running: Option<usize>,
    trace: Vec<usize>,
}

/// Runs participants `0..n` one at a time: each blocks at a step point until every live participant is at one (or
/// finished), then the RNG picks who goes on. With every blob store call and sweep pin check a step point, and no
/// participant holding a lock another needs across one, the interleaving is a function of the seed.
#[derive(Debug)]
struct Scheduler {
    state: Mutex<SchedState>,
    cv: Condvar,
}

impl Scheduler {
    fn new(n: usize, seed: u64) -> Self {
        Self {
            state: Mutex::new(SchedState {
                rng: Rng(seed),
                alive: (0..n).collect(),
                waiting: BTreeSet::new(),
                running: None,
                trace: Vec::new(),
            }),
            cv: Condvar::new(),
        }
    }

    fn pick(&self, s: &mut SchedState) {
        if s.running.is_none() && !s.alive.is_empty() && s.waiting.len() == s.alive.len() {
            let waiting: Vec<usize> = s.waiting.iter().copied().collect();
            let next = match s.trace.last() {
                // Bursts: the last runner often goes on, so a whole resume (pin, list, stage, bind, unpin) fits
                // between two steps of a sweep, and a sweep's listing and its checks fall around one.
                Some(&last) if s.waiting.contains(&last) && s.rng.chance(BURST_PERCENT) => last,
                _ => waiting[s.rng.range(0, waiting.len() as u64) as usize],
            };
            s.running = Some(next);
            s.trace.push(next);
            self.cv.notify_all();
        }
    }

    /// Yields: waits until this participant is picked again. A stall (a participant blocked off a step point while
    /// another waits at one) is a harness bug or a deadlock: it aborts with the trace, so a run never hangs.
    fn step(&self, me: usize) {
        let mut s = lock(&self.state);
        if s.running == Some(me) {
            s.running = None;
        }
        s.waiting.insert(me);
        self.pick(&mut s);
        while s.running != Some(me) {
            let (next, timeout) = self
                .cv
                .wait_timeout(s, Duration::from_mins(2))
                .unwrap_or_else(PoisonError::into_inner);
            s = next;
            if timeout.timed_out() && s.running != Some(me) {
                eprintln!("scheduler stalled: {s:?}");
                std::process::abort();
            }
        }
        s.waiting.remove(&me);
    }

    fn finish(&self, me: usize) {
        let mut s = lock(&self.state);
        s.alive.remove(&me);
        s.waiting.remove(&me);
        if s.running == Some(me) {
            s.running = None;
        }
        self.pick(&mut s);
        self.cv.notify_all();
    }

    fn trace(&self) -> Vec<usize> {
        lock(&self.state).trace.clone()
    }
}

/// The scheduler of the current round, shared by the store and the gate.
#[derive(Debug, Default)]
struct Turns(Mutex<Option<Arc<Scheduler>>>);

impl Turns {
    /// A step point: yields if this thread is a participant of a running round.
    fn step(&self) {
        let Some(me) = ME.with(Cell::get) else {
            return;
        };
        let sched = lock(&self.0).clone();
        if let Some(sched) = sched {
            sched.step(me);
        }
    }
}

/// Runs `body` as participant `me`: it waits for its first turn, and finishes (lets the others go on) however it
/// ends, a panic included.
fn participant<T>(sched: &Arc<Scheduler>, me: usize, body: impl FnOnce() -> T) -> T {
    struct Done<'a>(&'a Scheduler, usize);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            ME.with(|c| c.set(None));
            self.0.finish(self.1);
        }
    }
    ME.with(|c| c.set(Some(me)));
    let _done = Done(sched, me);
    sched.step(me);
    body()
}

// -- the store under test: `RemoteBlobStore<S>`, a step point before every call --

struct Stepped<S: Storage> {
    inner: RemoteBlobStore<S>,
    turns: Arc<Turns>,
}

impl<S: Storage> BlobStore for Stepped<S> {
    fn stage_chunk(&self, upload_id: UploadId, index: u32, bytes: &[u8]) -> Result<()> {
        self.turns.step();
        self.inner.stage_chunk(upload_id, index, bytes)
    }

    fn stage_chunks(&self, upload_id: UploadId, first_index: u32, chunks: &[&[u8]]) -> Result<()> {
        // Chunk by chunk, each a step point: a sweep can run between two chunks of one window.
        for (index, bytes) in (first_index..).zip(chunks) {
            self.stage_chunk(upload_id, index, bytes)?;
        }
        Ok(())
    }

    fn bind(&self, manifest: &BlobManifest) -> Result<BindOutcome> {
        self.turns.step();
        self.inner.bind(manifest)
    }

    fn bind_small(&self, manifest: &BlobManifest, bytes: &[u8]) -> Result<BindOutcome> {
        self.turns.step();
        self.inner.bind_small(manifest, bytes)
    }

    fn manifest(&self, blob_id: BlobId) -> Result<Option<BlobManifest>> {
        self.turns.step();
        self.inner.manifest(blob_id)
    }

    fn read_chunk(&self, manifest: &BlobManifest, index: u32) -> Result<Vec<u8>> {
        self.turns.step();
        self.inner.read_chunk(manifest, index)
    }

    fn read_chunks(&self, manifest: &BlobManifest, indices: Range<u32>) -> Result<Vec<Vec<u8>>> {
        self.turns.step();
        self.inner.read_chunks(manifest, indices)
    }

    fn read_chunks_checked(
        &self,
        manifest: &BlobManifest,
        indices: Range<u32>,
        scratch: &mut Vec<u8>,
        sink: &mut dyn FnMut(u32, &[u8]) -> Result<()>,
    ) -> Result<()> {
        self.turns.step();
        self.inner
            .read_chunks_checked(manifest, indices, scratch, sink)
    }

    fn discard_upload(&self, upload_id: UploadId) -> Result<()> {
        self.turns.step();
        self.inner.discard_upload(upload_id)
    }

    fn contains(&self, blob_ref: &BlobRef) -> Result<bool> {
        self.turns.step();
        self.inner.contains(blob_ref)
    }

    fn resumable(&self) -> bool {
        self.inner.resumable()
    }

    fn resume(&self, token: ResumeToken) -> Result<ResumedUpload> {
        self.turns.step();
        BlobStore::resume(&self.inner, token)
    }

    fn restage(&self, from: ResumeToken, chunk_crcs: &[u32], to: UploadId) -> Result<()> {
        self.turns.step();
        BlobStore::restage(&self.inner, from, chunk_crcs, to)
    }
}

impl<S: Storage> Sweep for Stepped<S> {
    fn sweep<P: PinGate>(
        &self,
        mark: &Mark,
        pins: &P,
        options: &SweepOptions,
    ) -> Result<SweepReport> {
        self.inner.sweep(mark, pins, options)
    }
}

/// The writer's pin set as the sweep's gate, with a step point before every pin check (outside the pin set's lock:
/// what the sweep does under it runs in one turn), and a backend catch-up at the first upload check.
struct Gate<'a> {
    pins: &'a PinSet,
    turns: &'a Turns,
    catch_up: Option<&'a (dyn Fn() + Sync)>,
}

impl PinGate for Gate<'_> {
    fn pinned(&self) -> Vec<GcObject> {
        PinGate::pinned(self.pins)
    }

    fn unless_pinned<T>(
        &self,
        object: GcObject,
        delete: impl FnOnce(&dyn PinCheck) -> T,
    ) -> Option<T> {
        self.turns.step();
        if matches!(object, GcObject::Upload(_))
            && let Some(catch_up) = self.catch_up
        {
            catch_up();
        }
        PinGate::unless_pinned(self.pins, object, delete)
    }
}

// -- the scenario --

/// How a token's upload was left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Left {
    /// Chunks `0..k`.
    Run(u32),
    /// Chunks `0..k` and `k + 1`.
    Gap(u32),
    /// Every chunk (a bind that failed with a known outcome): the resume restages it (ROADMAP 9.3.6).
    Full,
    /// Nothing (an unknown upload): the resume starts from chunk 0.
    Nothing,
    /// A short chunk 0 (another blob's): the resume conflicts and restarts under a new upload id.
    ShortZero,
}

/// One resume to run: its token, its blob, whether it checks the id or the size, whether it holds its pins.
#[derive(Debug, Clone)]
struct Resume {
    token: ResumeToken,
    data: Arc<Vec<u8>>,
    by_id: bool,
    hold_pins: bool,
    left: Left,
}

type Db<S> = BlobDatabase<Stepped<S>>;

fn database<S: Storage>(dir: &std::path::Path, storage: S, turns: &Arc<Turns>, seed: u64) -> Db<S> {
    let mut log = Database::create(dir.join("app.persia"), DatabaseOptions::default()).unwrap();
    log.create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    let store = Stepped {
        inner: RemoteBlobStore::new(storage).with_parallelism(NonZeroUsize::MIN),
        turns: Arc::clone(turns),
    };
    let blobs = Blobs::with_upload_ids(Arc::new(store), SegmentIds::seeded(seed))
        .with_retry_clock(Arc::new(ManualRetryClock::new()));
    BlobDatabase::new(log, blobs)
}

fn read_whole<S: Storage>(db: &Db<S>, r: &BlobRef) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::new();
    db.get_blob(r)
        .map_err(|e| format!("{e:?}"))?
        .read_to_end(&mut out)
        .map_err(|e| format!("{e:?}"))?;
    Ok(out)
}

/// Stages the chunks `left` describes for `data` under `upload` straight into the store, as a writer that died left
/// them.
fn leave_behind<S: Storage>(store: &RemoteBlobStore<S>, upload: UploadId, data: &[u8], left: Left) {
    let chunks: Vec<&[u8]> = data.chunks(CHUNK_LEN).collect();
    let n = chunks.len() as u32;
    let indices: Vec<u32> = match left {
        Left::Run(k) => (0..k).collect(),
        Left::Gap(k) => (0..k).chain([k + 1]).collect(),
        Left::Full => (0..n).collect(),
        Left::Nothing => Vec::new(),
        Left::ShortZero => {
            store
                .stage_chunk(upload, 0, &chunks[0][..CHUNK_LEN / 2])
                .unwrap();
            Vec::new()
        }
    };
    for i in indices {
        store.stage_chunk(upload, i, chunks[i as usize]).unwrap();
    }
}

/// The round's resumes: seeded kinds, a few chunks each, two sometimes sharing a token.
fn plan(rng: &mut Rng, round: u64) -> Vec<Resume> {
    let mut out: Vec<Resume> = Vec::new();
    for i in 0..RESUMERS {
        if i > 0 && rng.chance(15) {
            // Twin: a second resume of the previous token, with the same blob.
            let mut twin = out.last().unwrap().clone();
            twin.by_id = rng.chance(50);
            twin.hold_pins = rng.chance(50);
            out.push(twin);
            continue;
        }
        let n = rng.range(2, 7) as u32;
        let len = (n as usize - 1) * CHUNK_LEN + rng.range(1, CHUNK_LEN as u64 + 1) as usize;
        let left = match rng.range(0, 5) {
            0 => Left::Run(rng.range(1, u64::from(n)) as u32),
            1 if n >= 3 => Left::Gap(rng.range(1, u64::from(n) - 1) as u32),
            2 => Left::Full,
            3 => Left::Nothing,
            4 => Left::ShortZero,
            _ => Left::Run(1),
        };
        out.push(Resume {
            token: ResumeToken {
                upload_id: UploadId::new((0xa5 << 120) | (u128::from(round) << 32) | u128::from(i)),
                chunk_size: ChunkSize::new(CHUNK).unwrap(),
            },
            data: Arc::new(bytes(len, rng.next_u64())),
            by_id: rng.chance(50),
            hold_pins: rng.chance(50),
            left,
        });
    }
    out
}

/// What the round's participants bound.
#[derive(Debug)]
struct Bound {
    r: BlobRef,
    data: Arc<Vec<u8>>,
    what: String,
    /// The token's upload, for a resume.
    token: Option<UploadId>,
}

/// What the runs reached: the sweeps' outcomes and where the resumes bound, summed and printed (and the main
/// paths asserted reached, over the fixed seeds).
#[derive(Debug, Default)]
struct Coverage {
    steps: u64,
    sweeps: u64,
    blobs_deleted: u64,
    uploads_deleted: u64,
    chunks_deleted: u64,
    pinned: u64,
    young: u64,
    changed: u64,
    /// Resumes bound under the token's upload, and under another (a 9.3.6 restage, a conflict).
    same_upload: u64,
    moved_upload: u64,
}

impl Coverage {
    fn add(&mut self, s: &SweepReport) {
        self.sweeps += 1;
        self.blobs_deleted += s.blobs_deleted;
        self.uploads_deleted += s.uploads_deleted;
        self.chunks_deleted += s.chunks_deleted;
        self.pinned += s.pinned;
        self.young += s.young;
        self.changed += s.changed;
    }

    /// The paths these fixed seeds reach: a sweep deleting an expired upload beside the resumes, one keeping an upload
    /// a resume pinned, one keeping an upload whose locked re-list changed (ROADMAP 9.5.10), resumes bound under
    /// the token's upload and under another. Reaching them is a function of
    /// the seeds; a change of the schedule that loses one fails here rather than silently testing less.
    fn assert_reached(&self, name: &str) {
        assert!(
            self.uploads_deleted > 0
                && self.pinned > 0
                && self.changed > 0
                && self.same_upload > 0
                && self.moved_upload > 0,
            "{name}: a path was not reached: {self:?}"
        );
    }

    fn merge(&mut self, o: &Self) {
        self.steps += o.steps;
        self.sweeps += o.sweeps;
        self.blobs_deleted += o.blobs_deleted;
        self.uploads_deleted += o.uploads_deleted;
        self.chunks_deleted += o.chunks_deleted;
        self.pinned += o.pinned;
        self.young += o.young;
        self.changed += o.changed;
        self.same_upload += o.same_upload;
        self.moved_upload += o.moved_upload;
    }
}

struct Variant<'a> {
    name: &'static str,
    catch_up: Option<&'a (dyn Fn() + Sync)>,
}

/// One run of `ROUNDS` rounds on `db`, whose store's clock is `clock`.
fn run<S: Storage>(
    seed: u64,
    db: &mut Db<S>,
    clock: &ManualClock,
    turns: &Arc<Turns>,
    variant: &Variant<'_>,
) -> Coverage {
    let mut coverage = Coverage::default();
    let mut rng = Rng(seed);
    let mut documents: Vec<(String, BlobRef, Arc<Vec<u8>>)> = Vec::new();
    let mut next_doc = 0_u64;
    let hour = Duration::from_hours(1);
    for round in 0..ROUNDS {
        let ctx = format!("{} seed {seed} round {round}", variant.name);
        let resumes = plan(&mut rng, round);
        let mut left_behind = HashSet::new();
        for resume in &resumes {
            if left_behind.insert(resume.token.upload_id) {
                leave_behind(
                    &db.blobs().store().inner,
                    resume.token.upload_id,
                    &resume.data,
                    resume.left,
                );
            }
        }
        // Every upload left behind (and every orphan of earlier rounds) is expired from here on.
        clock.advance(DEFAULT_GRACE + hour);
        let fresh = Arc::new(bytes(
            2 * CHUNK_LEN + rng.range(1, 1000) as usize,
            rng.next_u64(),
        ));
        let referenced: Vec<(BlobRef, Arc<Vec<u8>>)> = documents
            .iter()
            .map(|(_, r, d)| (r.clone(), Arc::clone(d)))
            .collect();

        // Participants: the resumers, the fresh put, the reader, GC.
        let n = resumes.len() + 3;
        let sched = Arc::new(Scheduler::new(n, rng.next_u64()));
        *lock(&turns.0) = Some(Arc::clone(&sched));
        let shared: &Db<S> = db;
        let bound: Mutex<Vec<Bound>> = Mutex::new(Vec::new());
        let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let gc_reports: Mutex<Vec<SweepReport>> = Mutex::new(Vec::new());
        let now = clock.now();
        // GC starts after a seeded number of turns, so its first listing may come after resumes wrote (and, lagging,
        // miss what they wrote).
        let gc_delay = rng.range(0, 8);
        std::thread::scope(|scope| {
            for (me, resume) in resumes.iter().enumerate() {
                let (sched, bound, failures, ctx) = (&sched, &bound, &failures, &ctx);
                scope.spawn(move || {
                    participant(sched, me, || {
                        let check = if resume.by_id {
                            PutBlobOptions {
                                expected_id: Some(blob_id_of(&resume.data)),
                                ..options()
                            }
                        } else {
                            PutBlobOptions {
                                expected_size: Some(resume.data.len() as u64),
                                ..options()
                            }
                        };
                        let what = format!(
                            "resume {me} {:?} of {}",
                            resume.left, resume.token.upload_id
                        );
                        let opts = PutBlobOptions {
                            resume: Some(resume.token),
                            ..check
                        };
                        let (r, pins) = match shared.put_blob(&resume.data[..], &opts) {
                            Ok(bound) => bound,
                            Err(e) => {
                                lock(failures).push(format!("{ctx}: {what}: {e:?}"));
                                return;
                            }
                        };
                        if r.blob_id() != blob_id_of(&resume.data)
                            || r.size() != resume.data.len() as u64
                        {
                            lock(failures).push(format!("{ctx}: {what}: bound {r:?}"));
                        }
                        if resume.hold_pins {
                            match read_whole(shared, &r) {
                                Ok(out) if out == *resume.data => {}
                                Ok(_) => lock(failures)
                                    .push(format!("{ctx}: {what}: read other bytes while pinned")),
                                Err(e) => lock(failures)
                                    .push(format!("{ctx}: {what}: read while pinned: {e}")),
                            }
                        }
                        drop(pins);
                        lock(bound).push(Bound {
                            r,
                            data: Arc::clone(&resume.data),
                            what,
                            token: Some(resume.token.upload_id),
                        });
                    });
                });
            }
            let me = resumes.len();
            let (sched_ref, bound_ref, failures_ref, ctx_ref, fresh_ref) =
                (&sched, &bound, &failures, &ctx, &fresh);
            scope.spawn(move || {
                participant(sched_ref, me, || {
                    match shared.put_blob(&fresh_ref[..], &options()) {
                        Ok((r, pins)) => {
                            drop(pins);
                            lock(bound_ref).push(Bound {
                                r,
                                data: Arc::clone(fresh_ref),
                                what: "fresh put".to_owned(),
                                token: None,
                            });
                        }
                        Err(e) => lock(failures_ref).push(format!("{ctx_ref}: fresh put: {e:?}")),
                    }
                });
            });
            let referenced = &referenced;
            scope.spawn(move || {
                participant(sched_ref, me + 1, || {
                    for pass in 0..2 {
                        for (r, data) in referenced {
                            match read_whole(shared, r) {
                                Ok(out) if out == **data => {}
                                Ok(_) => lock(failures_ref).push(format!(
                                    "{ctx_ref}: reader pass {pass}: {r:?}: other bytes"
                                )),
                                Err(e) => lock(failures_ref)
                                    .push(format!("{ctx_ref}: reader pass {pass}: {r:?}: {e}")),
                            }
                        }
                    }
                });
            });
            let gc_ref = &gc_reports;
            let turns_ref = &**turns;
            scope.spawn(move || {
                participant(sched_ref, me + 2, || {
                    for _ in 0..gc_delay {
                        turns_ref.step();
                    }
                    for _ in 0..SWEEPS {
                        let mark = match shared.mark() {
                            Ok(mark) => mark,
                            Err(e) => {
                                lock(failures_ref).push(format!("{ctx_ref}: mark: {e:?}"));
                                return;
                            }
                        };
                        if !mark.missing.is_empty() {
                            lock(failures_ref).push(format!(
                                "{ctx_ref}: the mark misses referenced blobs {:?}",
                                mark.missing
                            ));
                        }
                        let gate = Gate {
                            pins: shared.blobs().pin_set(),
                            turns: turns_ref,
                            catch_up: variant.catch_up,
                        };
                        let options = SweepOptions {
                            grace: DEFAULT_GRACE,
                            now,
                            rewrite_ratio: 0.5,
                        };
                        match shared.blobs().store().inner.sweep(&mark, &gate, &options) {
                            Ok(report) => lock(gc_ref).push(report),
                            Err(e) => lock(failures_ref).push(format!("{ctx_ref}: sweep: {e:?}")),
                        }
                    }
                });
            });
        });
        *lock(&turns.0) = None;
        let trace = sched.trace();
        coverage.steps += trace.len() as u64;
        for s in gc_reports.lock().unwrap().iter() {
            coverage.add(s);
        }
        let failures = failures.into_inner().unwrap();
        assert!(
            failures.is_empty(),
            "{ctx}: {failures:#?}\nplan {resumes:#?}\nsweeps {:#?}\ntrace ({} steps) {trace:?}",
            gc_reports.lock().unwrap(),
            trace.len()
        );

        // Invariant 2 after the round: every bound blob is still bound and whole; a document now references it.
        for b in bound.into_inner().unwrap() {
            let id = format!("d{next_doc}");
            next_doc += 1;
            let logged = db.put(
                "c",
                &Document::new(&id),
                Blob::Stored(b.r.clone()),
                Durability::Fsync,
            );
            assert!(
                logged.is_ok(),
                "{ctx}: {}: the bound blob is gone: {logged:?}\nplan {resumes:#?}\nsweeps {:#?}\ntrace {trace:?}",
                b.what,
                gc_reports.lock().unwrap(),
            );
            db.release_durable();
            let out = read_whole(db, &b.r).unwrap_or_else(|e| {
                panic!(
                    "{ctx}: {}: read after the round: {e}\ntrace {trace:?}",
                    b.what
                )
            });
            assert!(
                out == *b.data,
                "{ctx}: {}: other bytes after the round",
                b.what
            );
            if let Some(token) = b.token {
                let upload = db
                    .blobs()
                    .store()
                    .inner
                    .manifest(b.r.blob_id())
                    .unwrap()
                    .unwrap()
                    .upload_id();
                if upload == token {
                    coverage.same_upload += 1;
                } else {
                    coverage.moved_upload += 1;
                }
            }
            documents.push((id, b.r, b.data));
        }
        db.commit().unwrap();
        // Garbage for the next rounds: a few older documents deleted.
        for _ in 0..2 {
            if documents.len() > 3 {
                let i = rng.range(0, documents.len() as u64) as usize;
                let (id, _, _) = documents.swap_remove(i);
                db.log_mut().delete("c", &id, Durability::Fsync).unwrap();
                db.release_durable();
            }
        }
        db.commit().unwrap();
    }

    // Invariant 3: past the grace period, with nothing pinned, two GCs leave only what documents reference.
    for _ in 0..2 {
        clock.advance(DEFAULT_GRACE + hour);
        let report = db
            .gc(&GcOptions {
                now: Some(clock.now()),
                ..GcOptions::default()
            })
            .unwrap();
        assert!(report.missing.is_empty(), "seed {seed}: {report:?}");
    }
    let referenced: HashSet<BlobId> = documents.iter().map(|(_, r, _)| r.blob_id()).collect();
    let store = &db.blobs().store().inner;
    let named: HashSet<UploadId> = referenced
        .iter()
        .map(|id| store.manifest(*id).unwrap().unwrap().upload_id())
        .collect();
    let listed = store
        .storage()
        .list(&KeyPrefix::new("blobs/").unwrap())
        .unwrap();
    for meta in &listed {
        match RemoteKey::parse(&meta.key) {
            Some(RemoteKey::Manifest(id)) => assert!(
                referenced.contains(&id),
                "{} seed {seed}: an orphan manifest survived: {}",
                variant.name,
                meta.key.as_str()
            ),
            Some(RemoteKey::Chunk { upload_id, .. }) => assert!(
                named.contains(&upload_id),
                "{} seed {seed}: an orphan upload survived: {}",
                variant.name,
                meta.key.as_str()
            ),
            None => panic!("a foreign key {}", meta.key.as_str()),
        }
    }
    for (id, r, data) in &documents {
        let out = read_whole(db, r).unwrap_or_else(|e| panic!("seed {seed}: {id} at the end: {e}"));
        assert!(out == **data, "seed {seed}: {id}: other bytes at the end");
    }
    coverage
}

fn clocked() -> (Arc<ManualClock>, MemoryStorage) {
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_hours(1_000)));
    let storage = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    (clock, storage)
}

#[test]
fn sweeps_interleaved_with_resumes_keep_every_bound_blob_on_a_strong_listing() {
    let base = base_seed();
    let mut total = Coverage::default();
    for i in 0..SEEDS {
        let seed = base.wrapping_add(i);
        eprintln!("strong seed {seed}");
        let dir = tempfile::tempdir().unwrap();
        let turns = Arc::new(Turns::default());
        let (clock, storage) = clocked();
        let mut db = database(dir.path(), storage, &turns, seed);
        let variant = Variant {
            name: "strong",
            catch_up: None,
        };
        total.merge(&run(seed, &mut db, &clock, &turns, &variant));
    }
    eprintln!("strong: {total:?}");
    total.assert_reached("strong");
}

#[test]
fn sweeps_interleaved_with_resumes_keep_every_bound_blob_on_a_lagging_listing() {
    let base = base_seed();
    let mut hidden = 0;
    let mut total = Coverage::default();
    for i in 0..SEEDS {
        let seed = base.wrapping_add(i).wrapping_add(1 << 32);
        eprintln!("lagging seed {seed}");
        let dir = tempfile::tempdir().unwrap();
        let turns = Arc::new(Turns::default());
        let (clock, storage) = clocked();
        // Lagging from the first round's start: its interrupted uploads are written before, and visible.
        let lagging = Arc::new(LaggingList::new(
            storage,
            clock.now() + Duration::from_secs(1),
        ));
        let mut db = database(dir.path(), Arc::clone(&lagging), &turns, seed);
        let catch_up = || lagging.catch_up();
        let variant = Variant {
            name: "lagging",
            catch_up: Some(&catch_up),
        };
        total.merge(&run(seed, &mut db, &clock, &turns, &variant));
        hidden += lagging.hidden();
    }
    // Whether a given schedule's first listing missed a fresh object depends on the seed; these seeds reach it.
    eprintln!("objects the lagging listings left out: {hidden}");
    eprintln!("lagging: {total:?}");
    total.assert_reached("lagging");
    assert!(hidden > 0, "no listing lagged: the variant tested nothing");
}
