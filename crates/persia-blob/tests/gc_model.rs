//! GC safety and completeness against a reference model (SPEC §9.5; ROADMAP 9.5.1, 9.5.2, 9.5.4, 9.5.8; ADR-0052),
//! for the local pack (on a `SimFs`) and the remote layout (`MemoryStorage` with a `ManualClock`).
//!
//! The model plays the writer (`persia`'s protocol, SPEC §9.5) and the engine: `put_blob` pins its upload before its
//! first chunk and its blob id before the bind; `put(doc, blob)` logs the document (always `Fsync`, so its pins go at
//! once); a `BlobRef` put again is pinned, looked up, and its upload pinned; deletes and commits change what documents
//! reference; snapshots hold what the view showed when taken (the last commit's documents, and every put of the
//! memtable they share, until the next commit); `get_blob` readers pin a blob and its upload. Since the model knows
//! exactly what the engine's view references, it predicts each sweep exactly.
//!
//! Invariants, after every operation:
//! 1. Safety: a blob a document, a held snapshot, a held `put_blob` or a reader references or pins is bound and reads
//!    back byte for byte; so is one a reader pinned between the mark and the sweep, or at the sweep's first pin
//!    check (its blob id only, ROADMAP 9.5.8).
//! 2. Completeness: after a sweep, the bound blobs are exactly those marked or pinned, plus, remotely, those not older
//!    than the grace period. Locally the rewrite drops the others at once (or nothing, below the ratio, or when a pin
//!    taken after the mark names one it drops).
//! 3. Remotely, no upload is left whose chunks are older than the grace period that no bound manifest names and no
//!    pin holds.
//! 4. A bind answers `Bound` for a blob the model says is gone and `AlreadyBound` for one it says is bound.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure with the operations.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated.

mod gc_support;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gc_support::{Pins, data_of, manifest_of, read_all};
use persia_blob::gc::{GcObject, Mark, Rewrite, Sweep, SweepOptions, SweepReport};
use persia_blob::remote::{RemoteBlobStore, RemoteKey};
use persia_blob::{BindOutcome, BlobId, BlobManifest, BlobStore, PackMode, PackStore, UploadId};
use persia_storage::container::sim::CrashLoss;
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use persia_storage::{KeyPrefix, MemoryStorage, Storage};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

const HOUR: Duration = Duration::from_secs(3600);
const GRACE: Duration = Duration::from_hours(24);
const PACK: &str = "/db/app.persia.blobs";

/// A store under test, and what the model needs of it beyond [`BlobStore`] and [`Sweep`].
trait Backend {
    type Store: BlobStore + Sweep;
    fn store(&self) -> &Self::Store;
    /// The process ends and a new one opens the store (durable state only).
    fn reopen(&mut self);
    /// The backend's clock moves on (the pack has none).
    fn advance(&self, by: Duration);
    fn now(&self) -> SystemTime;
    /// Whether objects age (remote) or a sweep drops what nothing keeps at once (pack).
    fn remote(&self) -> bool;
    /// The staged uploads and each one's newest chunk time, remotely.
    fn uploads(&self) -> BTreeMap<UploadId, SystemTime>;
}

struct Pack {
    fs: SimFs,
    store: PackStore<SimFsFile>,
}

impl Pack {
    fn new() -> Self {
        let fs = SimFs::new();
        fs.create_dir(Path::new("/db")).unwrap();
        fs.sync_dir(Path::new("/")).unwrap();
        let store = Self::open(&fs);
        Self { fs, store }
    }

    fn open(fs: &SimFs) -> PackStore<SimFsFile> {
        PackStore::open_in(Arc::new(fs.clone()), PACK, PackMode::ReadWrite, [9; 16]).unwrap()
    }
}

