//! The remote sweep (SPEC §9.4, §9.5; ROADMAP 9.5.2, 9.5.4; ADR-0052), against a model.
//!
//! Invariants:
//! 1. Nothing marked or pinned is deleted: a marked or pinned blob's manifest stays, and so do the chunks of every
//!    upload that is marked, pinned, or named by a manifest that stays.
//! 2. Everything else older than the grace period is deleted: unmarked manifests, then the chunks of uploads no
//!    remaining manifest names, aged from the upload's newest chunk.
//! 3. An object whose age is unknown (`last_modified` is `None`) is never deleted, and is counted.
//! 4. A key under `blobs/` that Persia did not write is never deleted.
//! 5. A crash at any step of a sweep (`LocalStorage` on `SimFs`) leaves no manifest without its chunks, and every
//!    marked blob readable.
//! 6. An unreadable manifest, marked or kept, stops the staged-chunk phase.
//! 7. A sweep of a generated store stopped at any delete (failed, or applied with the process gone) leaves no manifest
//!    without its chunks, and the next sweep ends where an uninterrupted one does.
//! 8. One object of unknown age keeps its upload (a chunk) or its manifest and the chunks it names.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::struct_excessive_bools,
    clippy::assert_is_empty,
    missing_docs
)] // Tests: inputs are generated or fixed.

mod gc_support;

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use gc_support::{Pins, data_of, manifest_of, put, read_all, stage};
use persia_blob::gc::{GcObject, Mark, Sweep, SweepOptions, SweepReport};
use persia_blob::remote::{RemoteBlobStore, RemoteKey};
use persia_blob::{BindOutcome, BlobId, BlobManifest, BlobStore, UploadId};
use persia_format::ByteSource;
use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::vfs::sim::SimFs;
use persia_storage::{
    Capabilities, Key, KeyPrefix, LocalStorage, MemoryStorage, ObjectMeta, PutOutcome, Storage,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

const HOUR: Duration = Duration::from_secs(3600);

fn at(hours: u64) -> SystemTime {
    UNIX_EPOCH + HOUR * u32::try_from(hours).unwrap()
}

fn options(now: SystemTime) -> SweepOptions {
    SweepOptions {
        grace: 24 * HOUR,
        now,
        rewrite_ratio: 0.5,
    }
}

/// One object of the generated store.
#[derive(Debug, Clone)]
struct Spec {
    /// Bytes: 1..=12 (inline when bound small), staged in chunks of `chunk`.
    len: usize,
    chunk: u32,
    /// Hours after the epoch at which its chunks, then (an hour later) its manifest, are written.
    hour: u64,
    /// Whether it is bound (else only staged), and if so inline.
    bound: bool,
    inline: bool,
    marked: bool,
    pin_blob: bool,
    pin_upload: bool,
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        1..=12_usize,
        2..=5_u32,
        0..60_u64,
        prop::bool::weighted(0.7),
        prop::bool::weighted(0.2),
        prop::bool::weighted(0.3),
        prop::bool::weighted(0.15),
        prop::bool::weighted(0.15),
    )
        .prop_map(
            |(len, chunk, hour, bound, inline, marked, pin_blob, pin_upload)| Spec {
                len,
                chunk,
                hour,
                bound,
                inline,
                marked,
                pin_blob,
                pin_upload,
            },
        )
}

/// What the generated store holds, as the test wrote it.
struct Built {
    /// Per spec: its manifest (bound or not) and data.
    blobs: Vec<(Spec, BlobManifest, Vec<u8>)>,
    pins: Pins,
    marked: BTreeSet<BlobId>,
}

fn build<S: Storage>(store: &RemoteBlobStore<S>, clock: &ManualClock, specs: &[Spec]) -> Built {
    let mut order: Vec<usize> = (0..specs.len()).collect();
    order.sort_by_key(|&i| specs[i].hour);
    let mut blobs = Vec::new();
    let pins = Pins::default();
    let mut marked = BTreeSet::new();
    let mut ids = HashSet::new();
    for i in order {
        let s = &specs[i];
        let data = data_of(s.len, i as u8);
        // Distinct bytes per spec: the seed alone may collide for one-byte blobs.
        let mut data = data;
        data.push(i as u8);
        let upload = 1000 + i as u128;
        clock.set(at(s.hour));
        let m = if s.bound && s.inline {
            let m = manifest_of(&data, 64 << 10, upload);
            assert!(matches!(
                store.bind_small(&m, &data).unwrap(),
                BindOutcome::Bound
            ));
            m
        } else {
            let m = stage(store, &data, s.chunk, upload);
            if s.bound {
                clock.set(at(s.hour + 1));
                assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
            }
            m
        };
        assert!(ids.insert(m.blob_id()));
        if s.marked {
            marked.insert(m.blob_id());
        }
        if s.pin_blob {
            pins.pin(GcObject::Blob(m.blob_id()));
        }
        if s.pin_upload {
            pins.pin(GcObject::Upload(m.upload_id()));
        }
        blobs.push((s.clone(), m, data));
    }
    Built {
        blobs,
        pins,
        marked,
    }
}

