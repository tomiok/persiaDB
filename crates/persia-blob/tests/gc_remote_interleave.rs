//! The remote sweep interleaved with a resume, step by step (SPEC §9.4, §9.5; ROADMAP 9.5.10; ADR-0051 item 5,
//! ADR-0053).
//!
//! A resume of an old upload is modeled as the steps `put_blob` makes on the store and the pin set: pin the token's
//! upload, list it, stage the chunks its listing lacks (or, when nothing would be new, pin a fresh upload and restage
//! there, ROADMAP 9.3.6), pin the blob, bind it, drop every pin. The sweep runs over a backend without `strong_list`
//! (`Lagging`, as `persia_blob::testing::LaggingList`, which needs the `testing` feature: a listing misses every object
//! written after the old chunks until it catches up), and after each of
//! its storage requests a seeded schedule runs some of the resume's next steps. Steps that take the pin set's lock
//! (pin, unpin) run only while the sweep does not hold it; staging and binding may run at any point, the locked
//! check included. The backend catches up at a seeded point, at the latest when the sweep takes the lock to check an
//! upload (SPEC §9.5: a backend whose two listings both miss a just-written chunk is not covered). Two decoys, an
//! unbound upload and an unreferenced bound blob, both expired, give the sweep real work.
//!
//! Invariants:
//! 1. A blob the resume bound reads back whole after the sweep, and after a later sweep that marks it (it is
//!    referenced by its `BlobRef` once bound): no bound, referenced blob ever loses a chunk.
//! 2. The decoys are deleted by the first sweep; after the later sweep `blobs/` holds exactly the bound blob's
//!    manifest and the chunks of the upload it names.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated.

mod gc_support;

use core::ops::Range;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use gc_support::{Pins, data_of, manifest_of, put, read_all};
use persia_blob::gc::{GcObject, Mark, PinCheck, PinGate, Sweep, SweepOptions};
use persia_blob::remote::{RemoteBlobStore, RemoteKey};
use persia_blob::{BindOutcome, BlobManifest, BlobStore, ChunkSize, ResumeToken, UploadId};
use persia_format::ByteSource;
use persia_format::checksum::crc32c;
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::{
    Capabilities, Key, KeyPrefix, MemoryStorage, ObjectMeta, PutOutcome, Storage,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

const CHUNK: u32 = 4;
const GRACE: Duration = Duration::from_secs(3600);
const TOKEN: u128 = 0x7070;
const FRESH: u128 = 0x7171;
const DECOY_UPLOAD: u128 = 0x9090;
const DECOY_BLOB: u128 = 0x9191;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One step of the resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    PinUpload,
    List,
    Stage(u32),
    PinFresh,
    /// Copy chunks `0..n` of the token's upload under the fresh one (`RemoteBlobStore::restage`).
    Restage(u32),
    /// Stage chunk `i` under the fresh upload.
    StageFresh(u32),
    PinBlob,
    Bind,
    Unpin,
}

impl Step {
    /// Whether the step takes the pin set's lock (so it cannot run while the sweep holds it).
    fn locks(self) -> bool {
        matches!(
            self,
            Self::PinUpload | Self::PinFresh | Self::PinBlob | Self::Unpin
        )
    }
}

#[derive(Debug, Clone)]
struct Case {
    /// Chunks of the blob (the last may be short) and how many the interrupted upload staged.
    chunks: u32,
    kept: u32,
    short: bool,
    /// Resume steps run before the sweep starts.
    before: usize,
    /// After yield point `at`, run up to `steps` resume steps.
    schedule: Vec<(usize, usize)>,
    /// The yield point at which the backend's listings catch up.
    catch_up: usize,
}

fn case() -> impl Strategy<Value = Case> {
    (1..=5_u32, any::<bool>())
        .prop_flat_map(|(chunks, short)| {
            (
                Just(chunks),
                0..=chunks,
                Just(short),
                prop_oneof![3 => Just(0_usize), 1 => 0..8_usize],
                prop::collection::vec((0..40_usize, 1..5_usize), 0..6),
                0..40_usize,
            )
        })
        .prop_map(|(chunks, kept, short, before, schedule, catch_up)| Case {
            chunks,
            kept,
            short,
            before,
            schedule,
            catch_up,
        })
}

/// A backend without `strong_list` (SPEC §10.1): until [`Lagging::catch_up`], a listing misses every object written
/// after `since`. `persia_blob::testing::LaggingList` is this, behind the `testing` feature.
struct Lagging<S> {
    inner: S,
    since: SystemTime,
    behind: AtomicBool,
}