impl Backend for Pack {
    type Store = PackStore<SimFsFile>;
    fn store(&self) -> &Self::Store {
        &self.store
    }
    fn reopen(&mut self) {
        // Everything unsynced is lost: whatever a bind or a rewrite returned is durable (SPEC §18.10).
        self.fs = self.fs.crash(CrashLoss::All);
        self.store = Self::open(&self.fs);
    }
    fn advance(&self, _: Duration) {}
    fn now(&self) -> SystemTime {
        UNIX_EPOCH
    }
    fn remote(&self) -> bool {
        false
    }
    fn uploads(&self) -> BTreeMap<UploadId, SystemTime> {
        BTreeMap::new()
    }
}

struct Remote {
    clock: Arc<ManualClock>,
    store: RemoteBlobStore<MemoryStorage>,
}

impl Remote {
    fn new() -> Self {
        let clock = Arc::new(ManualClock::new(UNIX_EPOCH + 1000 * HOUR));
        let storage = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        Self {
            clock,
            store: RemoteBlobStore::new(storage),
        }
    }
}

impl Backend for Remote {
    type Store = RemoteBlobStore<MemoryStorage>;
    fn store(&self) -> &Self::Store {
        &self.store
    }
    // The remote store keeps no state of its own: a new process only loses its pins.
    fn reopen(&mut self) {}
    fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }
    fn now(&self) -> SystemTime {
        self.clock.now()
    }
    fn remote(&self) -> bool {
        true
    }
    fn uploads(&self) -> BTreeMap<UploadId, SystemTime> {
        let listed = self
            .store
            .storage()
            .list(&KeyPrefix::new("blobs/uploads/").unwrap())
            .unwrap();
        let mut uploads: BTreeMap<UploadId, SystemTime> = BTreeMap::new();
        for meta in listed {
            if let Some(RemoteKey::Chunk { upload_id, .. }) = RemoteKey::parse(&meta.key) {
                let t = meta.last_modified.unwrap();
                let newest = uploads.entry(upload_id).or_insert(t);
                *newest = (*newest).max(t);
            }
        }
        uploads
    }
}

#[derive(Debug, Clone)]
enum Op {
    /// `put(doc, blob)` with new bytes (a duplicate when `seed` repeats).
    Put {
        doc: u8,
        seed: u8,
        small: bool,
    },
    /// `put(doc, blob)` with a `BlobRef` put before.
    PutStored {
        doc: u8,
        which: usize,
    },
    Delete(u8),
    Commit,
    HoldSnapshot,
    DropSnapshot(usize),
    /// `put_blob`, its pins held.
    HoldBlob {
        seed: u8,
        small: bool,
    },
    DropBlob(usize),
    /// A `get_blob` stream of a blob put before, held.
    HoldReader(usize),
    DropReader(usize),
    Reopen,
    /// `gc` after the clock moves on by `hours`, with a rewrite ratio; maybe a read of a blob put before starts between
    /// the mark and the sweep and stays open: a `get_blob` stream (its blob and upload pinned) or, when `range`, a
    /// `get_blob_range` (its blob id only), and maybe another starts at the sweep's first pin check (its
    /// blob id pinned, its upload not yet).
    Gc {
        hours: u8,
        ratio: f64,
        reader_after_mark: Option<(usize, bool)>,
        late_reader: Option<usize>,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..6_u8, 0..12_u8, prop::bool::weighted(0.3))
            .prop_map(|(doc, seed, small)| Op::Put { doc, seed, small }),
        2 => (0..6_u8, any::<usize>()).prop_map(|(doc, which)| Op::PutStored { doc, which }),
        3 => (0..6_u8).prop_map(Op::Delete),
        3 => Just(Op::Commit),
        1 => Just(Op::HoldSnapshot),
        1 => any::<usize>().prop_map(Op::DropSnapshot),
        2 => (0..12_u8, prop::bool::weighted(0.3))
            .prop_map(|(seed, small)| Op::HoldBlob { seed, small }),
        1 => any::<usize>().prop_map(Op::DropBlob),
        1 => any::<usize>().prop_map(Op::HoldReader),
        1 => any::<usize>().prop_map(Op::DropReader),
        1 => Just(Op::Reopen),
        4 => (
            prop::sample::select(vec![0_u8, 1, 12, 23, 24, 25, 48]),
            prop::sample::select(vec![0.0, 0.0, 0.5, 2.0]),
            prop::option::weighted(0.25, (any::<usize>(), any::<bool>())),
            prop::option::weighted(0.3, any::<usize>()),
        )
            .prop_map(|(hours, ratio, reader_after_mark, late_reader)| Op::Gc {
                hours,
                ratio,
                reader_after_mark,
                late_reader,
            }),
    ]
}