fn mark_of<S: BlobStore>(store: &S, built: &Built) -> Mark {
    let mut mark = Mark::new(built.marked.iter().copied(), built.pins_now());
    mark.add_manifest_uploads(store).unwrap();
    mark
}

impl Built {
    fn pins_now(&self) -> Vec<GcObject> {
        persia_blob::PinGate::pinned(&self.pins)
    }
}

fn exists<S: Storage>(store: &RemoteBlobStore<S>, key: &Key) -> bool {
    store.storage().get(key).is_ok()
}

fn chunk_keys(m: &BlobManifest) -> Vec<Key> {
    if m.is_inline() {
        return Vec::new();
    }
    (0..m.chunk_count())
        .map(|index| {
            RemoteKey::Chunk {
                upload_id: m.upload_id(),
                index,
            }
            .key()
            .unwrap()
        })
        .collect()
}

/// Checks the store after a sweep at `now` against the model (invariants 1–3); `known` says whether ages were known.
fn check<S: Storage>(
    store: &RemoteBlobStore<S>,
    built: &Built,
    now: SystemTime,
    known: bool,
    report: &SweepReport,
) {
    let expired = |hour: u64| known && now.duration_since(at(hour)).is_ok_and(|a| a > 24 * HOUR);
    let pinned = |o: GcObject| built.pins_now().contains(&o);
    let mut kept_uploads = HashSet::new();
    let mut deleted_blobs = 0;
    for (s, m, _) in &built.blobs {
        let key = RemoteKey::Manifest(m.blob_id()).key().unwrap();
        if !s.bound {
            continue;
        }
        let keep = built.marked.contains(&m.blob_id())
            || pinned(GcObject::Blob(m.blob_id()))
            || !expired(if s.inline { s.hour } else { s.hour + 1 });
        assert_eq!(exists(store, &key), keep, "manifest of {s:?}");
        if keep && !m.is_inline() {
            kept_uploads.insert(m.upload_id());
        }
        if !keep {
            deleted_blobs += 1;
        }
    }
    let mut deleted_uploads = 0;
    for (s, m, data) in &built.blobs {
        if s.bound && s.inline {
            continue;
        }
        let keep = kept_uploads.contains(&m.upload_id())
            || pinned(GcObject::Upload(m.upload_id()))
            || !expired(s.hour);
        for key in chunk_keys(m) {
            assert_eq!(exists(store, &key), keep, "chunks of {s:?}");
        }
        if keep && s.bound && exists(store, &RemoteKey::Manifest(m.blob_id()).key().unwrap()) {
            assert_eq!(&read_all(store, m).unwrap(), data, "{s:?}");
        }
        if !keep {
            deleted_uploads += 1;
        }
    }
    assert_eq!(report.blobs_deleted, deleted_blobs, "{report:?}");
    assert_eq!(report.uploads_deleted, deleted_uploads, "{report:?}");
    if !known {
        assert_eq!(report.blobs_deleted + report.uploads_deleted, 0);
    }
}

/// Keys under `blobs/` that Persia did not write: a sweep never deletes them (invariant 4).
fn foreign() -> Vec<Key> {
    let id = "ab".repeat(32);
    [
        format!("blobs/ab/{}", id.to_uppercase()),
        format!("blobs/cd/{id}"),
        format!("blobs/ab/{id}/deeper"),
        "blobs/uploads/notanupload/0000000001".to_owned(),
        format!("blobs/uploads/{}/12", "0".repeat(32)),
        "blobs/zz".to_owned(),
    ]
    .into_iter()
    .map(|k| Key::new(&k).unwrap())
    .collect()
}

