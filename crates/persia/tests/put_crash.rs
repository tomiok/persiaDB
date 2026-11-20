//! Crash order of `put(doc, blob)` (SPEC §9.5, §4.5, §8.2.1; ROADMAP 9.4.6): the blob is durable before the document
//! that references it is logged, at every crash point.
//!
//! The container and the pack share one simulated file system (`SimFs`), so a fault counts every mutating call of
//! both: each write, sync, length change, link, removal and directory sync, of the container's WAL and commits and of
//! the pack's creation, chunks, index frames and slots. A fault fails that call (or applies it, then fails) and every
//! later one, as a process dying there; the crash then keeps none, all, or a seeded subset of the unsynced writes,
//! and the database is reopened with its pack.
//!
//! Invariants, for every fault point, fault kind and crash loss:
//! 1. The reopen succeeds (a corrupt tail is quarantined, as an operator would, ADR-0014).
//! 2. Every blob a document of the reopened database may name (live in a segment or a memtable, or put by a WAL
//!    record from `wal_start` on: `Database::referenced_blobs`) is bound in the pack and reads back whole, hashing to
//!    its id: no WAL record survives without its blob.
//! 3. Every live document is one the workload put, with the reference its `put` returned, or the document of the
//!    `put` that failed; every `Fsync` put acknowledged before the fault is there (puts of distinct ids, so nothing
//!    replaces them).
//! 4. The same after a second fault during the reopen, then a clean reopen.
//!
//! `CrashLoss::Seeded` runs only with the `slow` feature.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::result_large_err
)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::sync::Arc;

use persia::internal::blob::PutBlobOptions;
use persia::internal::documents::{Blob, BlobDatabase};
use persia_blob::{BlobRef, BlobStore, ChunkSize, PackStore, blob_id_of, read_manifest_range};
use persia_engine::db::{
    CollectionSnapshot, Database, DatabaseError, DatabaseOptions, SearchRequest, SegmentIds,
};
use persia_engine::document::Document;
use persia_engine::query::Query;
use persia_engine::schema::SchemaBuilder;
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::wal::Durability;
use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::container::{Container, Error as ContainerError, Mode, Repair};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};

const DB: &str = "/app.persia";
const PACK: &str = "/app.persia.blobs";

type SimDb = BlobDatabase<PackStore<SimFsFile>, Database<SimFsFile>>;

fn options() -> DatabaseOptions {
    DatabaseOptions {
        segment_ids: SegmentIds::seeded(23),
        // Small chunks, so the WAL rotates within the workload.
        wal_chunk_target: 256,
        ..DatabaseOptions::default()
    }
}

fn put_options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(ChunkSize::MIN).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed))
        .collect()
}

/// Large blobs are staged in 64 KiB chunks (two whole ones and a short one); small ones go through `bind_small`.
fn blob(seed: u8, large: bool) -> Vec<u8> {
    bytes(if large { (128 << 10) + 100 } else { 300 }, seed)
}

#[derive(Debug, Clone, Copy)]
enum Op {
    /// `put` of document `d<id>` with new bytes.
    Put {
        id: u8,
        seed: u8,
        large: bool,
        durability: Durability,
    },
    /// `put` of document `d<id>` with the reference an earlier `put` of `d<of>` returned (`Blob::Stored`).
    PutStored {
        id: u8,
        of: u8,
        durability: Durability,
    },
    Commit,
}

fn workload() -> Vec<Op> {
    use Durability::{Async, Buffered, Fsync};
    let put = |id, large, durability| Op::Put {
        id,
        seed: id.wrapping_mul(7),
        large,
        durability,
    };
    vec![
        put(0, true, Fsync),
        put(1, false, Fsync),
        put(2, true, Buffered),
        Op::Commit,
        Op::PutStored {
            id: 3,
            of: 0,
            durability: Fsync,
        },
        put(4, false, Async),
        put(5, true, Fsync),
        Op::PutStored {
            id: 6,
            of: 2,
            durability: Buffered,
        },
        put(7, false, Fsync),
        Op::Commit,
        put(8, false, Fsync),
    ]
}

