//! GC safety (SPEC §9.5; ROADMAP 9.5.4; ADR-0052): with the local pack, against documents.
//!
//! Invariants:
//! 1. No live blob is ever deleted: across any interleaving of `put(doc, blob)` (bytes or a stored reference, at
//!    every durability), deletes, commits, merges, held snapshots, held `put_blob` results, reopens and GC (the pack
//!    rewritten or not), every blob a document of the database, or of a snapshot a reader holds, references reads back
//!    byte for byte, and so does every blob whose `put_blob` pins are held.
//! 2. A crash between the blob write and the document's commit leaves an orphan blob, never a document without its
//!    blob; the next GC collects the orphan.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)] // Tests: inputs are generated or fixed.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::{Blob, BlobDatabase, DocumentLog};
use persia::internal::gc::{BlobReferences, GcOptions};
use persia::internal::pins::Pins;
use persia_blob::{BlobId, BlobRef, BlobStore, ChunkSize, PackStore};
use persia_engine::db::{
    CollectionSnapshot, Database, DatabaseError, DatabaseOptions, SearchRequest, SegmentIds,
    Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::query::Query;
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::wal::Durability;
use persia_engine::wal::WalMark;
use persia_storage::container::sim::CrashLoss;
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn doc(id: u8) -> Document {
    let body = schema().field("body").unwrap().id();
    Document::new(format!("d{id}")).with(body, Value::String(format!("text of {id}")))
}

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Small blobs go through `bind_small`, large ones are staged in 64 KiB chunks.
fn blob_bytes(seed: u8, large: bool) -> Vec<u8> {
    bytes(if large { (64 << 10) + 100 } else { 100 }, seed)
}

fn put_options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(ChunkSize::MIN).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn db_options() -> DatabaseOptions {
    DatabaseOptions {
        segment_ids: SegmentIds::seeded(17),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        ..DatabaseOptions::default()
    }
}

type Db = BlobDatabase<PackStore>;

/// The engine log on a simulated file, so a [`CrashLoss`] can drop what was not synced.
struct Log(Database<SimFsFile>);

impl DocumentLog for Log {
    fn put(
        &mut self,
        collection: &str,
        doc: &Document,
        durability: Durability,
    ) -> Result<(), DatabaseError> {
        self.0.put(collection, doc, durability)
    }

    fn wal_mark(&self) -> WalMark {
        self.0.wal_mark()
    }

    fn durable_mark(&self) -> WalMark {
        self.0.durable_mark()
    }

    fn commit(&mut self) -> Result<u64, DatabaseError> {
        self.0.commit()
    }
}

impl BlobReferences for Log {
    fn referenced_blobs(&self) -> Result<HashSet<BlobId>, DatabaseError> {
        self.0.referenced_blobs()
    }
}

impl Log {
    fn snapshot(&self) -> Arc<Snapshot> {
        self.0.snapshot()
    }

    fn delete(&mut self, collection: &str, id: &str, durability: Durability) {
        self.0.delete(collection, id, durability).unwrap();
    }

    fn finish_merges(&mut self) {
        self.0.finish_merges().unwrap();
    }
}

type SimDb = BlobDatabase<PackStore<SimFsFile>, Log>;

const SIM_UUID: [u8; 16] = [9; 16];

struct SimOpen {
    fs: SimFs,
    db: SimDb,
}

fn create_sim() -> SimOpen {
    let fs = SimFs::new();
    let file = fs.create_new(Path::new("/app.persia")).unwrap();
    let mut log = Database::create_on(file, SIM_UUID, db_options()).unwrap();
    log.create_collection(schema()).unwrap();
    // The container file's name is durable only after this. Later unsynced bytes are still lost by a crash.
    fs.sync_dir(Path::new("/")).unwrap();
    let pack = PackStore::open_writer_in(
        Arc::new(fs.clone()),
        Path::new("/app.persia.blobs"),
        &log.writer_proof(),
    )
    .unwrap();
    SimOpen {
        fs,
        db: BlobDatabase::new(Log(log), Blobs::new(Arc::new(pack))),
    }
}

fn reopen_sim(fs: &SimFs) -> SimDb {
    let file = fs.open(Path::new("/app.persia"), true).unwrap();
    let log = Database::open_on(file, db_options()).unwrap();
    let pack = PackStore::open_writer_in(
        Arc::new(fs.clone()),
        Path::new("/app.persia.blobs"),
        &log.writer_proof(),
    )
    .unwrap();
    BlobDatabase::new(Log(log), Blobs::new(Arc::new(pack)))
}

fn open(dir: &Path, create: bool) -> Db {
    let path = dir.join("app.persia");
    let log = if create {
        let mut db = Database::create(&path, db_options()).unwrap();
        db.create_collection(schema()).unwrap();
        db
    } else {
        Database::open(&path, db_options()).unwrap()
    };
    let pack = PackStore::open_writer(dir.join("app.persia.blobs"), &log.writer_proof()).unwrap();
    BlobDatabase::new(log, Blobs::new(Arc::new(pack)))
}

/// Every live document of `c` in `snapshot` (segments and memtable), with its blob reference.
fn documents(snapshot: &Snapshot) -> Vec<Document> {
    documents_of(snapshot.collection("c").unwrap())
}

/// Every live document of a collection snapshot.
fn documents_of(c: &CollectionSnapshot) -> Vec<Document> {
    let memtable = c.memtable();
    let plan = SearchPlan::new(
        &Query::MatchAll,
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    let request = SearchRequest {
        limit: 1000,
        ..SearchRequest::default()
    };
    let views = c.views(&memtable).unwrap();
    let results = views.search(&plan, &request, Pruning::default()).unwrap();
    results.hits.into_iter().map(|h| h.document).collect()
}

#[derive(Debug, Clone)]
enum Op {
    Put {
        doc: u8,
        seed: u8,
        large: bool,
        durability: u8,
    },
    PutStored {
        doc: u8,
        which: usize,
    },
    Delete {
        doc: u8,
        durability: u8,
    },
    /// Lose every unsynced write (`CrashLoss::All`) and reopen.
    Crash,
    Commit,
    Merge,
    Gc {
        force: bool,
    },
    HoldBlob {
        seed: u8,
        large: bool,
    },
    DropBlob(usize),
    HoldSnapshot,
    HoldView,
    DropSnapshot(usize),
    Reopen,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..6_u8, any::<u8>(), any::<bool>(), 0..3_u8)
            .prop_map(|(doc, seed, large, durability)| Op::Put { doc, seed, large, durability }),
        2 => (0..6_u8, any::<usize>()).prop_map(|(doc, which)| Op::PutStored { doc, which }),
        3 => (0..6_u8, 0..3_u8).prop_map(|(doc, durability)| Op::Delete { doc, durability }),
        1 => Just(Op::Crash),
        3 => Just(Op::Commit),
        1 => Just(Op::Merge),
        4 => any::<bool>().prop_map(|force| Op::Gc { force }),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(seed, large)| Op::HoldBlob { seed, large }),
        1 => any::<usize>().prop_map(Op::DropBlob),
        1 => Just(Op::HoldSnapshot),
        1 => Just(Op::HoldView),
        1 => any::<usize>().prop_map(Op::DropSnapshot),
        1 => Just(Op::Reopen),
    ]
}