/// A snapshot a reader holds: the documents it shows, and what its view references (the last commit's documents and
/// the puts of the memtable it shares).
struct Held {
    docs: BTreeMap<u8, BlobId>,
    committed: BTreeSet<BlobId>,
    memtable: Rc<RefCell<BTreeSet<BlobId>>>,
}

struct Model<B: Backend> {
    backend: B,
    pins: Pins,
    next_upload: u128,
    /// Every blob ever put: its manifest (as first bound) and bytes.
    blobs: BTreeMap<BlobId, (BlobManifest, Vec<u8>)>,
    /// Ids in the order first put, for `which` indices.
    order: Vec<BlobId>,
    /// What the model says is bound, with the time its manifest was written.
    bound: BTreeMap<BlobId, SystemTime>,
    /// The live documents, and those of the last commit.
    docs: BTreeMap<u8, BlobId>,
    committed: BTreeSet<BlobId>,
    /// Every put since the last commit (the memtable and the WAL from `wal_start` on).
    memtable: Rc<RefCell<BTreeSet<BlobId>>>,
    snapshots: Vec<Held>,
    /// Pins held by `put_blob` callers and readers, each a set of objects.
    held_blobs: Vec<Vec<GcObject>>,
    readers: Vec<Vec<GcObject>>,
}

