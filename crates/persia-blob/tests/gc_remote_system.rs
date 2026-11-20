//! System-level tests of the remote sweep (SPEC §9.4, §9.5, §10.1; ROADMAP 9.5.2; ADR-0052) over the real local
//! backend: `LocalStorage` on a temporary directory, so real files, real modification times and real deletes.
//!
//! Ages are the files' real modification times. Objects written "long ago" are aged after they are written with
//! `File::set_modified` (never by waiting), and a sweep measures ages at the system clock's `now`. Where only marks
//! and pins may protect anything, the sweep runs at a `now` a month ahead instead, so every object is expired.
//!
//! The writer's side (`put_blob`, `get_blob`, the pin set, the references) is emulated here after SPEC §9.5 and
//! ADR-0051: `persia`'s `Blobs` and `PinSet` cannot run over a remote store in tests while `persia` has no
//! `persia-storage` dev-dependency (ROADMAP 9.5.9 covers the real `PinSet`).
//!
//! Invariants:
//! 1. A seeded workload (blobs inline, empty and multi-chunk at the real 64 KiB chunk floor; referenced, orphaned,
//!    pinned; uploads abandoned mid-staging, with gaps, half old and half young, a duplicate's leftover, a
//!    `put_blob` in flight; keys under `blobs/` that Persia did not write): one sweep deletes exactly the keys the
//!    model says (unmarked and expired), the report counts exactly them, foreign keys keep their bytes, every
//!    surviving blob reads back byte for byte, a second sweep deletes nothing, and once every reference and pin is
//!    gone and every object is aged, only the foreign keys are left.
//! 2. A crash loop: the n-th delete of a sweep fails, for every n, as a crash before or after the delete is applied
//!    (`FaultyStorage` over `LocalStorage`; with `slow`, also an I/O error and a lost response). After each, no
//!    manifest lacks a chunk, every surviving blob reads back, nothing unexpected was deleted; after a restart (a new
//!    store over the same directory) a second sweep converges on the fault-free end state.
//! 3. Concurrency: `put_blob` and `get_blob` / `get_blob_range` (emulated with the writer's pin protocol, SPEC §9.5,
//!    ADR-0051 item 5, over a reference-counted pin set) on other threads while sweeps run over the same store and
//!    pins, with duplicates of orphans being swept. No sweep fails; no pinned or referenced blob is ever unreadable;
//!    a read is the exact bytes or `BlobNotFound`; no manifest ever lacks a chunk; at the end, with everything
//!    expired, exactly the committed blobs (and the foreign keys) are left.
//!    Then, deterministically: a whole sweep at every point (before and after each storage call) of one `get_blob`,
//!    `get_blob_range`, `put(doc, BlobRef)` lookup or `put_blob` (fresh, or a duplicate of an orphan being swept):
//!    with everything expired, the operation gets its exact bytes and what it pinned survives a further sweep.
//! 4. (`slow`) Scale smoke: about 10k objects; the sweep's time is printed.
//!
//! Seeds: [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) is printed; replay a failure with
//! `PERSIA_TEST_SEED=<seed> cargo test -p persia-blob --test gc_remote_system`. `--features slow` runs more.

#![allow(
    clippy::too_many_lines,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::string_slice,
    missing_docs
)]

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use common::{blob_ref, keys_of, manifest_of};
use persia_blob::gc::{GcObject, Mark, PinCheck, PinGate, Sweep, SweepOptions, SweepReport};
use persia_blob::remote::RemoteKey;
use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobStore, ChunkSize, Error, RemoteBlobStore,
    SMALL_BLOB_MAX, UploadId, blob_id_of, read_manifest_range, read_range,
};
use persia_storage::faulty::{Fault, FaultyStorage, Trigger};
use persia_storage::{Key, LocalStorage, Op, Storage};

const HOUR: Duration = Duration::from_hours(1);
/// SPEC §9.5's default grace period.
const GRACE: Duration = Duration::from_hours(24);
/// How far back "long ago" objects are aged: twice the grace period.
const AGED: Duration = Duration::from_hours(48);
/// The writer's chunk size floor (SPEC §9.2): what `put_blob` uses at the least.
const CHUNK: u32 = ChunkSize::MIN;

fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_0952_0002);
    eprintln!("PERSIA_TEST_SEED={seed}");
    seed
}

/// `SplitMix64`: small, seedable, good enough for test workloads.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below((hi - lo + 1) as u64) as usize
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len + 8);
        while v.len() < len {
            v.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        v.truncate(len);
        v
    }
}

fn local(root: &Path) -> RemoteBlobStore<LocalStorage> {
    RemoteBlobStore::new(LocalStorage::open(root).unwrap())
}

fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

fn manifest_key(id: BlobId) -> String {
    RemoteKey::Manifest(id).key().unwrap().as_str().to_owned()
}

fn chunk_key(upload: u128, index: u32) -> String {
    RemoteKey::Chunk {
        upload_id: UploadId::new(upload),
        index,
    }
    .key()
    .unwrap()
    .as_str()
    .to_owned()
}

fn options(now: SystemTime) -> SweepOptions {
    SweepOptions {
        grace: GRACE,
        now,
        rewrite_ratio: 0.5,
    }
}

/// Sets the modification time of every file under `root` to `t`: what "written long ago" means to `LocalStorage`.
fn age_tree(root: &Path, t: SystemTime) {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            age_tree(&entry.path(), t);
        } else if kind.is_file() {
            File::options()
                .write(true)
                .open(entry.path())
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
    }
}

/// Copies the tree at `from` to `to`, keeping every file's modification time (the objects' ages).
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let dest = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest);
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &dest).unwrap();
            let written = entry.metadata().unwrap().modified().unwrap();
            File::options()
                .write(true)
                .open(&dest)
                .unwrap()
                .set_modified(written)
                .unwrap();
        }
    }
}

/// Runs `f(i)` for every `i < n` on `threads` threads.
fn par_for(n: usize, threads: usize, f: impl Fn(usize) + Sync) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    f(i);
                }
            });
        }
    });
}

// ---------------------------------------------------------------------------------------------------------------
// The writer's pin set, reference-counted as `persia`'s `PinSet` is (two holders of one id each keep it pinned).
// ---------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct RefPins {
    pinned: Mutex<BTreeMap<GcObject, usize>>,
}

impl RefPins {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<GcObject, usize>> {
        self.pinned.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pin(self: &Arc<Self>, objects: impl IntoIterator<Item = GcObject>) -> PinGuard {
        let mut guard = PinGuard {
            pins: Arc::clone(self),
            held: Vec::new(),
        };
        for object in objects {
            guard.add(object);
        }
        guard
    }
}

/// Pins held until dropped, as `persia`'s `Pins`.
struct PinGuard {
    pins: Arc<RefPins>,
    held: Vec<GcObject>,
}

impl PinGuard {
    fn add(&mut self, object: GcObject) {
        *self.pins.lock().entry(object).or_insert(0) += 1;
        self.held.push(object);
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        let mut pinned = self.pins.lock();
        for object in &self.held {
            if let Some(count) = pinned.get_mut(object) {
                *count -= 1;
                if *count == 0 {
                    pinned.remove(object);
                }
            }
        }
    }
}

struct View<'a>(&'a BTreeMap<GcObject, usize>);