fn run(specs: &[Spec], now_hours: u64, known: bool) {
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
    let memory = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    let store = RemoteBlobStore::new(NoTimesIf(known.then_some(()).is_none(), memory));
    let built = build(&store, &clock, specs);
    for key in foreign() {
        store.storage().put(&key, Bytes::from_static(b"x")).unwrap();
    }
    let mark = mark_of(&store, &built);
    let now = at(now_hours);
    let report = store.sweep(&mark, &built.pins, &options(now)).unwrap();
    check(&store, &built, now, known, &report);
    for key in foreign() {
        assert!(exists(&store, &key), "{key} was deleted");
    }
    // A second sweep finds nothing more.
    let again = store.sweep(&mark, &built.pins, &options(now)).unwrap();
    assert_eq!(again.blobs_deleted + again.uploads_deleted, 0, "{again:?}");
}

/// [`NoTimes`] when the flag is set, the storage itself otherwise.
struct NoTimesIf<S>(bool, S);

impl<S: Storage> Storage for NoTimesIf<S> {
    fn get_range(
        &self,
        key: &Key,
        range: core::ops::Range<u64>,
    ) -> persia_storage::Result<ByteSource> {
        self.1.get_range(key, range)
    }
    fn get(&self, key: &Key) -> persia_storage::Result<Bytes> {
        self.1.get(key)
    }
    fn put(&self, key: &Key, data: Bytes) -> persia_storage::Result<()> {
        self.1.put(key, data)
    }
    fn put_if_absent(&self, key: &Key, data: Bytes) -> persia_storage::Result<PutOutcome> {
        self.1.put_if_absent(key, data)
    }
    fn list(&self, prefix: &KeyPrefix) -> persia_storage::Result<Vec<ObjectMeta>> {
        let mut listed = self.1.list(prefix)?;
        if self.0 {
            for meta in &mut listed {
                meta.last_modified = None;
            }
        }
        Ok(listed)
    }
    fn delete(&self, key: &Key) -> persia_storage::Result<()> {
        self.1.delete(key)
    }
    fn capabilities(&self) -> Capabilities {
        self.1.capabilities()
    }
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 1024 } else { 128 },
        rng_seed: RngSeed::Fixed(0x9_5_2),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn the_sweep_keeps_exactly_what_the_mark_pins_and_grace_keep(
        specs in prop::collection::vec(spec(), 1..10),
        now in 0..100_u64,
    ) {
        run(&specs, now, true);
    }

    #[test]
    fn objects_of_unknown_age_are_never_deleted(
        specs in prop::collection::vec(spec(), 1..10),
        now in 0..100_u64,
    ) {
        run(&specs, now, false);
    }
}

#[test]
fn the_grace_period_is_strict_and_counts_from_the_newest_chunk() {
    let clock = Arc::new(ManualClock::new(at(0)));
    let store = RemoteBlobStore::new(MemoryStorage::with_clock(
        Arc::clone(&clock) as Arc<dyn Clock>
    ));
    let upload = UploadId::new(7);
    store.stage_chunk(upload, 0, b"old").unwrap();
    clock.set(at(10));
    store.stage_chunk(upload, 1, b"new").unwrap();
    let pins = Pins::default();
    let mark = Mark::default();
    // At exactly 24 h past the newest chunk the upload is not older than the grace period yet.
    let report = store.sweep(&mark, &pins, &options(at(34))).unwrap();
    assert_eq!((report.uploads_deleted, report.young), (0, 1));
    let report = store
        .sweep(&mark, &pins, &options(at(34) + Duration::from_secs(1)))
        .unwrap();
    assert_eq!((report.uploads_deleted, report.chunks_deleted), (1, 2));
    assert!(store.staged_chunks(upload).unwrap().is_empty());
    // A clock behind the objects' (written "in the future") keeps them.
    store.stage_chunk(UploadId::new(8), 0, b"x").unwrap();
    let report = store.sweep(&mark, &pins, &options(at(0))).unwrap();
    assert_eq!((report.uploads_deleted, report.young), (0, 1));
}