const DURABILITY: [Durability; 3] = [Durability::Buffered, Durability::Async, Durability::Fsync];

/// What the test holds of a running database, and every blob it stored.
struct World {
    fs: SimFs,
    db: Option<SimDb>,
    snapshots: Vec<Arc<Snapshot>>,
    /// Collection snapshots cloned out of a snapshot, which is dropped (ROADMAP 9.5.4 review).
    views: Vec<CollectionSnapshot>,
    held: Vec<(BlobRef, Pins)>,
    stored: Vec<BlobRef>,
    contents: BTreeMap<BlobId, Vec<u8>>,
}

impl World {
    fn db(&self) -> &SimDb {
        self.db.as_ref().expect("open")
    }

    fn db_mut(&mut self) -> &mut SimDb {
        self.db.as_mut().expect("open")
    }

    fn readable(&self, r: &BlobRef, what: &str) {
        let expected = &self.contents[&r.blob_id()];
        let read = self
            .db()
            .get_blob_range(r, 0..r.size())
            .unwrap_or_else(|e| panic!("{what}: blob {} of a live reference: {e}", r.blob_id()));
        assert_eq!(&read, expected, "{what}");
    }

    /// Invariant 1.
    fn check(&self, what: &str) {
        let mut snapshots = vec![self.db().log().snapshot()];
        snapshots.extend(self.snapshots.iter().cloned());
        for snapshot in &snapshots {
            for d in documents(snapshot) {
                let r = d.blob_ref().expect("every document has a blob");
                self.readable(r, what);
            }
        }
        for view in &self.views {
            for d in documents_of(view) {
                self.readable(d.blob_ref().expect("every document has a blob"), what);
            }
        }
        for (r, _) in &self.held {
            self.readable(r, what);
        }
    }
}