impl PinCheck for View<'_> {
    fn is_pinned(&self, object: GcObject) -> bool {
        self.0.contains_key(&object)
    }

    fn pinned_blobs(&self) -> Vec<BlobId> {
        self.0
            .keys()
            .filter_map(|o| match o {
                GcObject::Blob(id) => Some(*id),
                GcObject::Upload(_) => None,
            })
            .collect()
    }
}

impl PinGate for RefPins {
    fn pinned(&self) -> Vec<GcObject> {
        self.lock().keys().copied().collect()
    }

    fn unless_pinned<T>(
        &self,
        object: GcObject,
        delete: impl FnOnce(&dyn PinCheck) -> T,
    ) -> Option<T> {
        let pinned = self.lock();
        if pinned.contains_key(&object) {
            return None;
        }
        Some(delete(&View(&pinned)))
    }
}

/// GC's mark as `BlobDatabase::mark` takes it: the pins first, then the references, then the manifests' uploads.
fn mark_of<S: BlobStore + ?Sized>(
    store: &S,
    pins: &RefPins,
    referenced: impl IntoIterator<Item = BlobId>,
) -> Mark {
    let pinned = pins.pinned();
    let mut mark = Mark::new(referenced, pinned);
    mark.add_manifest_uploads(store).unwrap();
    mark
}

// ---------------------------------------------------------------------------------------------------------------
// The synthetic workload and its model.
// ---------------------------------------------------------------------------------------------------------------

/// When an object is written: before the tree is aged (expired at the sweep), or after (young).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Old,
    Young,
}

/// A blob that gets bound.
#[derive(Debug, Clone)]
struct BlobSpec {
    data: Vec<u8>,
    upload: u128,
    /// When its chunks are staged (unused for an inline blob).
    chunks: Phase,
    /// When it is bound: `Young` whenever `chunks` is (chunks first, SPEC §9.5).
    manifest: Phase,
    referenced: bool,
    pinned: bool,
}

/// An upload that is never bound.
#[derive(Debug, Clone)]
struct UploadSpec {
    /// The blob it was uploading: its chunks are this blob's.
    data: Vec<u8>,
    upload: u128,
    /// The chunks it staged, each with when.
    staged: Vec<(u32, Phase)>,
    /// A `put_blob` in flight: its upload and the blob id it hashed are pinned.
    in_flight: bool,
}

#[derive(Debug, Clone, Copy)]
struct Shape {
    blobs: usize,
    uploads: usize,
    chunk: u32,
    max_len: usize,
}

#[derive(Debug, Clone)]
struct Workload {
    chunk: u32,
    blobs: Vec<BlobSpec>,
    uploads: Vec<UploadSpec>,
    /// Keys under (or beside) `blobs/` that Persia did not write: never deleted.
    foreign: Vec<(String, Vec<u8>, Phase)>,
    /// A referenced blob that was never stored (a dangling reference): `Mark::missing`.
    dangling: BlobId,
}

impl Workload {
    fn small(&self, data: &[u8]) -> bool {
        data.len() as u64 <= u64::from(self.chunk).min(SMALL_BLOB_MAX)
    }

    fn chunk_count(&self, data: &[u8]) -> u32 {
        data.len().div_ceil(self.chunk as usize) as u32
    }

    fn referenced(&self) -> Vec<BlobId> {
        self.blobs
            .iter()
            .filter(|b| b.referenced)
            .map(|b| blob_id_of(&b.data))
            .chain([self.dangling])
            .collect()
    }

    fn pins(&self, pins: &Arc<RefPins>) -> PinGuard {
        let mut objects = Vec::new();
        for b in self.blobs.iter().filter(|b| b.pinned) {
            objects.push(GcObject::Blob(blob_id_of(&b.data)));
        }
        for u in self.uploads.iter().filter(|u| u.in_flight) {
            objects.push(GcObject::Upload(UploadId::new(u.upload)));
            objects.push(GcObject::Blob(blob_id_of(&u.data)));
        }
        pins.pin(objects)
    }

    /// Every blob's bytes, by id.
    fn data(&self) -> HashMap<BlobId, Vec<u8>> {
        self.blobs
            .iter()
            .map(|b| (blob_id_of(&b.data), b.data.clone()))
            .collect()
    }
}

/// Foreign keys: not of the forms SPEC §9.4 lists, so no sweep deletes them.
fn foreign_keys() -> Vec<String> {
    let id = "ab".to_owned() + &"0123456789abcdef".repeat(4)[2..];
    let upload = "0123456789abcdef".repeat(2);
    vec![
        "blobs/README".to_owned(),
        "blobs/index.json".to_owned(),
        format!("blobs/AB/{}", id.to_uppercase()),
        // A valid id under another prefix than its first byte.
        format!("blobs/cd/{id}"),
        format!("blobs/ab/{}", &id[..63]),
        format!("blobs/ab/{id}0"),
        format!("blobs/ab/{id}/extra"),
        format!("blobs/ab/{id}.bak"),
        format!("blobs/uploads/{}", "fedcba9876543210".repeat(2)),
        format!("blobs/uploads/{upload}/123"),
        format!("blobs/uploads/{upload}/4294967296"),
        format!("blobs/uploads/{upload}/0000000000.part"),
        format!("blobs/uploads/{upload}/0000000000/x"),
        format!("blobs/uploads/{}/0000000000", &upload[..31]),
        format!("blobs/uploads/{}/0000000000", upload.to_uppercase()),
        format!("blobs/uploads/{upload}0/0000000000"),
        format!("blob/{id}"),
        format!("other-app/blobs/ab/{id}"),
    ]
}