#[test]
fn an_unreadable_manifest_leaves_every_staged_chunk() {
    for marked in [true, false] {
        let clock = Arc::new(ManualClock::new(at(0)));
        let store = RemoteBlobStore::new(MemoryStorage::with_clock(
            Arc::clone(&clock) as Arc<dyn Clock>
        ));
        let damaged = put(&store, b"damaged blob", 4, 1);
        stage(&store, b"orphan chunks", 4, 2);
        clock.set(at(1));
        let key = RemoteKey::Manifest(damaged.blob_id()).key().unwrap();
        store
            .storage()
            .put(&key, Bytes::from_static(b"garbage"))
            .unwrap();
        let mut mark = Mark::new(marked.then_some(damaged.blob_id()), []);
        mark.add_manifest_uploads(&store).unwrap();
        // Young, so kept and read when unmarked; either way its upload is unknown.
        let report = store
            .sweep(
                &mark,
                &Pins::default(),
                &options(at(1) + 24 * HOUR - HOUR / 2),
            )
            .unwrap();
        assert!(report.uploads_skipped, "marked {marked}: {report:?}");
        assert_eq!(store.staged_chunks(UploadId::new(2)).unwrap().len(), 4);
        assert_eq!(store.staged_chunks(UploadId::new(1)).unwrap().len(), 3);
    }
}

#[test]
fn a_kept_manifest_keeps_its_uploads_chunks_whatever_their_age() {
    let clock = Arc::new(ManualClock::new(at(0)));
    let store = RemoteBlobStore::new(MemoryStorage::with_clock(
        Arc::clone(&clock) as Arc<dyn Clock>
    ));
    // Chunks staged at hour 0, bound at hour 30 (a slow bind, or a resumed upload): at hour 40 the manifest is young
    // and its chunks are old. And a pin taken after the mark, at the sweep's check, keeps an old manifest.
    let young = stage(&store, b"bound late", 4, 1);
    let pinned = put(&store, b"pinned at the check", 4, 2);
    clock.set(at(30));
    assert_eq!(store.bind(&young).unwrap(), BindOutcome::Bound);
    let pins = Pins::default();
    let id = pinned.blob_id();
    pins.on_check(move |p| {
        p.insert(GcObject::Blob(id));
    });
    let mut mark = Mark::default();
    mark.add_manifest_uploads(&store).unwrap();
    let report = store.sweep(&mark, &pins, &options(at(40))).unwrap();
    assert_eq!(
        (report.young, report.pinned, report.blobs_deleted),
        (1, 1, 0),
        "{report:?}"
    );
    assert_eq!(report.uploads_deleted, 0, "{report:?}");
    assert_eq!(read_all(&store, &young).unwrap(), b"bound late");
    assert_eq!(read_all(&store, &pinned).unwrap(), b"pinned at the check");
}

/// ROADMAP 9.5.8 (ADR-0051 item 5): a blob is pinned before the manifest naming an upload is read, and that upload
/// only after; between the two, the sweep must find the pinned blob's manifest under the pin set's lock and keep the
/// upload's chunks. Here the blob is bound and pinned while the sweep runs, after its listing: an upload whose chunks
/// are old (a resumed one), bound late, with its upload pin not taken yet.
#[test]
fn an_uploads_chunks_stay_while_a_pinned_blobs_manifest_names_it() {
    // What the hook does at the upload's check: nothing, bind and pin the blob, pin a blob whose manifest is
    // unreadable, or pin a blob that has no manifest yet.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Late {
        Nothing,
        BindAndPin,
        PinUnreadable,
        PinUnbound,
    }
    for late in [
        Late::Nothing,
        Late::BindAndPin,
        Late::PinUnreadable,
        Late::PinUnbound,
    ] {
        let clock = Arc::new(ManualClock::new(at(0)));
        let store = Arc::new(RemoteBlobStore::new(MemoryStorage::with_clock(
            Arc::clone(&clock) as Arc<dyn Clock>,
        )));
        let resumed = stage(&*store, b"staged long ago", 4, 1);
        clock.set(at(100));
        let pins = Pins::default();
        let hooked = Arc::clone(&store);
        let m = resumed.clone();
        let mut done = false;
        pins.on_check(move |pinned| {
            if std::mem::replace(&mut done, true) {
                return;
            }
            match late {
                Late::Nothing => {}
                Late::BindAndPin => {
                    assert_eq!(hooked.bind(&m).unwrap(), BindOutcome::Bound);
                    pinned.insert(GcObject::Blob(m.blob_id()));
                }
                Late::PinUnreadable => {
                    let other = persia_blob::blob_id_of(b"unreadable");
                    let key = RemoteKey::Manifest(other).key().unwrap();
                    hooked
                        .storage()
                        .put(&key, Bytes::from_static(b"garbage"))
                        .unwrap();
                    pinned.insert(GcObject::Blob(other));
                }
                Late::PinUnbound => {
                    pinned.insert(GcObject::Blob(persia_blob::blob_id_of(b"not bound yet")));
                }
            }
        });
        let mut mark = Mark::default();
        mark.add_manifest_uploads(&*store).unwrap();
        let report = store.sweep(&mark, &pins, &options(at(100))).unwrap();
        let kept = matches!(late, Late::BindAndPin | Late::PinUnreadable);
        assert_eq!(
            report.uploads_deleted,
            u64::from(!kept),
            "{late:?}: {report:?}"
        );
        assert_eq!(report.pinned, u64::from(kept), "{late:?}: {report:?}");
        assert_eq!(
            store.staged_chunks(resumed.upload_id()).unwrap().len(),
            if kept { 4 } else { 0 },
            "{late:?}"
        );
        if late == Late::BindAndPin {
            assert_eq!(read_all(&*store, &resumed).unwrap(), b"staged long ago");
        }
    }
}