impl World {
    fn apply(&mut self, step: usize, op: &Op) {
        match op {
            Op::Put {
                doc: d,
                seed,
                large,
                durability,
            } => {
                let data = blob_bytes(*seed, *large);
                let r = self
                    .db_mut()
                    .put(
                        "c",
                        &doc(*d),
                        Blob::from_reader(&data[..]).with_options(put_options()),
                        DURABILITY[usize::from(*durability)],
                    )
                    .unwrap();
                self.contents.insert(r.blob_id(), data);
                self.stored.push(r);
            }
            Op::PutStored { doc: d, which } => {
                if self.stored.is_empty() {
                    return;
                }
                let r = self.stored[which % self.stored.len()].clone();
                // A stored reference GC has collected since is refused, and nothing is logged.
                let present = self.db_mut().blobs().store().contains(&r).unwrap();
                let put = self
                    .db_mut()
                    .put("c", &doc(*d), Blob::Stored(r), Durability::Fsync);
                assert_eq!(put.is_ok(), present, "step {step}: {put:?}");
            }
            Op::Delete { doc: d, durability } => {
                self.db_mut().log_mut().delete(
                    "c",
                    &format!("d{d}"),
                    DURABILITY[usize::from(*durability)],
                );
            }
            Op::Commit => {
                self.db_mut().commit().unwrap();
            }
            Op::Merge => {
                self.db_mut().log_mut().finish_merges();
            }
            Op::Gc { force } => {
                let options = GcOptions {
                    rewrite_ratio: if *force { 0.0 } else { 0.5 },
                    ..GcOptions::default()
                };
                self.db_mut().gc(&options).unwrap();
            }
            Op::HoldBlob { seed, large } => {
                let data = blob_bytes(*seed, *large);
                let (r, pins) = self.db_mut().put_blob(&data[..], &put_options()).unwrap();
                self.contents.insert(r.blob_id(), data);
                self.stored.push(r.clone());
                self.held.push((r, pins));
            }
            Op::DropBlob(i) => {
                if !self.held.is_empty() {
                    let len = self.held.len();
                    drop(self.held.remove(i % len));
                }
            }
            Op::HoldSnapshot => {
                let snapshot = self.db().log().snapshot();
                self.snapshots.push(snapshot);
            }
            Op::HoldView => {
                let view = self.db().log().snapshot().collection("c").unwrap().clone();
                self.views.push(view);
            }
            Op::DropSnapshot(i) => {
                if !self.snapshots.is_empty() {
                    let len = self.snapshots.len();
                    self.snapshots.remove(i % len);
                }
                if !self.views.is_empty() {
                    let len = self.views.len();
                    self.views.remove(i % len);
                }
            }
            Op::Reopen => {
                // The process ends cleanly: its readers and pins with it. Unsynced bytes stay, as a real file's do
                // when the handle is dropped without a crash.
                self.snapshots.clear();
                self.views.clear();
                self.held.clear();
                drop(self.db.take());
                self.db = Some(reopen_sim(&self.fs));
            }
            Op::Crash => {
                // The process dies: unsynced `Buffered` and `Async` records are gone, synced blobs are not.
                self.snapshots.clear();
                self.views.clear();
                self.held.clear();
                drop(self.db.take());
                self.fs = self.fs.crash(CrashLoss::All);
                self.db = Some(reopen_sim(&self.fs));
            }
        }
    }
}