fn workload(seed: u64, shape: Shape) -> Workload {
    let mut rng = Rng(seed);
    let chunk = shape.chunk as usize;
    let small_max = chunk.min(SMALL_BLOB_MAX as usize);
    let mut ids = HashSet::new();
    let mut next_upload = u128::from(rng.next_u64()) << 32;
    let mut upload = || {
        next_upload += 1;
        next_upload
    };
    let phase = |rng: &mut Rng| {
        if rng.below(10) < 7 {
            Phase::Old
        } else {
            Phase::Young
        }
    };

    // Edge sizes first (empty, one byte, around the inline threshold and the chunk size), then random ones.
    let edges = [
        0,
        1,
        small_max - 1,
        small_max,
        small_max + 1,
        2 * chunk,
        2 * chunk + 1,
    ];
    let mut blobs = Vec::new();
    for i in 0..shape.blobs {
        let len = match edges.get(i) {
            Some(&len) => len,
            None if rng.below(3) == 0 => rng.range(1, small_max),
            None => rng.range(small_max + 1, shape.max_len),
        };
        let data = rng.bytes(len);
        if !ids.insert(blob_id_of(&data)) {
            continue;
        }
        // The first roles are forced, so every case is covered whatever the seed; then they are random.
        let role = if i < 8 { i } else { rng.range(0, 7) };
        let (chunks, manifest, referenced, pinned) = match role {
            // Referenced, long ago.
            0 => (Phase::Old, Phase::Old, true, false),
            // Orphans long ago: deleted.
            1 | 6 => (Phase::Old, Phase::Old, false, false),
            // An orphan bound recently, of chunks staged long ago: both kept.
            2 => (Phase::Old, Phase::Young, false, false),
            // A recent orphan: kept.
            3 => (Phase::Young, Phase::Young, false, false),
            // Pinned (a document not yet durable, or a read): kept.
            4 => (Phase::Old, Phase::Old, false, true),
            // Referenced, recent.
            5 => (Phase::Young, Phase::Young, true, false),
            _ => {
                let chunks = phase(&mut rng);
                let manifest = if chunks == Phase::Old {
                    phase(&mut rng)
                } else {
                    Phase::Young
                };
                (chunks, manifest, rng.below(3) == 0, rng.below(10) == 0)
            }
        };
        blobs.push(BlobSpec {
            data,
            upload: upload(),
            chunks,
            manifest,
            referenced,
            pinned,
        });
    }

    let chunked: Vec<usize> = (0..blobs.len())
        .filter(|&i| blobs[i].data.len() > small_max)
        .collect();
    let mut uploads = Vec::new();
    for i in 0..shape.uploads {
        let role = if i < 6 { i } else { rng.range(0, 5) };
        let data = if role == 3 {
            // A duplicate's upload: another writer's copy of a bound blob, left after an unknown bind outcome.
            blobs[chunked[rng.below(chunked.len() as u64) as usize]]
                .data
                .clone()
        } else {
            let len = rng.range(3 * chunk + 1, 4 * chunk);
            rng.bytes(len)
        };
        let n = data.len().div_ceil(chunk) as u32;
        let (staged, in_flight): (Vec<(u32, Phase)>, bool) = match role {
            // Abandoned mid-staging long ago: deleted.
            0 => ((0..n - 1).map(|i| (i, Phase::Old)).collect(), false),
            // Abandoned recently: kept.
            1 => ((0..2).map(|i| (i, Phase::Young)).collect(), false),
            // Staged long ago, its newest chunk recent: the upload is young (aged from its newest chunk).
            2 => (
                vec![(0, Phase::Old), (1, Phase::Old), (2, Phase::Young)],
                false,
            ),
            // The duplicate's full upload, long ago: deleted.
            3 => ((0..n).map(|i| (i, Phase::Old)).collect(), false),
            // A `put_blob` in flight: pinned.
            4 => ((0..2).map(|i| (i, Phase::Old)).collect(), true),
            // Abandoned long ago with a gap (a parallel stage that failed midway): deleted.
            _ => (vec![(0, Phase::Old), (2, Phase::Old)], false),
        };
        uploads.push(UploadSpec {
            data,
            upload: upload(),
            staged,
            in_flight,
        });
    }

    let foreign = foreign_keys()
        .into_iter()
        .enumerate()
        .map(|(i, k)| {
            let bytes = format!("not Persia's: {k}").into_bytes();
            (k, bytes, if i % 4 == 3 { Phase::Young } else { Phase::Old })
        })
        .collect();
    Workload {
        chunk: shape.chunk,
        blobs,
        uploads,
        foreign,
        dangling: blob_id_of(b"referenced, never stored"),
    }
}

/// Writes the objects of `phase` (the blobs' chunks before their manifests), on `threads` threads.
fn write_phase<S: Storage + Sync>(
    store: &RemoteBlobStore<S>,
    w: &Workload,
    phase: Phase,
    threads: usize,
) {
    par_for(w.blobs.len(), threads, |i| {
        let b = &w.blobs[i];
        let m = manifest_of(&b.data, w.chunk, b.upload);
        if w.small(&b.data) {
            if b.manifest == phase {
                assert_eq!(store.bind_small(&m, &b.data).unwrap(), BindOutcome::Bound);
            }
            return;
        }
        if b.chunks == phase {
            let chunks: Vec<&[u8]> = b.data.chunks(w.chunk as usize).collect();
            store.stage_chunks(m.upload_id(), 0, &chunks).unwrap();
        }
        if b.manifest == phase {
            assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        }
    });
    par_for(w.uploads.len(), threads, |i| {
        let u = &w.uploads[i];
        let chunks: Vec<&[u8]> = u.data.chunks(w.chunk as usize).collect();
        for &(index, p) in &u.staged {
            if p == phase {
                store
                    .stage_chunk(UploadId::new(u.upload), index, chunks[index as usize])
                    .unwrap();
            }
        }
    });
    for (k, bytes, p) in &w.foreign {
        if *p == phase {
            store.storage().put(&key(k), bytes.clone().into()).unwrap();
        }
    }
}

/// Writes `w` under `root`: the old objects, then they are aged, then the young ones. Returns every key written.
fn build(root: &Path, w: &Workload, threads: usize) -> BTreeSet<String> {
    let store = local(root);
    write_phase(&store, w, Phase::Old, threads);
    age_tree(root, SystemTime::now() - AGED);
    write_phase(&store, w, Phase::Young, threads);
    keys_of(store.storage()).into_iter().collect()
}

/// What one sweep at the system clock's `now` must do to `w` (SPEC §9.5): the keys it deletes, and its report.
fn expect(w: &Workload) -> (BTreeSet<String>, SweepReport) {
    let mut report = SweepReport::default();
    let mut deleted = BTreeSet::new();
    let mut marked: HashSet<BlobId> = w.referenced().into_iter().collect();
    let mut keep: HashSet<u128> = HashSet::new();
    for b in w.blobs.iter().filter(|b| b.pinned) {
        marked.insert(blob_id_of(&b.data));
    }
    for u in w.uploads.iter().filter(|u| u.in_flight) {
        marked.insert(blob_id_of(&u.data));
        keep.insert(u.upload);
    }
    // Manifests first.
    for b in &w.blobs {
        let id = blob_id_of(&b.data);
        let small = w.small(&b.data);
        if marked.contains(&id) {
            if !small {
                keep.insert(b.upload);
            }
            continue;
        }
        match b.manifest {
            Phase::Old => {
                deleted.insert(manifest_key(id));
                report.blobs_deleted += 1;
            }
            Phase::Young => {
                report.young += 1;
                if !small {
                    keep.insert(b.upload);
                }
            }
        }
    }
    // Then the staged chunks, grouped by upload and aged from its newest chunk.
    let mut staged: BTreeMap<u128, Vec<(u32, Phase)>> = BTreeMap::new();
    for b in w.blobs.iter().filter(|b| !w.small(&b.data)) {
        let n = w.chunk_count(&b.data);
        staged.insert(b.upload, (0..n).map(|i| (i, b.chunks)).collect());
    }
    for u in &w.uploads {
        staged.insert(u.upload, u.staged.clone());
    }
    for (upload, chunks) in staged {
        if keep.contains(&upload) {
            continue;
        }
        if chunks.iter().any(|&(_, p)| p == Phase::Young) {
            report.young += 1;
            continue;
        }
        report.uploads_deleted += 1;
        for (index, _) in chunks {
            deleted.insert(chunk_key(upload, index));
            report.chunks_deleted += 1;
        }
    }
    (deleted, report)
}