impl<S> Lagging<S> {
    fn new(inner: S, since: SystemTime) -> Self {
        Self {
            inner,
            since,
            behind: AtomicBool::new(true),
        }
    }

    fn catch_up(&self) {
        self.behind.store(false, Ordering::SeqCst);
    }
}

impl<S: Storage> Storage for Lagging<S> {
    fn get_range(&self, key: &Key, range: Range<u64>) -> persia_storage::Result<ByteSource> {
        self.inner.get_range(key, range)
    }
    fn get(&self, key: &Key) -> persia_storage::Result<Bytes> {
        self.inner.get(key)
    }
    fn put(&self, key: &Key, data: Bytes) -> persia_storage::Result<()> {
        self.inner.put(key, data)
    }
    fn put_if_absent(&self, key: &Key, data: Bytes) -> persia_storage::Result<PutOutcome> {
        self.inner.put_if_absent(key, data)
    }
    fn list(&self, prefix: &KeyPrefix) -> persia_storage::Result<Vec<ObjectMeta>> {
        let mut listed = self.inner.list(prefix)?;
        if self.behind.load(Ordering::SeqCst) {
            listed.retain(|meta| meta.last_modified.is_none_or(|t| t <= self.since));
        }
        Ok(listed)
    }
    fn delete(&self, key: &Key) -> persia_storage::Result<()> {
        self.inner.delete(key)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            strong_list: false,
            ..self.inner.capabilities()
        }
    }
}

/// The resume, its schedule, and where the sweep is.
struct Resume {
    /// The resume's view of the store: the same objects, listed strongly.
    store: RemoteBlobStore<Arc<MemoryStorage>>,
    pins: Arc<Pins>,
    lagging: Arc<Lagging<Arc<MemoryStorage>>>,
    data: Vec<u8>,
    token: ResumeToken,
    manifest: Option<BlobManifest>,
    steps: VecDeque<Step>,
    done: Vec<Step>,
    /// Steps to run after each yield point.
    schedule: BTreeMap<usize, usize>,
    catch_up: usize,
    yields: usize,
}

impl Resume {
    fn chunk(&self, index: u32) -> &[u8] {
        self.data
            .chunks(CHUNK as usize)
            .nth(index as usize)
            .unwrap()
    }

    fn crcs(&self) -> Vec<u32> {
        self.data.chunks(CHUNK as usize).map(crc32c).collect()
    }

    fn chunk_count(&self) -> u32 {
        self.data.chunks(CHUNK as usize).count() as u32
    }

    /// Runs the next step; `false` when it must wait for the pin set's lock, or nothing is left.
    fn step(&mut self, locked: bool) -> bool {
        let Some(&next) = self.steps.front() else {
            return false;
        };
        if locked && next.locks() {
            return false;
        }
        self.steps.pop_front();
        self.done.push(next);
        let upload = self.token.upload_id;
        let fresh = UploadId::new(FRESH);
        match next {
            Step::PinUpload => self.pins.pin(GcObject::Upload(upload)),
            Step::List => {
                // The kept run: full chunks from 0, as listed now.
                let listed: BTreeSet<u32> = self
                    .store
                    .staged_chunks(upload)
                    .unwrap()
                    .into_iter()
                    .map(|c| c.index)
                    .collect();
                let n = self.chunk_count();
                let missing: Vec<u32> = (0..n).filter(|i| !listed.contains(i)).collect();
                let mut plan: Vec<Step> = if missing.is_empty() {
                    // ROADMAP 9.3.6: nothing would be new, so the chunks move to a fresh upload first.
                    vec![
                        Step::PinFresh,
                        Step::Restage(n - 1),
                        Step::StageFresh(n - 1),
                    ]
                } else {
                    missing.into_iter().map(Step::Stage).collect()
                };
                plan.extend([Step::PinBlob, Step::Bind, Step::Unpin]);
                self.steps.extend(plan);
                let to = if self.steps.contains(&Step::PinFresh) {
                    FRESH
                } else {
                    upload.get()
                };
                self.manifest = Some(manifest_of(&self.data, CHUNK, to));
            }
            Step::Stage(i) => self.store.stage_chunk(upload, i, self.chunk(i)).unwrap(),
            Step::PinFresh => self.pins.pin(GcObject::Upload(fresh)),
            Step::Restage(n) => {
                let crcs = self.crcs();
                self.store
                    .restage(self.token, &crcs[..n as usize], fresh)
                    .unwrap();
            }
            Step::StageFresh(i) => self.store.stage_chunk(fresh, i, self.chunk(i)).unwrap(),
            Step::PinBlob => {
                let id = self.manifest.as_ref().unwrap().blob_id();
                self.pins.pin(GcObject::Blob(id));
            }
            Step::Bind => {
                let m = self.manifest.as_ref().unwrap();
                assert_eq!(self.store.bind(m).unwrap(), BindOutcome::Bound);
            }
            Step::Unpin => {
                let id = self.manifest.as_ref().unwrap().blob_id();
                self.pins.unpin(GcObject::Upload(upload));
                self.pins.unpin(GcObject::Upload(fresh));
                self.pins.unpin(GcObject::Blob(id));
            }
        }
        true
    }