fn run(ops: &[Op]) {
    let opened = create_sim();
    let mut w = World {
        fs: opened.fs,
        db: Some(opened.db),
        snapshots: Vec::new(),
        views: Vec::new(),
        held: Vec::new(),
        stored: Vec::new(),
        contents: BTreeMap::new(),
    };
    for (step, op) in ops.iter().enumerate() {
        w.apply(step, op);
        w.check(&format!("after step {step} ({op:?}) of {ops:?}"));
    }
    // Finally, with no reader and no pin left, a GC keeps exactly the referenced blobs.
    w.snapshots.clear();
    w.views.clear();
    w.held.clear();
    w.db_mut().commit().unwrap();
    let forced = GcOptions {
        rewrite_ratio: 0.0,
        ..GcOptions::default()
    };
    w.db_mut().gc(&forced).unwrap();
    w.check("after the last GC");
    let live: Vec<BlobId> = documents(&w.db_mut().log().snapshot())
        .iter()
        .map(|d| d.blob_ref().unwrap().blob_id())
        .collect();
    let mut bound = w.db_mut().blobs().store().blob_ids();
    bound.sort_unstable();
    let mut live = live;
    live.sort_unstable();
    live.dedup();
    assert_eq!(bound, live, "orphans left after the last GC: {ops:?}");
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 256 } else { 24 },
        rng_seed: RngSeed::Fixed(0x9_5_4),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn gc_never_deletes_a_live_blob(ops in prop::collection::vec(op(), 1..30)) {
        run(&ops);
    }
}

/// A `Buffered` or `Async` delete or replace is not durable. GC must not drop the blob, and a crash that loses every
/// unsynced write revives the document with that blob intact (SPEC §9.5).
#[test]
fn a_non_durable_delete_or_replace_revives_with_its_blob_after_gc_and_a_crash() {
    for (n, durability) in [Durability::Buffered, Durability::Async]
        .into_iter()
        .enumerate()
    {
        for replace in [false, true] {
            let seed = u8::try_from(n).unwrap().saturating_mul(2) + u8::from(replace);
            let ctx = format!("{durability:?}, replace {replace}, seed {seed}");
            let mut opened = create_sim();
            let original = opened
                .db
                .put(
                    "c",
                    &doc(0),
                    Blob::from_reader(&blob_bytes(seed, true)[..]).with_options(put_options()),
                    Durability::Fsync,
                )
                .unwrap();
            opened.db.commit().unwrap();
            // The pack's name is durable after `put_blob` synced its directory. Sync again so a crash cannot lose it.
            opened.fs.sync_dir(Path::new("/")).unwrap();
            if replace {
                opened
                    .db
                    .put(
                        "c",
                        &doc(0),
                        Blob::from_reader(&blob_bytes(seed.wrapping_add(1), false)[..])
                            .with_options(put_options()),
                        durability,
                    )
                    .unwrap();
            } else {
                opened.db.log_mut().delete("c", "d0", durability);
            }
            let forced = GcOptions {
                rewrite_ratio: 0.0,
                ..GcOptions::default()
            };
            opened.db.gc(&forced).unwrap();
            assert_eq!(
                opened
                    .db
                    .get_blob_range(&original, 0..original.size())
                    .unwrap(),
                blob_bytes(seed, true),
                "{ctx}: gc dropped the blob a crash revives"
            );
            drop(opened.db);
            let fs = opened.fs.crash(CrashLoss::All);
            let db = reopen_sim(&fs);
            let revived = documents(&db.log().snapshot())
                .into_iter()
                .find(|d| d.id() == "d0")
                .unwrap_or_else(|| panic!("{ctx}: the document did not revive"));
            let blob = revived.blob_ref().expect("revived document has its blob");
            assert_eq!(blob, &original, "{ctx}");
            assert_eq!(
                db.get_blob_range(blob, 0..blob.size()).unwrap(),
                blob_bytes(seed, true),
                "{ctx}"
            );
        }
    }
}