/// Checks that every manifest in `store` has all its chunks and reads back the bytes `data` holds for it (byte for
/// byte, whole and a sub-range), and that every foreign key holds its bytes.
fn check_consistent<S: Storage>(
    store: &RemoteBlobStore<S>,
    data: &HashMap<BlobId, Vec<u8>>,
    foreign: &[(String, Vec<u8>, Phase)],
    ctx: &str,
) {
    for k in keys_of(store.storage()) {
        let Some(RemoteKey::Manifest(id)) = RemoteKey::parse(&key(&k)) else {
            continue;
        };
        let m = store
            .manifest(id)
            .unwrap()
            .unwrap_or_else(|| panic!("{ctx}: {k} listed, not read"));
        if !m.is_inline() {
            for index in 0..m.chunk_count() {
                let chunk = chunk_key(m.upload_id().get(), index);
                assert!(
                    store.storage().get(&key(&chunk)).is_ok(),
                    "{ctx}: manifest {k} without its chunk {chunk}"
                );
            }
        }
        let expected = data
            .get(&id)
            .unwrap_or_else(|| panic!("{ctx}: a manifest nobody bound: {k}"));
        let r = blob_ref(&m);
        let got = read_range(store, &r, 0..m.size())
            .unwrap_or_else(|e| panic!("{ctx}: {k} unreadable: {e:?}"));
        assert!(got == *expected, "{ctx}: {k} reads other bytes");
        let (start, end) = (m.size() / 3, m.size() - m.size() / 5);
        assert!(
            read_range(store, &r, start..end).unwrap() == expected[start as usize..end as usize],
            "{ctx}: {k} range"
        );
    }
    for (k, bytes, _) in foreign {
        assert_eq!(
            &*store.storage().get(&key(k)).unwrap(),
            &bytes[..],
            "{ctx}: foreign key {k}"
        );
    }
}

fn present(store: &RemoteBlobStore<impl Storage>) -> BTreeSet<String> {
    keys_of(store.storage()).into_iter().collect()
}

// ---------------------------------------------------------------------------------------------------------------
// 1. A synthetic workload, swept once at the system clock.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_synthetic_workload_loses_exactly_its_expired_unmarked_objects() {
    let seed = base_seed();
    let shape = Shape {
        blobs: if cfg!(feature = "slow") { 120 } else { 40 },
        uploads: if cfg!(feature = "slow") { 30 } else { 12 },
        chunk: CHUNK,
        max_len: 5 * CHUNK as usize,
    };
    let w = workload(seed, shape);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("bucket");
    let t = Instant::now();
    let before = build(&root, &w, 4);
    let built = t.elapsed();

    let store = local(&root);
    let pins = Arc::new(RefPins::default());
    let held = w.pins(&pins);
    let (deleted, expected) = expect(&w);
    let t = Instant::now();
    let mark = mark_of(&store, &pins, w.referenced());
    assert_eq!(mark.missing, [w.dangling], "seed {seed}");
    assert!(mark.unreadable.is_empty(), "seed {seed}");
    let report = store
        .sweep(&mark, &*pins, &options(SystemTime::now()))
        .unwrap();
    let swept = t.elapsed();
    eprintln!(
        "{} objects ({} blobs, {} uploads), built in {built:?}; mark + sweep in {swept:?}: {report:?}",
        before.len(),
        w.blobs.len(),
        w.uploads.len()
    );
    assert_eq!(report, expected, "seed {seed}");
    // Every case of the model happened.
    assert!(
        expected.blobs_deleted > 0 && expected.uploads_deleted > 0 && expected.young > 0,
        "seed {seed}: {expected:?}"
    );

    let after = present(&store);
    let gone: BTreeSet<String> = before.difference(&after).cloned().collect();
    assert_eq!(gone, deleted, "seed {seed}: deleted keys");
    assert!(after.is_subset(&before), "seed {seed}");
    check_consistent(&store, &w.data(), &w.foreign, &format!("seed {seed}"));
    for b in w.blobs.iter().filter(|b| b.referenced || b.pinned) {
        let id = blob_id_of(&b.data);
        assert!(
            store.manifest(id).unwrap().is_some(),
            "seed {seed}: kept blob {id} gone"
        );
    }

    // A second sweep finds nothing more to delete.
    let mark = mark_of(&store, &pins, w.referenced());
    let again = store
        .sweep(&mark, &*pins, &options(SystemTime::now()))
        .unwrap();
    assert_eq!(
        (
            again.blobs_deleted,
            again.uploads_deleted,
            again.chunks_deleted
        ),
        (0, 0, 0),
        "seed {seed}: {again:?}"
    );
    assert_eq!(present(&store), after, "seed {seed}");

    // Every document deleted, every pin released, every object aged: only the foreign keys are left.
    drop(held);
    age_tree(&root, SystemTime::now() - AGED);
    let mark = mark_of(&store, &pins, []);
    let last = store
        .sweep(&mark, &*pins, &options(SystemTime::now()))
        .unwrap();
    let foreign: BTreeSet<String> = w.foreign.iter().map(|(k, _, _)| k.clone()).collect();
    assert_eq!(present(&store), foreign, "seed {seed}: {last:?}");
    check_consistent(&store, &w.data(), &w.foreign, &format!("seed {seed}"));
}