#[test]
fn manifests_are_listed_per_prefix_and_uploads_once() {
    let clock = Arc::new(ManualClock::new(at(0)));
    let counting = Counting::new(MemoryStorage::with_clock(
        Arc::clone(&clock) as Arc<dyn Clock>
    ));
    let store = RemoteBlobStore::new(counting);
    put(&store, b"one blob", 3, 1);
    store
        .sweep(&Mark::default(), &Pins::default(), &options(at(100)))
        .unwrap();
    let lists = store.storage().lists.lock().unwrap().clone();
    assert_eq!(lists.len(), 257, "{lists:?}");
    let expected: Vec<String> = (0..=255_u8)
        .map(|b| format!("blobs/{b:02x}/"))
        .chain(["blobs/uploads/".to_owned()])
        .collect();
    assert_eq!(lists, expected);
}

/// Records the prefixes listed.
struct Counting<S> {
    inner: S,
    lists: std::sync::Mutex<Vec<String>>,
}

impl<S> Counting<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            lists: std::sync::Mutex::default(),
        }
    }
}

impl<S: Storage> Storage for Counting<S> {
    fn get_range(
        &self,
        key: &Key,
        range: core::ops::Range<u64>,
    ) -> persia_storage::Result<ByteSource> {
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
        self.lists.lock().unwrap().push(prefix.as_str().to_owned());
        self.inner.list(prefix)
    }
    fn delete(&self, key: &Key) -> persia_storage::Result<()> {
        self.inner.delete(key)
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

// --- Crash tests: `LocalStorage` on a `SimFs` with a clock (ROADMAP 9.5.7, 9.5.4) ---

const ROOT: &str = "/store";

fn local(fs: &SimFs) -> RemoteBlobStore<LocalStorage<SimFs>> {
    RemoteBlobStore::new(LocalStorage::open_in(fs.clone(), ROOT).unwrap())
}

const KEPT: &[u8] = b"a marked blob";
const GONE: [(&[u8], u32, u128); 2] = [(b"unmarked blob one", 4, 2), (b"unmarked two", 5, 3)];

/// A marked blob, two unmarked ones and an unbound upload, all written at hour 0 on a fresh file system.
fn crash_fixture(clock: &Arc<ManualClock>) -> (SimFs, BlobManifest, Vec<BlobManifest>) {
    clock.set(at(0));
    let fs = SimFs::with_clock(Arc::clone(clock) as Arc<dyn Clock>);
    let store = local(&fs);
    let kept = put(&store, KEPT, 4, 1);
    let mut gone: Vec<_> = GONE
        .iter()
        .map(|&(data, chunk, upload)| put(&store, data, chunk, upload))
        .collect();
    gone.push(stage(&store, b"staged only", 3, 4));
    (fs, kept, gone)
}

fn marked(store: &RemoteBlobStore<LocalStorage<SimFs>>, kept: &BlobManifest) -> Mark {
    let mut mark = Mark::new([kept.blob_id()], []);
    mark.add_manifest_uploads(store).unwrap();
    mark
}

/// After a crash: the marked blob reads back, every manifest left has all its chunks, and a sweep completes.
fn check_crashed(fs: &SimFs, kept: &BlobManifest, gone: &[BlobManifest], what: &str) {
    let store = local(fs);
    assert_eq!(read_all(&store, kept).unwrap(), KEPT, "{what}");
    for m in gone {
        if store.manifest(m.blob_id()).unwrap().is_some() {
            read_all(&store, m)
                .unwrap_or_else(|e| panic!("{what}: a manifest without its chunks: {e}"));
        }
    }
    let mark = marked(&store, kept);
    store
        .sweep(&mark, &Pins::default(), &options(at(100)))
        .unwrap_or_else(|e| panic!("{what}: sweep after the crash: {e}"));
    for m in gone {
        assert!(store.manifest(m.blob_id()).unwrap().is_none(), "{what}");
        assert!(
            store.staged_chunks(m.upload_id()).unwrap().is_empty(),
            "{what}"
        );
    }
    assert_eq!(read_all(&store, kept).unwrap(), KEPT, "{what}");
}

fn crash_during_sweep(seeds: u64) {
    let clock = Arc::new(ManualClock::new(at(0)));
    let total = {
        let (fs, kept, gone) = crash_fixture(&clock);
        let store = local(&fs);
        let mark = marked(&store, &kept);
        let start = fs.mutations();
        let report = store
            .sweep(&mark, &Pins::default(), &options(at(100)))
            .unwrap();
        assert_eq!((report.blobs_deleted, report.uploads_deleted), (2, 3));
        let total = fs.mutations() - start;
        check_crashed(&fs, &kept, &gone, "no crash");
        total
    };
    assert!(total >= 10, "the sweep makes {total} mutating calls");
    for k in 0..total {
        for fault in [SimFault::FailAt, SimFault::ApplyThenFail] {
            let (fs, kept, gone) = crash_fixture(&clock);
            let store = local(&fs);
            let mark = marked(&store, &kept);
            fs.set_fault(Some(fault(fs.mutations() + k)));
            assert!(
                store
                    .sweep(&mark, &Pins::default(), &options(at(100)))
                    .is_err(),
                "the fault at call {k} fires"
            );
            let losses = [CrashLoss::All, CrashLoss::Nothing]
                .into_iter()
                .chain((0..seeds).map(CrashLoss::Seeded));
            for loss in losses {
                let what = format!("{:?} at call {k}, {loss:?}", fault(k));
                check_crashed(&fs.crash(loss), &kept, &gone, &what);
            }
        }
    }
}

#[test]
fn a_crash_anywhere_in_a_sweep_leaves_no_manifest_without_its_chunks() {
    crash_during_sweep(4);
}

#[cfg(feature = "slow")]
#[test]
fn a_crash_anywhere_in_a_sweep_leaves_no_manifest_without_its_chunks_many_seeds() {
    crash_during_sweep(64);
}

// --- A sweep stopped at any delete, on generated stores (ROADMAP 9.5.4) ---

/// Every key under `blobs/` (ignoring times).
fn all_keys<S: Storage>(store: &RemoteBlobStore<S>) -> BTreeSet<String> {
    store
        .storage()
        .list(&KeyPrefix::new("blobs/").unwrap())
        .unwrap()
        .into_iter()
        .map(|m| m.key.as_str().to_owned())
        .collect()
}

/// A generated store on a `FaultyStorage`, with its mark.
fn faulty_store(
    specs: &[Spec],
) -> (
    RemoteBlobStore<persia_storage::faulty::FaultyStorage<MemoryStorage>>,
    Built,
    Mark,
) {
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
    let memory = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    let store = RemoteBlobStore::new(persia_storage::faulty::FaultyStorage::new(memory, 7));
    let built = build(&store, &clock, specs);
    let mark = mark_of(&store, &built);
    (store, built, mark)
}

/// SPEC §9.5: manifests go before their chunks. Whichever delete the sweep stops at (failed, or applied with the
/// process gone), every manifest left has all its chunks, every marked or pinned blob reads back, and a sweep after a
/// restart ends where an uninterrupted one does.
fn stopped_sweeps(specs: &[Spec], now: SystemTime) {
    use persia_storage::Op;
    use persia_storage::faulty::{Fault, Trigger};

    let (store, built, mark) = faulty_store(specs);
    let report = store.sweep(&mark, &built.pins, &options(now)).unwrap();
    let deletes = report.blobs_deleted + report.chunks_deleted;
    let expected = all_keys(&store);
    for n in 1..=deletes {
        for fault in [Fault::Io, Fault::CrashAfter] {
            let (store, built, mark) = faulty_store(specs);
            store.storage().inject(Trigger::Nth(Op::Delete, n), fault);
            let stopped = store.sweep(&mark, &built.pins, &options(now));
            assert!(
                stopped.is_err(),
                "delete {n} of {deletes} ({fault:?}) stops it"
            );
            store.storage().restart();
            let what = format!("stopped at delete {n} of {deletes} ({fault:?})");
            for (s, m, data) in &built.blobs {
                if !s.bound {
                    continue;
                }
                let keep = built.marked.contains(&m.blob_id())
                    || built.pins_now().contains(&GcObject::Blob(m.blob_id()));
                match store.manifest(m.blob_id()).unwrap() {
                    Some(_) => assert_eq!(
                        &read_all(&store, m).unwrap_or_else(|e| panic!(
                            "{what}: a manifest without its chunks: {e}"
                        )),
                        data,
                        "{what}"
                    ),
                    None => assert!(!keep, "{what}: a kept blob was deleted: {s:?}"),
                }
            }
            store.sweep(&mark, &built.pins, &options(now)).unwrap();
            assert_eq!(all_keys(&store), expected, "{what}: the sweep after it");
        }
    }
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 256 } else { 24 },
        rng_seed: RngSeed::Fixed(0x9_5_4_3),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn a_sweep_stopped_at_any_delete_leaves_no_manifest_without_its_chunks(
        specs in prop::collection::vec(spec(), 1..8),
        now in 30..100_u64,
    ) {
        stopped_sweeps(&specs, at(now));
    }
}

// --- Unknown ages, object by object (SPEC §9.5, §10.1) ---

/// Lists the keys of `unknown` without a time (`last_modified: None`), the others as the storage does.
struct NoTimesFor<S> {
    inner: S,
    unknown: BTreeSet<String>,
}

impl<S: Storage> Storage for NoTimesFor<S> {
    fn get_range(
        &self,
        key: &Key,
        range: core::ops::Range<u64>,
    ) -> persia_storage::Result<ByteSource> {
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
        for meta in &mut listed {
            if self.unknown.contains(meta.key.as_str()) {
                meta.last_modified = None;
            }
        }
        Ok(listed)
    }
    fn delete(&self, key: &Key) -> persia_storage::Result<()> {
        self.inner.delete(key)
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

/// One chunk of unknown age keeps its whole upload, however old its other chunks are; a manifest of unknown age is
/// kept, and so are its upload's old chunks. Objects of known age beside them still go.
#[test]
fn one_object_of_unknown_age_keeps_its_upload_or_its_manifest_and_chunks() {
    let clock = Arc::new(ManualClock::new(at(0)));
    let memory = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    let chunk_key = |upload: u128, index: u32| {
        RemoteKey::Chunk {
            upload_id: UploadId::new(upload),
            index,
        }
        .key()
        .unwrap()
        .as_str()
        .to_owned()
    };
    let unknown_manifest = manifest_of(b"a manifest of unknown age", 4, 2);
    let unknown: BTreeSet<String> = [
        // Chunk 2 of upload 1 (an unbound upload of three chunks).
        chunk_key(1, 2),
        RemoteKey::Manifest(unknown_manifest.blob_id())
            .key()
            .unwrap()
            .as_str()
            .to_owned(),
    ]
    .into();
    let store = RemoteBlobStore::new(NoTimesFor {
        inner: memory,
        unknown,
    });
    let partly_unknown = stage(&store, b"three chunks", 4, 1);
    let kept = put(&store, b"a manifest of unknown age", 4, 2);
    assert_eq!(kept, unknown_manifest);
    let gone = put(&store, b"known and old", 4, 3);
    stage(&store, b"staged, known and old", 4, 4);
    let mark = Mark::default();
    let report = store
        .sweep(&mark, &Pins::default(), &options(at(1000)))
        .unwrap();
    assert_eq!(report.unknown_age, 2, "{report:?}");
    assert_eq!(
        (report.blobs_deleted, report.uploads_deleted),
        (1, 2),
        "{report:?}"
    );
    assert_eq!(
        store
            .staged_chunks(partly_unknown.upload_id())
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        read_all(&store, &kept).unwrap(),
        b"a manifest of unknown age"
    );
    assert!(store.manifest(gone.blob_id()).unwrap().is_none());
    assert!(store.staged_chunks(gone.upload_id()).unwrap().is_empty());
    assert!(store.staged_chunks(UploadId::new(4)).unwrap().is_empty());
}