impl<B: Backend> Model<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            pins: Pins::default(),
            next_upload: 1,
            blobs: BTreeMap::new(),
            order: Vec::new(),
            bound: BTreeMap::new(),
            docs: BTreeMap::new(),
            committed: BTreeSet::new(),
            memtable: Rc::default(),
            snapshots: Vec::new(),
            held_blobs: Vec::new(),
            readers: Vec::new(),
        }
    }

    fn data(seed: u8, small: bool) -> Vec<u8> {
        let mut data = data_of(if small { 9 } else { 30 }, seed);
        data.push(seed);
        data.push(u8::from(small));
        data
    }

    /// `put_blob`'s protocol (SPEC §9.5): returns the blob id and the pins taken, still held.
    fn put_blob(&mut self, seed: u8, small: bool, what: &str) -> (BlobId, Vec<GcObject>) {
        let data = Self::data(seed, small);
        let upload = self.next_upload;
        self.next_upload += 1;
        let store = self.backend.store();
        let mut pins = vec![GcObject::Upload(UploadId::new(upload))];
        self.pins.pin(pins[0]);
        let m = if small {
            manifest_of(&data, 64 << 10, upload)
        } else {
            manifest_of(&data, 4, upload)
        };
        if !small {
            for (i, c) in data.chunks(4).enumerate() {
                store.stage_chunk(m.upload_id(), i as u32, c).unwrap();
            }
        }
        pins.push(GcObject::Blob(m.blob_id()));
        self.pins.pin(GcObject::Blob(m.blob_id()));
        let outcome = if small {
            store.bind_small(&m, &data).unwrap()
        } else {
            store.bind(&m).unwrap()
        };
        let was_bound = self.bound.contains_key(&m.blob_id());
        match outcome {
            // SPEC §10.1: an inline manifest names no upload, so a duplicate's is the same object, which a
            // conditional put takes for its own earlier attempt: `Bound`, and the object (and its time) unchanged.
            BindOutcome::Bound if was_bound => {
                assert!(
                    small && self.backend.remote(),
                    "{what}: Bound for a blob the model holds bound"
                );
            }
            BindOutcome::Bound => {
                self.bound.insert(m.blob_id(), self.backend.now());
                self.blobs.insert(m.blob_id(), (m.clone(), data));
            }
            BindOutcome::AlreadyBound(b) => {
                assert!(
                    was_bound,
                    "{what}: AlreadyBound for a blob the model holds gone"
                );
                let upload = GcObject::Upload(b.upload_id());
                self.pins.pin(upload);
                pins.push(upload);
                store.discard_upload(m.upload_id()).unwrap();
            }
        }
        if !self.order.contains(&m.blob_id()) {
            self.order.push(m.blob_id());
        }
        (m.blob_id(), pins)
    }

    fn release(&self, pins: &[GcObject]) {
        // A pin set counts: release only what no other holder still pins.
        for p in pins {
            let still = self
                .held_blobs
                .iter()
                .chain(&self.readers)
                .any(|h| h.contains(p));
            if !still {
                self.pins.unpin(*p);
            }
        }
    }

    fn log(&mut self, doc: u8, id: BlobId) {
        self.docs.insert(doc, id);
        self.memtable.borrow_mut().insert(id);
    }

    /// A reader's pins on `id` (`get_blob`: the blob, then the upload its manifest names), or `None` when gone.
    fn open_reader(&self, id: BlobId) -> Option<Vec<GcObject>> {
        self.pins.pin(GcObject::Blob(id));
        let Some(m) = self.backend.store().manifest(id).unwrap() else {
            self.release(&[GcObject::Blob(id)]);
            return None;
        };
        self.pins.pin(GcObject::Upload(m.upload_id()));
        Some(vec![GcObject::Blob(id), GcObject::Upload(m.upload_id())])
    }

    /// What the engine's view references now.
    fn referenced(&self) -> BTreeSet<BlobId> {
        let mut ids: BTreeSet<BlobId> = self.docs.values().copied().collect();
        ids.extend(&self.committed);
        ids.extend(self.memtable.borrow().iter());
        for s in &self.snapshots {
            ids.extend(s.docs.values());
            ids.extend(&s.committed);
            ids.extend(s.memtable.borrow().iter());
        }
        ids
    }

    fn pinned_blobs(&self) -> BTreeSet<BlobId> {
        persia_blob::PinGate::pinned(&self.pins)
            .into_iter()
            .filter_map(|o| match o {
                GcObject::Blob(id) => Some(id),
                GcObject::Upload(_) => None,
            })
            .collect()
    }

    fn apply(&mut self, op: &Op, what: &str) {
        match op {
            Op::Put { doc, seed, small } => {
                let (id, pins) = self.put_blob(*seed, *small, what);
                self.log(*doc, id);
                self.release(&pins);
            }
            Op::PutStored { doc, which } => {
                if self.order.is_empty() {
                    return;
                }
                let id = self.order[which % self.order.len()];
                if let Some(pins) = self.open_reader(id) {
                    assert!(
                        self.bound.contains_key(&id),
                        "{what}: a collected blob is found"
                    );
                    self.log(*doc, id);
                    self.release(&pins);
                } else {
                    assert!(
                        !self.bound.contains_key(&id),
                        "{what}: a bound blob is not found"
                    );
                }
            }
            Op::Delete(doc) => {
                self.docs.remove(doc);
            }
            Op::Commit => {
                self.committed = self.docs.values().copied().collect();
                self.memtable = Rc::default();
            }
            Op::HoldSnapshot => self.snapshots.push(Held {
                docs: self.docs.clone(),
                committed: self.committed.clone(),
                memtable: Rc::clone(&self.memtable),
            }),
            Op::DropSnapshot(i) => {
                if !self.snapshots.is_empty() {
                    let len = self.snapshots.len();
                    self.snapshots.remove(i % len);
                }
            }
            Op::HoldBlob { seed, small } => {
                let (_, pins) = self.put_blob(*seed, *small, what);
                // Counted pins: hold them as a new holder, then drop the call's own.
                self.held_blobs.push(pins);
            }
            Op::DropBlob(i) => {
                if !self.held_blobs.is_empty() {
                    let len = self.held_blobs.len();
                    let pins = self.held_blobs.remove(i % len);
                    self.release(&pins);
                }
            }
            Op::HoldReader(i) => {
                if !self.order.is_empty() {
                    let id = self.order[i % self.order.len()];
                    if let Some(pins) = self.open_reader(id) {
                        self.readers.push(pins);
                    }
                }
            }
            Op::DropReader(i) => {
                if !self.readers.is_empty() {
                    let len = self.readers.len();
                    let pins = self.readers.remove(i % len);
                    self.release(&pins);
                }
            }
            Op::Reopen => {
                // The process ends with its readers and pins; the WAL replays every (fsynced) put since the commit.
                self.snapshots.clear();
                self.held_blobs.clear();
                self.readers.clear();
                self.pins = Pins::default();
                self.backend.reopen();
            }
            Op::Gc {
                hours,
                ratio,
                reader_after_mark,
                late_reader,
            } => self.gc(*hours, *ratio, *reader_after_mark, *late_reader, what),
        }
    }

    fn gc(
        &mut self,
        hours: u8,
        ratio: f64,
        reader_after_mark: Option<(usize, bool)>,
        late_reader: Option<usize>,
        what: &str,
    ) {
        self.backend.advance(HOUR * u32::from(hours));
        let now = self.backend.now();
        let mut mark = Mark::new(self.referenced(), persia_blob::PinGate::pinned(&self.pins));
        mark.add_manifest_uploads(self.backend.store()).unwrap();
        assert!(
            mark.missing.is_empty(),
            "{what}: dangling {:?}",
            mark.missing
        );
        // Reads aim at garbage (bound, unmarked) when there is some: there a pin is all that keeps a blob.
        let garbage: Vec<BlobId> = self
            .bound
            .keys()
            .filter(|id| !mark.blobs.contains(id))
            .copied()
            .collect();
        let pick = |i: usize| match (garbage.is_empty(), self.order.is_empty()) {
            (false, _) => Some(garbage[i % garbage.len()]),
            (true, false) => Some(self.order[i % self.order.len()]),
            (true, true) => None,
        };
        // A pin taken after the mark: what it names is kept all the same (SPEC §9.5).
        let after_mark = reader_after_mark.and_then(|(i, range)| Some((pick(i)?, range)));
        let late = late_reader.and_then(pick);
        if let Some((id, range)) = after_mark {
            if range {
                // `get_blob_range`: the blob id pinned, then the read, which finds a collected blob gone.
                self.pins.pin(GcObject::Blob(id));
                if self.backend.store().manifest(id).unwrap().is_some() {
                    self.readers.push(vec![GcObject::Blob(id)]);
                } else {
                    self.release(&[GcObject::Blob(id)]);
                }
            } else if let Some(pins) = self.open_reader(id) {
                self.readers.push(pins);
            }
        }
        // ROADMAP 9.5.8: a `get_blob` that starts at the sweep's first pin check pins the blob id only (its upload
        // comes after its manifest is read). Nothing is deleted before the first check, so the pin protects it whole.
        let fired = Arc::new(std::sync::Mutex::new(false));
        if let Some(id) = late {
            let flag = Arc::clone(&fired);
            self.pins.on_check(move |pinned| {
                let mut fired = flag.lock().unwrap();
                if !*fired {
                    *fired = true;
                    pinned.insert(GcObject::Blob(id));
                }
            });
        }
        let options = SweepOptions {
            grace: GRACE,
            now,
            rewrite_ratio: ratio,
        };
        let pinned_before = persia_blob::PinGate::pinned(&self.pins);
        let report = self.backend.store().sweep(&mark, &self.pins, &options);
        self.pins.on_check(|_| {});
        let late = late.filter(|_| *fired.lock().unwrap());
        self.check_sweep(&mark, &pinned_before, report, late, &options, what);
        if let Some(id) = late {
            // The stream read and was dropped.
            self.release(&[GcObject::Blob(id)]);
        }
    }

    /// Invariants 2 and 3, then the model follows the store.
    fn check_sweep(
        &mut self,
        mark: &Mark,
        pinned_before: &[GcObject],
        report: persia_blob::Result<SweepReport>,
        late: Option<BlobId>,
        options: &SweepOptions,
        what: &str,
    ) {
        let (now, ratio) = (options.now, options.rewrite_ratio);
        let report = report.unwrap_or_else(|e| panic!("{what}: sweep: {e}"));
        let late_bound = late.filter(|id| self.bound.contains_key(id));
        let pinned: BTreeSet<BlobId> = pinned_before
            .iter()
            .filter_map(|o| match o {
                GcObject::Blob(id) => Some(*id),
                GcObject::Upload(_) => None,
            })
            .collect();
        let pinned_uploads: BTreeSet<UploadId> = pinned_before
            .iter()
            .filter_map(|o| match o {
                GcObject::Upload(u) => Some(*u),
                GcObject::Blob(_) => None,
            })
            .collect();
        let remote = self.backend.remote();
        let keep = |id: &BlobId, written: &SystemTime, with_late: bool| {
            let upload = self.blobs[id].0.upload_id();
            mark.blobs.contains(id)
                || pinned.contains(id)
                || (with_late && late_bound == Some(*id))
                // The pack keeps a blob whose upload is marked or pinned too (its chunks are its upload's).
                || (!remote && (mark.uploads.contains(&upload) || pinned_uploads.contains(&upload)))
                || (remote && now.duration_since(*written).is_ok_and(|a| a <= GRACE))
        };
        let would_keep: BTreeSet<BlobId> = self
            .bound
            .iter()
            .filter(|(id, written)| keep(id, written, true))
            .map(|(id, _)| *id)
            .collect();
        let before: BTreeSet<BlobId> = self.bound.keys().copied().collect();
        let expected = if self.backend.remote() {
            assert_eq!(report.rewrite, None, "{what}");
            would_keep
        } else {
            match report.rewrite {
                Some(Rewrite::Done { .. }) => would_keep,
                Some(Rewrite::NoPack) => {
                    assert!(before.is_empty(), "{what}: no pack, yet blobs bound");
                    before.clone()
                }
                Some(Rewrite::BelowRatio { .. }) => {
                    assert!(ratio > 0.0, "{what}: a forced rewrite did not run");
                    before.clone()
                }
                // Only a pin taken after the plan defers it: the late reader's, on a blob the rewrite drops.
                Some(Rewrite::Deferred) => {
                    let late =
                        late_bound.unwrap_or_else(|| panic!("{what}: deferred without a late pin"));
                    assert!(
                        !keep(&late, &self.bound[&late], false),
                        "{what}: deferred by a blob the plan kept"
                    );
                    before.clone()
                }
                None => panic!("{what}: a pack sweep without a rewrite outcome"),
            }
        };
        let deleted = before.difference(&expected).count() as u64;
        assert_eq!(report.blobs_deleted, deleted, "{what}: {report:?}");
        for id in &before {
            let found = self.backend.store().manifest(*id).unwrap().is_some();
            assert_eq!(
                found,
                expected.contains(id),
                "{what}: blob {id} (late {late:?}) {report:?}"
            );
        }
        self.bound.retain(|id, _| expected.contains(id));
        if self.backend.remote() {
            // Invariant 3: every upload left is named by a bound manifest, pinned, or young.
            let named: BTreeSet<UploadId> = self
                .bound
                .keys()
                .map(|id| self.blobs[id].0.upload_id())
                .collect();
            for (upload, newest) in self.backend.uploads() {
                let young = now.duration_since(newest).is_ok_and(|a| a <= GRACE);
                assert!(
                    named.contains(&upload) || pinned_uploads.contains(&upload) || young,
                    "{what}: upload {upload} left behind, {report:?}"
                );
            }
        }
    }

    /// Invariant 1.
    fn check_safety(&self, what: &str) {
        let mut live: BTreeSet<BlobId> = self.docs.values().copied().collect();
        for s in &self.snapshots {
            live.extend(s.docs.values());
        }
        live.extend(self.pinned_blobs());
        for id in live {
            assert!(
                self.bound.contains_key(&id),
                "{what}: a live blob {id} is gone from the model"
            );
            let (m, data) = &self.blobs[&id];
            let read = read_all(self.backend.store(), m)
                .unwrap_or_else(|e| panic!("{what}: live blob {id}: {e}"));
            assert_eq!(&read, data, "{what}: live blob {id}");
        }
    }
}