// ---------------------------------------------------------------------------------------------------------------
// 2. A crash loop: the n-th delete of a sweep fails, for every n.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_sweep_failing_at_any_delete_leaves_no_manifest_without_chunks_and_converges() {
    let seed = base_seed();
    let shape = Shape {
        blobs: if cfg!(feature = "slow") { 30 } else { 14 },
        uploads: if cfg!(feature = "slow") { 12 } else { 6 },
        chunk: CHUNK,
        max_len: 3 * CHUNK as usize,
    };
    let w = workload(seed ^ 0xc4a5, shape);
    let template = tempfile::tempdir().unwrap();
    let template_root = template.path().join("bucket");
    let before = build(&template_root, &w, 4);
    let (deleted, expected) = expect(&w);
    let total = expected.blobs_deleted + expected.chunks_deleted;
    let end: BTreeSet<String> = before.difference(&deleted).cloned().collect();
    let data = w.data();
    let kinds: &[Fault] = if cfg!(feature = "slow") {
        &[
            Fault::Crash,
            Fault::CrashAfter,
            Fault::Io,
            Fault::ResponseLost,
        ]
    } else {
        &[Fault::Crash, Fault::CrashAfter]
    };
    let started = Instant::now();
    let mut runs = 0;
    for &fault in kinds {
        for n in 1..=total + 1 {
            let ctx = format!("seed {seed}, {fault:?} at delete {n} of {total}");
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("bucket");
            copy_tree(&template_root, &root);
            let pins = Arc::new(RefPins::default());
            let held = w.pins(&pins);
            {
                let faulty = FaultyStorage::new(LocalStorage::open(&root).unwrap(), seed ^ n);
                faulty.inject(Trigger::Nth(Op::Delete, n), fault);
                let store = RemoteBlobStore::new(faulty);
                let mark = mark_of(&store, &pins, w.referenced());
                let result = store.sweep(&mark, &*pins, &options(SystemTime::now()));
                if n <= total {
                    assert!(
                        matches!(result, Err(Error::Storage(_))),
                        "{ctx}: {result:?}"
                    );
                    assert!(store.storage().pending().is_empty(), "{ctx}");
                } else {
                    assert_eq!(result.unwrap(), expected, "{ctx}");
                }
            }
            // The state the failure left, through a fresh process's store.
            let store = local(&root);
            let now = present(&store);
            assert!(now.is_subset(&before), "{ctx}");
            assert!(
                end.is_subset(&now),
                "{ctx}: deleted what it must keep: {:?}",
                end.difference(&now).collect::<Vec<_>>()
            );
            let applied = before.len() - now.len();
            let applied_expected = match fault {
                Fault::Crash | Fault::Io if n <= total => n - 1,
                _ => n.min(total),
            };
            assert_eq!(applied as u64, applied_expected, "{ctx}");
            check_consistent(&store, &data, &w.foreign, &ctx);

            // The restart: a second GC converges on the fault-free end state.
            let mark = mark_of(&store, &pins, w.referenced());
            store
                .sweep(&mark, &*pins, &options(SystemTime::now()))
                .unwrap_or_else(|e| panic!("{ctx}: second sweep: {e:?}"));
            assert_eq!(present(&store), end, "{ctx}: second sweep");
            check_consistent(&store, &data, &w.foreign, &ctx);
            drop(held);
            runs += 1;
        }
    }
    eprintln!(
        "{runs} crash runs over {} objects ({total} deletes each) in {:?}",
        before.len(),
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 3. Concurrency: put_blob and get_blob on other threads while sweeps run.
// ---------------------------------------------------------------------------------------------------------------

/// `put_blob`'s pin protocol (SPEC §9.5, ADR-0051): the upload pinned before its first chunk write, the blob id
/// before the bind; a duplicate also pins the upload the bound manifest names, and discards its own chunks.
fn put_blob<S: Storage>(
    store: &RemoteBlobStore<S>,
    pins: &Arc<RefPins>,
    data: &[u8],
    upload: u128,
) -> (BlobManifest, PinGuard) {
    let mut guard = pins.pin([GcObject::Upload(UploadId::new(upload))]);
    let m = manifest_of(data, CHUNK, upload);
    let id = m.blob_id();
    let outcome = if data.len() as u64 <= SMALL_BLOB_MAX {
        guard.add(GcObject::Blob(id));
        store.bind_small(&m, data).unwrap()
    } else {
        let chunks: Vec<&[u8]> = data.chunks(CHUNK as usize).collect();
        store.stage_chunks(m.upload_id(), 0, &chunks).unwrap();
        guard.add(GcObject::Blob(id));
        store.bind(&m).unwrap()
    };
    match outcome {
        BindOutcome::Bound => (store.manifest(id).unwrap().unwrap(), guard),
        BindOutcome::AlreadyBound(bound) => {
            assert_eq!(bound.blob_id(), id);
            if bound.upload_id() != m.upload_id() {
                guard.add(GcObject::Upload(bound.upload_id()));
                store.discard_upload(m.upload_id()).unwrap();
            }
            (bound, guard)
        }
    }
}

/// `get_blob`: the blob pinned before its manifest is looked up, the upload it names right after, then every byte.
/// `None` when it is not bound.
fn get_blob<S: Storage>(
    store: &RemoteBlobStore<S>,
    pins: &Arc<RefPins>,
    id: BlobId,
) -> Option<Vec<u8>> {
    let mut guard = pins.pin([GcObject::Blob(id)]);
    let m = store.manifest(id).unwrap()?;
    guard.add(GcObject::Upload(m.upload_id()));
    Some(read_manifest_range(store, &m, 0..m.size()).unwrap())
}

/// `get_blob_range`: only the blob pinned, for the call. `None` when it is not bound.
fn get_blob_range<S: Storage>(
    store: &RemoteBlobStore<S>,
    pins: &Arc<RefPins>,
    data: &[u8],
    range: core::ops::Range<u64>,
) -> Option<Vec<u8>> {
    let r = blob_ref(&manifest_of(data, CHUNK, 1));
    let _guard = pins.pin([GcObject::Blob(r.blob_id())]);
    match read_range(store, &r, range) {
        Ok(bytes) => Some(bytes),
        Err(Error::BlobNotFound { .. }) => None,
        Err(e) => panic!("get_blob_range: {e:?}"),
    }
}

/// How the concurrent test measures ages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clocking {
    /// At a `now` a month ahead: everything is expired, so only marks and pins protect anything; pins are released
    /// only between rounds (a document is logged only while no GC runs).
    Ahead,
    /// At the system clock, over orphans aged long ago: new writes are young; callers may also drop their pins
    /// (abandon a `put_blob`) while a sweep runs.
    System,
}

fn random_blob(rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(4) {
        0 => rng.range(0, 64),
        1 => rng.range(1, SMALL_BLOB_MAX as usize),
        _ => rng.range(SMALL_BLOB_MAX as usize + 1, 3 * CHUNK as usize + 7),
    };
    rng.bytes(len)
}