/// Invariant 2: `put_blob` bound the blob, then the process died before any document named it.
#[test]
fn a_crash_between_the_blob_and_its_document_leaves_an_orphan_gc_collects() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = open(dir.path(), true);
    let kept = db
        .put(
            "c",
            &doc(1),
            Blob::from_reader(&blob_bytes(1, true)[..]).with_options(put_options()),
            Durability::Fsync,
        )
        .unwrap();
    // The blob is durable (bind returned), the document never logged: the process dies here.
    let (orphan, pins) = db
        .put_blob(&blob_bytes(2, true)[..], &put_options())
        .unwrap();
    // A document logged `Buffered` and never synced is lost with the process; its blob is not.
    let buffered = db
        .put(
            "c",
            &doc(3),
            Blob::from_reader(&blob_bytes(3, false)[..]).with_options(put_options()),
            Durability::Buffered,
        )
        .unwrap();
    drop(pins);
    drop(db);

    let db = open(dir.path(), false);
    let docs = documents(&db.log().snapshot());
    for d in &docs {
        let r = d.blob_ref().unwrap();
        assert!(
            db.blobs().store().contains(r).unwrap(),
            "a document without its blob"
        );
    }
    assert!(
        db.blobs().store().contains(&orphan).unwrap(),
        "bound before the crash"
    );
    let buffered_survived = docs.iter().any(|d| d.blob_ref() == Some(&buffered));
    let report = db.gc(&GcOptions::default()).unwrap();
    assert!(!db.blobs().store().contains(&orphan).unwrap(), "{report:?}");
    assert_eq!(
        db.blobs().store().contains(&buffered).unwrap(),
        buffered_survived
    );
    assert_eq!(
        db.get_blob_range(&kept, 0..kept.size()).unwrap(),
        blob_bytes(1, true)
    );
}

/// A collection snapshot cloned out of a snapshot, which is dropped, keeps the blob of a document replaced since
/// (ROADMAP 9.5.4 review: such a view is a reader too).
#[test]
fn a_held_collection_snapshot_keeps_a_replaced_documents_blob() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = open(dir.path(), true);
    let put = |db: &mut Db, d: u8, seed: u8| {
        db.put(
            "c",
            &doc(d),
            Blob::from_reader(&blob_bytes(seed, true)[..]).with_options(put_options()),
            Durability::Fsync,
        )
        .unwrap()
    };
    put(&mut db, 0, 1);
    let old = put(&mut db, 1, 2);
    db.commit().unwrap();
    let view = db.log().snapshot().collection("c").unwrap().clone();
    let new = put(&mut db, 1, 3);
    db.commit().unwrap();
    let forced = GcOptions {
        rewrite_ratio: 0.0,
        ..GcOptions::default()
    };
    db.gc(&forced).unwrap();
    // Its segment still shows the old copy (its memtable reader shows the newer one first, as any view of it).
    let segment = &view.segments()[0];
    assert!((0..segment.doc_count()).any(|d| segment.blob_ref(d).unwrap().as_ref() == Some(&old)));
    assert_eq!(
        db.get_blob_range(&old, 0..old.size()).unwrap(),
        blob_bytes(2, true)
    );
    drop(view);
    db.gc(&forced).unwrap();
    assert!(!db.blobs().store().contains(&old).unwrap());
    assert!(db.blobs().store().contains(&new).unwrap());
}