/// What the workload put: each document's reference, and whether its put was acknowledged at `Fsync`; and the id of
/// the document whose `put` failed, if one did (it may survive the crash, with a reference the workload would have
/// given it).
#[derive(Debug, Default)]
struct Model {
    refs: BTreeMap<String, (BlobRef, bool)>,
    failed: Option<String>,
}

/// A fresh database on `fs`: the container and its collection created before any fault is armed.
fn create(fs: &SimFs) -> SimDb {
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
    let container = Container::create_in(Arc::clone(&vfs), DB).unwrap();
    let mut db = Database::create_container(container, options()).unwrap();
    db.create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    BlobDatabase::with_pack_in(db, vfs, PACK).unwrap()
}

/// Runs the workload until its first error; returns what was acknowledged.
fn run(db: &mut SimDb) -> Model {
    let mut model = Model::default();
    for op in workload() {
        match op {
            Op::Put {
                id,
                seed,
                large,
                durability,
            } => {
                let bytes = blob(seed, large);
                let put = db.put(
                    "c",
                    &Document::new(format!("d{id}")),
                    Blob::from_reader(&bytes[..]).with_options(put_options()),
                    durability,
                );
                let Ok(r) = put else {
                    model.failed = Some(format!("d{id}"));
                    return model;
                };
                model.refs.insert(
                    format!("d{id}"),
                    (r, matches!(durability, Durability::Fsync)),
                );
            }
            Op::PutStored { id, of, durability } => {
                let Some((r, _)) = model.refs.get(&format!("d{of}")).cloned() else {
                    return model;
                };
                let put = db.put(
                    "c",
                    &Document::new(format!("d{id}")),
                    Blob::Stored(r),
                    durability,
                );
                let Ok(r) = put else {
                    model.failed = Some(format!("d{id}"));
                    return model;
                };
                model.refs.insert(
                    format!("d{id}"),
                    (r, matches!(durability, Durability::Fsync)),
                );
            }
            Op::Commit => {
                if db.commit().is_err() {
                    return model;
                }
            }
        }
    }
    model
}

/// Reopens the database and its pack on `crashed`; a corrupt tail is quarantined (ADR-0014).
fn reopen(crashed: &SimFs, ctx: &str) -> SimDb {
    try_reopen(crashed).unwrap_or_else(|e| panic!("{ctx}: {e}"))
}

/// [`reopen`], returning what failed: a reopen that itself meets a fault fails like a process dying there.
fn try_reopen(crashed: &SimFs) -> Result<SimDb, String> {
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(crashed.clone());
    let opened = Container::open_in(Arc::clone(&vfs), DB, Mode::ReadWrite)
        .map_err(DatabaseError::from)
        .and_then(|c| Database::open_container(c, options()));
    let db = match opened {
        Ok(db) => db,
        Err(DatabaseError::Container(ContainerError::CorruptTail { .. })) => {
            let c =
                Container::open_in_with(Arc::clone(&vfs), DB, Mode::ReadWrite, Repair::Quarantine)
                    .map_err(|e| format!("quarantining open: {e}"))?;
            Database::open_container(c, options())
                .map_err(|e| format!("open after quarantine: {e}"))?
        }
        Err(e) => return Err(format!("reopen failed: {e}")),
    };
    BlobDatabase::with_pack_in(db, vfs, PACK).map_err(|e| format!("the pack does not reopen: {e}"))
}

/// Every live document of a collection snapshot (segments and memtable).
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