fn run<B: Backend>(backend: B, ops: &[Op]) {
    let mut model = Model::new(backend);
    for (step, op) in ops.iter().enumerate() {
        let what = format!("step {step} ({op:?})");
        model.apply(op, &what);
        model.check_safety(&what);
    }
    // At rest, with no reader and no pin, a sweep long after the last write keeps exactly what documents reference.
    model.apply(&Op::Commit, "final commit");
    model.apply(&Op::Reopen, "final reopen");
    model.apply(
        &Op::Gc {
            hours: 48,
            ratio: 0.0,
            reader_after_mark: None,
            late_reader: None,
        },
        "final gc",
    );
    model.check_safety("final gc");
    let live: BTreeSet<BlobId> = model.docs.values().copied().collect();
    let bound: BTreeSet<BlobId> = model.bound.keys().copied().collect();
    assert_eq!(bound, live, "orphans after the final gc");
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 1024 } else { 96 },
        rng_seed: RngSeed::Fixed(0x9_5_4_2),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn the_pack_sweep_keeps_what_is_referenced_or_pinned_and_drops_the_rest(
        ops in prop::collection::vec(op(), 1..40),
    ) {
        run(Pack::new(), &ops);
    }

    #[test]
    fn the_remote_sweep_keeps_what_is_referenced_pinned_or_young_and_drops_the_rest(
        ops in prop::collection::vec(op(), 1..40),
    ) {
        run(Remote::new(), &ops);
    }
}

/// The interleavings the 9.5.8 rule is about, pinned down: garbage that a read starts on between the mark and the
/// sweep (a stream, or a range read pinning the blob id only), or at the sweep's first pin check.
#[test]
fn reads_that_start_after_the_mark_keep_garbage_in_both_stores() {
    // Two garbage blobs, so a late pin may name either the first object the rewrite checks or another it drops.
    let garbage = [
        Op::HoldBlob {
            seed: 1,
            small: false,
        },
        Op::HoldBlob {
            seed: 3,
            small: false,
        },
        Op::DropBlob(0),
        Op::DropBlob(0),
        Op::Put {
            doc: 0,
            seed: 2,
            small: true,
        },
    ];
    for (reader_after_mark, late_reader) in [
        (Some((0, false)), None),
        (Some((0, true)), None),
        (Some((1, true)), None),
        (None, Some(0)),
        (None, Some(1)),
    ] {
        let mut ops = garbage.to_vec();
        ops.push(Op::Gc {
            hours: 30,
            ratio: 0.0,
            reader_after_mark,
            late_reader,
        });
        ops.push(Op::DropReader(0));
        run(Pack::new(), &ops);
        run(Remote::new(), &ops);
    }
}