    /// A yield point of the sweep.
    fn yield_point(&mut self, locked: bool, upload_check: bool) {
        let at = self.yields;
        self.yields += 1;
        if at >= self.catch_up || upload_check {
            self.lagging.catch_up();
        }
        let budget = self.schedule.get(&at).copied().unwrap_or(0);
        for _ in 0..budget {
            if !self.step(locked) {
                break;
            }
        }
    }
}

/// The sweep's backend: after each request (but the empty listings of manifest prefixes), a yield point.
struct Yielding {
    inner: Arc<Lagging<Arc<MemoryStorage>>>,
    resume: Arc<Mutex<Resume>>,
    locked: Arc<AtomicBool>,
}

impl Yielding {
    fn after<T>(&self, result: T) -> T {
        lock(&self.resume).yield_point(self.locked.load(Ordering::SeqCst), false);
        result
    }
}

impl Storage for Yielding {
    fn get_range(&self, key: &Key, range: Range<u64>) -> persia_storage::Result<ByteSource> {
        self.after(self.inner.get_range(key, range))
    }
    fn get(&self, key: &Key) -> persia_storage::Result<Bytes> {
        self.after(self.inner.get(key))
    }
    fn put(&self, key: &Key, data: Bytes) -> persia_storage::Result<()> {
        self.after(self.inner.put(key, data))
    }
    fn put_if_absent(&self, key: &Key, data: Bytes) -> persia_storage::Result<PutOutcome> {
        self.after(self.inner.put_if_absent(key, data))
    }
    fn list(&self, prefix: &KeyPrefix) -> persia_storage::Result<Vec<ObjectMeta>> {
        let listed = self.inner.list(prefix);
        if listed.as_ref().is_ok_and(Vec::is_empty)
            && !prefix.as_str().starts_with("blobs/uploads/")
        {
            return listed;
        }
        self.after(listed)
    }
    fn delete(&self, key: &Key) -> persia_storage::Result<()> {
        self.after(self.inner.delete(key))
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

/// The writer's pin set, telling the backend while the sweep holds its lock; a yield point before each check.
struct Gate {
    pins: Arc<Pins>,
    locked: Arc<AtomicBool>,
    resume: Arc<Mutex<Resume>>,
}

impl PinGate for Gate {
    fn pinned(&self) -> Vec<GcObject> {
        self.pins.pinned()
    }

    fn unless_pinned<T>(
        &self,
        object: GcObject,
        delete: impl FnOnce(&dyn PinCheck) -> T,
    ) -> Option<T> {
        let upload = matches!(object, GcObject::Upload(_));
        lock(&self.resume).yield_point(false, upload);
        self.pins.unless_pinned(object, |view| {
            self.locked.store(true, Ordering::SeqCst);
            let out = delete(view);
            self.locked.store(false, Ordering::SeqCst);
            out
        })
    }
}

fn keys<S: Storage>(storage: &S) -> BTreeSet<String> {
    storage
        .list(&KeyPrefix::new("blobs/").unwrap())
        .unwrap()
        .into_iter()
        .map(|m| m.key.as_str().to_owned())
        .collect()
}

fn run(case: &Case) {
    let t0 = UNIX_EPOCH + Duration::from_secs(1_000_000);
    let clock = Arc::new(ManualClock::new(t0));
    let memory = Arc::new(MemoryStorage::with_clock(
        Arc::clone(&clock) as Arc<dyn Clock>
    ));
    let lagging = Arc::new(Lagging::new(Arc::clone(&memory), t0));
    let strong = RemoteBlobStore::new(Arc::clone(&memory));
    let len = CHUNK as usize * case.chunks as usize - usize::from(case.short);
    let data = data_of(len, 41);
    let token = ResumeToken {
        upload_id: UploadId::new(TOKEN),
        chunk_size: ChunkSize::from_encoding(CHUNK).unwrap(),
    };
    // The interrupted upload's chunks, and the decoys, all written long ago.
    for (i, c) in data
        .chunks(CHUNK as usize)
        .enumerate()
        .take(case.kept as usize)
    {
        strong.stage_chunk(token.upload_id, i as u32, c).unwrap();
    }
    let decoy_upload = gc_support::stage(&strong, &data_of(9, 7), CHUNK, DECOY_UPLOAD);
    let decoy_blob = put(&strong, &data_of(11, 8), CHUNK, DECOY_BLOB);
    clock.set(t0 + 2 * GRACE);

    let pins = Arc::new(Pins::default());
    let locked = Arc::new(AtomicBool::new(false));
    let resume = Arc::new(Mutex::new(Resume {
        store: RemoteBlobStore::new(Arc::clone(&memory)),
        pins: Arc::clone(&pins),
        lagging: Arc::clone(&lagging),
        data: data.clone(),
        token,
        manifest: None,
        steps: VecDeque::from([Step::PinUpload, Step::List]),
        done: Vec::new(),
        schedule: case.schedule.iter().copied().collect(),
        catch_up: case.catch_up,
        yields: 0,
    }));
    for _ in 0..case.before {
        lock(&resume).step(false);
    }
    let sweeper = RemoteBlobStore::new(Yielding {
        inner: Arc::clone(&lagging),
        resume: Arc::clone(&resume),
        locked: Arc::clone(&locked),
    });
    let gate = Gate {
        pins: Arc::clone(&pins),
        locked,
        resume: Arc::clone(&resume),
    };
    let options = |now: SystemTime| SweepOptions {
        grace: GRACE,
        now,
        rewrite_ratio: 0.5,
    };
    let report = sweeper
        .sweep(&Mark::default(), &gate, &options(clock.now()))
        .unwrap();
    let done_in_sweep = lock(&resume).done.clone();
    // The resume ends, the backend catches up.
    while lock(&resume).step(false) {}
    lagging.catch_up();
    let manifest = lock(&resume).manifest.clone().unwrap();
    let context =
        || format!("{case:?}: resume steps during the sweep {done_in_sweep:?}, {report:?}");
    assert_eq!(pins.pinned(), Vec::new());

    // 1. The bound blob reads back whole.
    let read = read_all(&strong, &manifest);
    assert!(
        read.as_deref().is_ok_and(|r| r == data),
        "{}: {read:?}",
        context()
    );
    // 2. The decoys went in the first sweep (an unpinned expired upload whose listing did not change).
    assert!(
        strong
            .staged_chunks(decoy_upload.upload_id())
            .unwrap()
            .is_empty(),
        "{}",
        context()
    );
    assert!(
        strong.manifest(decoy_blob.blob_id()).unwrap().is_none(),
        "{}",
        context()
    );
    assert!(
        strong
            .staged_chunks(decoy_blob.upload_id())
            .unwrap()
            .is_empty(),
        "{}",
        context()
    );

    // A later sweep, the blob referenced by a document (marked), past the grace period of every chunk.
    clock.set(clock.now() + 2 * GRACE);
    let mut mark = Mark::new([manifest.blob_id()], []);
    mark.add_manifest_uploads(&strong).unwrap();
    strong
        .sweep(&mark, &Pins::default(), &options(clock.now()))
        .unwrap();
    let read = read_all(&strong, &manifest);
    assert!(
        read.as_deref().is_ok_and(|r| r == data),
        "{}: after a later sweep: {read:?}",
        context()
    );
    let mut expected: BTreeSet<String> = (0..manifest.chunk_count())
        .map(|index| {
            RemoteKey::Chunk {
                upload_id: manifest.upload_id(),
                index,
            }
            .key()
            .unwrap()
            .as_str()
            .to_owned()
        })
        .collect();
    expected.insert(
        RemoteKey::Manifest(manifest.blob_id())
            .key()
            .unwrap()
            .as_str()
            .to_owned(),
    );
    assert_eq!(keys(&*memory), expected, "{}", context());
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 20_000 } else { 768 },
        rng_seed: RngSeed::Fixed(9510),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn no_bound_referenced_blob_loses_a_chunk_whatever_the_resume_does_during_the_sweep(case in case()) {
        run(&case);
    }
}

/// The interleaving ROADMAP 9.5.10 was found with, for each listing lag: the sweep lists the uploads (the old
/// chunks, expired), then the whole resume runs before its locked check.
#[test]
fn the_whole_resume_between_the_upload_listing_and_its_check() {
    for catch_up in [0, 39] {
        for (chunks, kept) in [(4, 2), (4, 4), (1, 0), (3, 1)] {
            // Yield points: the decoy manifest's listing (0), its read... the upload listing comes before the first
            // upload check; run every step at each point up to there, one schedule per point.
            for at in 0..12 {
                run(&Case {
                    chunks,
                    kept,
                    short: false,
                    before: 0,
                    schedule: vec![(at, 64)],
                    catch_up,
                });
            }
        }
    }
}