/// Invariants 2 and 3 on the reopened database.
fn check(db: &SimDb, model: &Model, ctx: &str) {
    let store = db.blobs().store();
    for id in db.log().referenced_blobs().unwrap() {
        let manifest = store
            .manifest(id)
            .unwrap()
            .unwrap_or_else(|| panic!("{ctx}: a document names blob {id}, which is not bound"));
        let bytes = read_manifest_range(&**store, &manifest, 0..manifest.size())
            .unwrap_or_else(|e| panic!("{ctx}: blob {id} does not read back: {e}"));
        assert_eq!(blob_id_of(&bytes), id, "{ctx}: blob {id} reads other bytes");
    }
    let snapshot = db.log().snapshot();
    let docs = documents_of(snapshot.collection("c").unwrap());
    for doc in &docs {
        let r = doc
            .blob_ref()
            .unwrap_or_else(|| panic!("{ctx}: {} lost its reference", doc.id()));
        // The failed operation's document may survive without being in the model: its reference is then one the
        // workload would have given it, which the check above already found bound.
        // Any other document is not one the workload put.
        if let Some((expected, _)) = model.refs.get(doc.id()) {
            assert_eq!(r, expected, "{ctx}: {} names another blob", doc.id());
        } else {
            assert_eq!(
                model.failed.as_deref(),
                Some(doc.id()),
                "{ctx}: {} is live, but neither acknowledged nor the failed put",
                doc.id()
            );
        }
        assert!(
            store.contains(r).unwrap(),
            "{ctx}: {} is dangling",
            doc.id()
        );
    }
    for (id, (_, fsync)) in &model.refs {
        if *fsync {
            assert!(
                docs.iter().any(|d| d.id() == id),
                "{ctx}: the acknowledged Fsync put of {id} was lost"
            );
        }
    }
}

#[test]
fn a_crash_at_every_write_never_leaves_a_document_without_its_blob() {
    // The calls the workload makes, counted on a run without faults.
    let fs = SimFs::new();
    let mut db = create(&fs);
    let start = fs.mutations();
    let complete = run(&mut db);
    let calls = fs.mutations() - start;
    assert_eq!(complete.refs.len(), 9, "the workload runs to the end");
    drop(db);
    check(
        &reopen(&fs.crash(CrashLoss::Nothing), "no fault"),
        &complete,
        "no fault",
    );
    assert!(
        calls > 20,
        "{calls} calls: the workload writes the pack and the WAL"
    );

    let mut runs = 0;
    for n in 0..calls {
        for kind in [SimFault::FailAt, SimFault::ApplyThenFail] {
            // A seeded subset of the unsynced writes only in the extended run: twice the work for each fault point.
            let mut losses = vec![CrashLoss::All, CrashLoss::Nothing];
            if cfg!(feature = "slow") {
                losses.push(CrashLoss::Seeded(n));
            }
            for &loss in &losses {
                let ctx = format!("call {n} {kind:?} {loss:?}");
                let fs = SimFs::new();
                let mut db = create(&fs);
                fs.set_fault(Some(kind(fs.mutations() + n)));
                let model = run(&mut db);
                assert!(fs.crashed(), "{ctx}: the fault fired");
                drop(db);
                let crashed = fs.crash(loss);
                // A second fault, during the reopen itself (its recovery writes and syncs too), on a copy of the
                // crashed state; then a clean reopen of what that left (invariants 1 to 3 again).
                let faulted = crashed.crash(CrashLoss::Nothing);
                let db = reopen(&crashed, &ctx);
                check(&db, &model, &ctx);
                let again = format!("{ctx}, then a fault at reopen call {}", n % 8);
                faulted.set_fault(Some(kind(faulted.mutations() + n % 8)));
                // It may fail anywhere, or finish if it makes fewer calls: either way, a process ending there.
                drop(try_reopen(&faulted));
                let db = reopen(&faulted.crash(loss), &again);
                check(&db, &model, &again);
                runs += 1;
            }
        }
    }
    assert_eq!(runs, calls * if cfg!(feature = "slow") { 6 } else { 4 });
}