fn concurrent_puts_and_reads_during_sweeps(clocking: Clocking) {
    let seed = base_seed() ^ (clocking as u64 + 1);
    let mut rng = Rng(seed);
    let rounds = if cfg!(feature = "slow") { 10 } else { 3 };
    let ops = if cfg!(feature = "slow") { 30 } else { 10 };
    let (writers, readers) = (2, 2);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("bucket");
    let store = local(&root);
    let pins = Arc::new(RefPins::default());
    let next_upload = AtomicU64::new(1);
    let upload = || u128::from(next_upload.fetch_add(1, Ordering::Relaxed)) | (7 << 100);
    let all: Mutex<HashMap<BlobId, Vec<u8>>> = Mutex::new(HashMap::new());
    let foreign: Vec<(String, Vec<u8>, Phase)> = foreign_keys()
        .into_iter()
        .map(|k| (k.clone(), k.into_bytes(), Phase::Old))
        .collect();
    for (k, bytes, _) in &foreign {
        store.storage().put(&key(k), bytes.clone().into()).unwrap();
    }

    // The documents committed so far (their blobs referenced), and orphans: blobs no document references.
    let mut committed: BTreeMap<BlobId, Vec<u8>> = BTreeMap::new();
    let mut orphans: BTreeMap<BlobId, Vec<u8>> = BTreeMap::new();
    for i in 0..12 {
        let data = random_blob(&mut rng);
        let (m, _) = put_blob(&store, &pins, &data, upload());
        all.lock().unwrap().insert(m.blob_id(), data.clone());
        if i % 2 == 0 {
            committed.insert(m.blob_id(), data);
        } else {
            orphans.insert(m.blob_id(), data);
        }
    }
    // Uploads abandoned mid-staging.
    for _ in 0..3 {
        let data = rng.bytes(3 * CHUNK as usize);
        let chunks: Vec<&[u8]> = data.chunks(CHUNK as usize).take(2).collect();
        store
            .stage_chunks(UploadId::new(upload()), 0, &chunks)
            .unwrap();
    }
    age_tree(&root, SystemTime::now() - AGED);
    let now = || match clocking {
        Clocking::Ahead => SystemTime::now() + 30 * 24 * HOUR,
        Clocking::System => SystemTime::now(),
    };

    let started = Instant::now();
    let (mut sweeps, mut reads, mut not_found, mut deleted) = (0_u64, 0_u64, 0_u64, 0_u64);
    for round in 0..rounds {
        let referenced: HashSet<BlobId> = committed.keys().copied().collect();
        let committed_list: Vec<(BlobId, Vec<u8>)> =
            committed.iter().map(|(k, v)| (*k, v.clone())).collect();
        let orphan_list: Vec<(BlobId, Vec<u8>)> =
            orphans.iter().map(|(k, v)| (*k, v.clone())).collect();
        let writers_done = AtomicBool::new(false);
        let writing = AtomicUsize::new(writers);
        let barrier = Barrier::new(writers + readers + 1);
        let read_count = AtomicU64::new(0);
        let not_found_count = AtomicU64::new(0);
        let ctx = format!("seed {seed}, {clocking:?}, round {round}");

        let (puts, gc) = std::thread::scope(|s| {
            let writer_handles: Vec<_> = (0..writers)
                .map(|w| {
                    let mut rng = Rng(seed ^ (round << 8) ^ (w as u64 + 0x77));
                    let (store, pins, barrier, writing, writers_done, all, ctx) =
                        (&store, &pins, &barrier, &writing, &writers_done, &all, &ctx);
                    let (committed_list, orphan_list, upload) =
                        (&committed_list, &orphan_list, &upload);
                    s.spawn(move || {
                        barrier.wait();
                        let mut kept = Vec::new();
                        for _ in 0..ops {
                            let data = match rng.below(4) {
                                0 if !orphan_list.is_empty() => orphan_list
                                    [rng.below(orphan_list.len() as u64) as usize]
                                    .1
                                    .clone(),
                                1 if !committed_list.is_empty() => committed_list
                                    [rng.below(committed_list.len() as u64) as usize]
                                    .1
                                    .clone(),
                                _ => random_blob(&mut rng),
                            };
                            let (m, guard) = put_blob(store, pins, &data, upload());
                            all.lock().unwrap().insert(m.blob_id(), data.clone());
                            // Pinned: it reads back whole, whatever the sweeps do.
                            let got = read_range(store, &blob_ref(&m), 0..m.size())
                                .unwrap_or_else(|e| panic!("{ctx}: a pinned blob: {e:?}"));
                            assert!(got == data, "{ctx}: a pinned blob reads other bytes");
                            if clocking == Clocking::System && rng.below(4) == 0 {
                                // The caller gave up on the document: an orphan from now on.
                                drop(guard);
                                kept.push((m, data, None));
                            } else {
                                kept.push((m, data, Some(guard)));
                            }
                        }
                        if writing.fetch_sub(1, Ordering::SeqCst) == 1 {
                            writers_done.store(true, Ordering::SeqCst);
                        }
                        kept
                    })
                })
                .collect();

            for r in 0..readers {
                let mut rng = Rng(seed ^ (round << 8) ^ (r as u64 + 0x33));
                let (store, pins, barrier, writers_done, ctx) =
                    (&store, &pins, &barrier, &writers_done, &ctx);
                let (committed_list, orphan_list, read_count, not_found_count) =
                    (&committed_list, &orphan_list, &read_count, &not_found_count);
                s.spawn(move || {
                    barrier.wait();
                    let mut n = 0;
                    loop {
                        let finished = writers_done.load(Ordering::SeqCst);
                        // A referenced blob always reads back; an orphan reads back or is gone, never broken.
                        let (list, must) = if rng.below(2) == 0 && !committed_list.is_empty() {
                            (committed_list, true)
                        } else if !orphan_list.is_empty() {
                            (orphan_list, false)
                        } else {
                            (committed_list, true)
                        };
                        let (id, data) = &list[rng.below(list.len() as u64) as usize];
                        let (got, range) = if rng.below(2) == 0 {
                            (get_blob(store, pins, *id), 0..data.len())
                        } else {
                            let len = data.len() as u64;
                            let start = rng.below(len + 1);
                            let end = start + rng.below(len - start + 1);
                            let got = get_blob_range(store, pins, data, start..end);
                            (got, start as usize..end as usize)
                        };
                        match got {
                            Some(bytes) => {
                                assert!(bytes == data[range], "{ctx}: blob {id} reads other bytes");
                            }
                            None if must => panic!("{ctx}: referenced blob {id} gone"),
                            None => {
                                not_found_count.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        read_count.fetch_add(1, Ordering::Relaxed);
                        n += 1;
                        if finished && n >= 20 {
                            break;
                        }
                    }
                });
            }

            let gc = s.spawn(|| {
                barrier.wait();
                let mut swept = Vec::new();
                loop {
                    let finished = writers_done.load(Ordering::SeqCst);
                    let mark = mark_of(&store, &pins, referenced.iter().copied());
                    let report = store
                        .sweep(&mark, &*pins, &options(now()))
                        .unwrap_or_else(|e| panic!("{ctx}: sweep: {e:?}"));
                    swept.push(report);
                    if finished {
                        break;
                    }
                }
                swept
            });

            let puts: Vec<_> = writer_handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect();
            (puts, gc.join().unwrap())
        });
        sweeps += gc.len() as u64;
        deleted += gc.iter().map(|r| r.blobs_deleted).sum::<u64>();
        reads += read_count.load(Ordering::Relaxed);
        not_found += not_found_count.load(Ordering::Relaxed);

        // Still pinned after every sweep: each reads back whole.
        for (m, data, guard) in &puts {
            if guard.is_some() {
                let got = read_range(&store, &blob_ref(m), 0..m.size())
                    .unwrap_or_else(|e| panic!("{ctx}: a pinned blob after the round: {e:?}"));
                assert!(got == *data, "{ctx}");
            }
        }
        check_consistent(&store, &all.lock().unwrap(), &foreign, &ctx);
        for (id, data) in &committed {
            assert!(get_blob(&store, &pins, *id).as_ref() == Some(data), "{ctx}");
        }

        // Between rounds (no GC running): the documents are logged durably, their pins released; some documents
        // are deleted, their blobs orphans now.
        for (m, data, guard) in puts {
            if guard.is_some() {
                orphans.remove(&m.blob_id());
                committed.insert(m.blob_id(), data);
            } else if !committed.contains_key(&m.blob_id()) {
                orphans.insert(m.blob_id(), data);
            }
        }
        let ids: Vec<BlobId> = committed.keys().copied().collect();
        for id in ids {
            if rng.below(4) == 0 {
                let data = committed.remove(&id).unwrap();
                orphans.insert(id, data);
            }
        }
        // Orphans already swept are gone for good (unless a later put binds them again).
        orphans.retain(|id, _| store.manifest(*id).unwrap().is_some());
    }

    // At rest, every object expired: exactly the committed blobs and the foreign keys are left.
    assert!(pins.pinned().is_empty(), "seed {seed}: pins left");
    age_tree(&root, SystemTime::now() - AGED);
    let mark = mark_of(&store, &pins, committed.keys().copied());
    store.sweep(&mark, &*pins, &options(now())).unwrap();
    let mut expected: BTreeSet<String> = foreign.iter().map(|(k, _, _)| k.clone()).collect();
    for id in committed.keys() {
        let m = store.manifest(*id).unwrap().unwrap();
        expected.insert(manifest_key(*id));
        if !m.is_inline() {
            for index in 0..m.chunk_count() {
                expected.insert(chunk_key(m.upload_id().get(), index));
            }
        }
    }
    assert_eq!(present(&store), expected, "seed {seed}, {clocking:?}");
    check_consistent(
        &store,
        &all.lock().unwrap(),
        &foreign,
        &format!("seed {seed}"),
    );
    eprintln!(
        "{clocking:?}: {rounds} rounds, {sweeps} sweeps ({deleted} manifests deleted), {reads} reads \
         ({not_found} found an orphan gone), {} committed, in {:?}",
        committed.len(),
        started.elapsed()
    );
}

#[test]
fn put_and_get_race_sweeps_where_only_pins_protect_them() {
    concurrent_puts_and_reads_during_sweeps(Clocking::Ahead);
}

#[test]
fn put_and_get_race_sweeps_of_aged_orphans_at_the_system_clock() {
    concurrent_puts_and_reads_during_sweeps(Clocking::System);
}

// ---------------------------------------------------------------------------------------------------------------
// 3b. Deterministic interleavings: a whole sweep at every point of one put or read.
// ---------------------------------------------------------------------------------------------------------------

thread_local! {
    /// Set while a hook's sweep runs, so the sweep's own storage calls are not interleaving points.
    static IN_HOOK: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// A backend that runs `hook` at one point of its caller's operation: point `2k` is just before its k-th storage call
/// (from 0), point `2k + 1` just after it. Counting with `at = u64::MAX` tells how many points an operation has.
struct Interleave<S> {
    inner: S,
    at: u64,
    step: AtomicU64,
    hook: Box<dyn Fn() + Send + Sync>,
}

impl<S> Interleave<S> {
    fn point(&self) {
        if IN_HOOK.get() {
            return;
        }
        if self.step.fetch_add(1, Ordering::SeqCst) == self.at {
            IN_HOOK.set(true);
            (self.hook)();
            IN_HOOK.set(false);
        }
    }

    fn around<T>(&self, call: impl FnOnce() -> T) -> T {
        self.point();
        let result = call();
        self.point();
        result
    }
}

impl<S: Storage> Storage for Interleave<S> {
    fn get_range(
        &self,
        key: &Key,
        range: core::ops::Range<u64>,
    ) -> persia_storage::Result<persia_format::ByteSource> {
        self.around(|| self.inner.get_range(key, range))
    }
    fn get(&self, key: &Key) -> persia_storage::Result<bytes::Bytes> {
        self.around(|| self.inner.get(key))
    }
    fn put(&self, key: &Key, data: bytes::Bytes) -> persia_storage::Result<()> {
        self.around(|| self.inner.put(key, data))
    }
    fn put_if_absent(
        &self,
        key: &Key,
        data: bytes::Bytes,
    ) -> persia_storage::Result<persia_storage::PutOutcome> {
        self.around(|| self.inner.put_if_absent(key, data))
    }
    fn list(
        &self,
        prefix: &persia_storage::KeyPrefix,
    ) -> persia_storage::Result<Vec<persia_storage::ObjectMeta>> {
        self.around(|| self.inner.list(prefix))
    }
    fn delete(&self, key: &Key) -> persia_storage::Result<()> {
        self.around(|| self.inner.delete(key))
    }
    fn capabilities(&self) -> persia_storage::Capabilities {
        self.inner.capabilities()
    }
}

/// One operation of the writer, run with a sweep at one of its points.
#[derive(Debug, Clone, Copy)]
enum Step {
    GetBlob(usize),
    GetBlobRange(usize),
    PinContained(usize),
    PutBlob(usize),
}

#[test]
fn a_sweep_at_every_point_of_a_put_or_read_takes_nothing_it_uses() {
    let seed = base_seed();
    let mut rng = Rng(seed ^ 0x1e4f);
    // 0: a chunked orphan; 1: an inline orphan; 2: a referenced chunked blob; 3: fresh bytes, chunked; 4: fresh, inline.
    let corpus: Vec<Vec<u8>> = vec![
        rng.bytes(2 * CHUNK as usize + 17),
        rng.bytes(1000),
        rng.bytes(CHUNK as usize + 1),
        rng.bytes(3 * CHUNK as usize - 5),
        rng.bytes(40),
    ];
    let referenced: Arc<HashSet<BlobId>> = Arc::new([blob_id_of(&corpus[2])].into());
    let steps = [
        Step::GetBlob(0),
        Step::GetBlob(1),
        Step::GetBlob(2),
        Step::GetBlobRange(0),
        Step::GetBlobRange(1),
        Step::PinContained(0),
        // Duplicates of the orphans being swept, then of the referenced blob.
        Step::PutBlob(0),
        Step::PutBlob(1),
        Step::PutBlob(2),
        Step::PutBlob(3),
        Step::PutBlob(4),
    ];
    // A month ahead: every object is expired, so only the mark and the pins keep anything.
    let ahead = SystemTime::now() + 30 * 24 * HOUR;
    // The stored blobs, written once and copied for each run.
    let template = tempfile::tempdir().unwrap();
    let mut stored: HashMap<BlobId, Vec<u8>> = HashMap::new();
    {
        let store = local(template.path());
        let pins = Arc::new(RefPins::default());
        for (i, bytes) in corpus.iter().enumerate().take(3) {
            let (m, _) = put_blob(&store, &pins, bytes, 100 + i as u128);
            stored.insert(m.blob_id(), bytes.clone());
        }
    }
    let started = Instant::now();
    let mut runs = 0;
    for step in steps {
        let mut points = None;
        let mut at = 0;
        while points.is_none_or(|n| at < n) {
            let ctx = format!("seed {seed}, {step:?}, sweep at point {at}");
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("bucket");
            copy_tree(template.path(), &root);
            let base = local(&root);
            let pins = Arc::new(RefPins::default());
            let mut data = stored.clone();
            let swept = Arc::new(AtomicU64::new(0));
            let hook = {
                let (root, pins, referenced, swept) = (
                    root.clone(),
                    Arc::clone(&pins),
                    Arc::clone(&referenced),
                    Arc::clone(&swept),
                );
                move || {
                    let store = local(&root);
                    let mark = mark_of(&store, &pins, referenced.iter().copied());
                    store.sweep(&mark, &*pins, &options(ahead)).unwrap();
                    swept.fetch_add(1, Ordering::SeqCst);
                }
            };
            let store = RemoteBlobStore::new(Interleave {
                inner: LocalStorage::open(&root).unwrap(),
                at: if points.is_none() { u64::MAX } else { at },
                step: AtomicU64::new(0),
                hook: Box::new(hook),
            })
            .with_parallelism(core::num::NonZeroUsize::MIN);

            // The operation; what it returns stays pinned while it is checked.
            let (bytes, expected, guard): (Option<Vec<u8>>, &[u8], Option<PinGuard>) = match step {
                Step::GetBlob(i) => (
                    get_blob(&store, &pins, blob_id_of(&corpus[i])),
                    &corpus[i],
                    None,
                ),
                Step::GetBlobRange(i) => {
                    let n = corpus[i].len() as u64;
                    (
                        get_blob_range(&store, &pins, &corpus[i], 1..n - 1),
                        &corpus[i][1..corpus[i].len() - 1],
                        None,
                    )
                }
                Step::PinContained(i) => {
                    // `put(doc, BlobRef)`: the blob pinned, found bound, the upload it names pinned.
                    let id = blob_id_of(&corpus[i]);
                    let mut guard = pins.pin([GcObject::Blob(id)]);
                    let m = store.manifest(id).unwrap().expect(&ctx);
                    guard.add(GcObject::Upload(m.upload_id()));
                    (None, &corpus[i], Some(guard))
                }
                Step::PutBlob(i) => {
                    let (m, guard) = put_blob(&store, &pins, &corpus[i], 900);
                    data.insert(m.blob_id(), corpus[i].clone());
                    (None, &corpus[i], Some(guard))
                }
            };
            let total = store.storage().step.load(Ordering::SeqCst);
            if let Some(bytes) = bytes {
                assert!(bytes == expected, "{ctx}: other bytes");
            }
            if guard.is_some() {
                // Pinned: it survives one more sweep and reads back whole.
                let id = blob_id_of(expected);
                let r = blob_ref(&base.manifest(id).unwrap().expect(&ctx));
                let mark = mark_of(&base, &pins, referenced.iter().copied());
                base.sweep(&mark, &*pins, &options(ahead)).unwrap();
                let got = read_range(&base, &r, 0..r.size())
                    .unwrap_or_else(|e| panic!("{ctx}: a pinned blob: {e:?}"));
                assert!(got == expected, "{ctx}");
            }
            check_consistent(&base, &data, &[], &ctx);
            drop(guard);
            if points.is_none() {
                assert_eq!(swept.load(Ordering::SeqCst), 0, "{ctx}");
                points = Some(total);
            } else {
                assert_eq!(swept.load(Ordering::SeqCst), 1, "{ctx}: the hook ran");
                runs += 1;
                at += 1;
            }
        }
    }
    eprintln!(
        "{runs} interleavings ({} operations) in {:?}",
        steps.len(),
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 4. Scale smoke: about 10k objects.
// ---------------------------------------------------------------------------------------------------------------

#[cfg(feature = "slow")]
#[test]
fn a_sweep_of_ten_thousand_objects() {
    let seed = base_seed() ^ 0x10_000;
    // Small chunks (the encoding allows them, SPEC §9.1.1) so the objects are many and the bytes few.
    let shape = Shape {
        blobs: 3000,
        uploads: 600,
        chunk: 512,
        max_len: 2048,
    };
    let w = workload(seed, shape);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("bucket");
    let t = Instant::now();
    let before = build(&root, &w, 8);
    let built = t.elapsed();
    let store = local(&root);
    let pins = Arc::new(RefPins::default());
    let _held = w.pins(&pins);
    let (deleted, expected) = expect(&w);

    let t = Instant::now();
    let mark = mark_of(&store, &pins, w.referenced());
    let marked = t.elapsed();
    let t = Instant::now();
    let report = store
        .sweep(&mark, &*pins, &options(SystemTime::now()))
        .unwrap();
    let swept = t.elapsed();
    let t = Instant::now();
    let mark = mark_of(&store, &pins, w.referenced());
    let again = store
        .sweep(&mark, &*pins, &options(SystemTime::now()))
        .unwrap();
    let idle = t.elapsed();
    eprintln!(
        "{} objects built in {built:?}; mark {marked:?}, sweep {swept:?} ({} deletes: {report:?}); \
         a second mark + sweep (nothing to delete) {idle:?}",
        before.len(),
        report.blobs_deleted + report.chunks_deleted
    );
    assert!(before.len() >= 9000, "{} objects", before.len());
    assert_eq!(report, expected, "seed {seed}");
    assert_eq!(
        (again.blobs_deleted, again.chunks_deleted),
        (0, 0),
        "seed {seed}"
    );
    let after = present(&store);
    let gone: BTreeSet<String> = before.difference(&after).cloned().collect();
    assert_eq!(gone, deleted, "seed {seed}");
    check_consistent(&store, &w.data(), &w.foreign, &format!("seed {seed}"));
}
