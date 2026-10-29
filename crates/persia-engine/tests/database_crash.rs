//! Crash safety of the write path (ROADMAP 6.3.5; SPEC §4.5, §8.2.1, §8.6): a fault at every mutating call of a
//! workload of `Fsync` writes and commits, then a crash that keeps none, all, or a seeded subset of the unsynced
//! writes, then a reopen.
//!
//! The container's file is a `SimFile`, the container's counterpart of `FaultyStorage` (ROADMAP 2.2.4): a fault fails
//! one call and every later one (the process dies there), and `crash` gives the file a restart would find.
//!
//! Invariants, for every fault point, fault kind (fail, or apply then fail) and crash loss:
//! 1. The reopen succeeds and shows the documents of every acknowledged operation, plus at most the one operation
//!    that failed: a commit changes no document, so a commit cut at any step of SPEC §4.5 loses nothing.
//! 2. The committed `seq` is the one before the failed commit or one more; never anything in between.
//! 3. Recovery is idempotent: committing after the reopen, and reopening again, shows the same documents; every one
//!    of these states has at most one live copy of an id in the segments (SPEC §8.3).
//! 4. The same holds for a fault inside `create_collection` (the collection exists after the reopen if and only if
//!    its create was acknowledged, or it is the failed operation) and inside the size-triggered flush and commit a
//!    `put` runs before logging (SPEC §8.2 step 3).
//! 5. The workload changes a schema and drops and recreates a collection while records are unflushed (ROADMAP
//!    6.3.10): a fault in those commits keeps invariants 1 to 3, and the recreated collection never shows the dropped
//!    one's documents.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::result_large_err,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use std::path::Path;

use persia_engine::db::{
    CommitPolicy, Database, DatabaseError, DatabaseOptions, ManualClock, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::FlushPolicy;
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Container, ContainerFile, Error as ContainerError, Mode, Repair};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::SimFs;

fn schema(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

/// The test schema with a field added and `strict` off (SPEC §6.2.4).
fn evolved(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Text(TextOptions::default()))
        .strict(false)
        .build()
        .unwrap()
}

fn doc(id: &str, text: &str) -> Document {
    let body = schema("x").field("body").unwrap().id();
    Document::new(id).with(body, Value::String(text.into()))
}

fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)),
        segment_ids: SegmentIds::seeded(3),
        // Small chunks, so the workload rotates.
        wal_chunk_target: 256,
        ..DatabaseOptions::default()
    }
}

#[derive(Debug, Clone)]
enum Op {
    Put(&'static str, String, String),
    Delete(&'static str, String),
    Commit,
    /// `create_collection` of a collection with the test schema.
    Create(&'static str),
    /// `evolve_collection` into [`evolved`] (ROADMAP 6.3.10).
    Evolve(&'static str),
    /// `drop_collection` (ROADMAP 6.3.10).
    Drop(&'static str),
}

/// Documents by collection and id: their body text.
type State = BTreeMap<&'static str, BTreeMap<String, String>>;

fn apply(state: &mut State, op: &Op) {
    match op {
        Op::Put(c, id, text) => {
            state.entry(c).or_default().insert(id.clone(), text.clone());
        }
        Op::Delete(c, id) => {
            state.entry(c).or_default().remove(id);
        }
        Op::Commit | Op::Evolve(_) => {}
        Op::Create(c) => {
            state.entry(c).or_default();
        }
        Op::Drop(c) => {
            state.remove(c);
        }
    }
}

/// Puts and upserts to two collections, deletes of an id written since the last commit and of committed ids (whose
/// commits write tombstone sidecars, ROADMAP 6.4.3), and commits between them.
fn workload() -> Vec<Op> {
    let mut ops = Vec::new();
    for round in 0..3 {
        for i in 0..4 {
            let text = format!("round {round} item {i} lorem ipsum");
            ops.push(Op::Put("a", format!("a{i}"), text.clone()));
            if i % 2 == 0 {
                ops.push(Op::Put("b", format!("b{round}-{i}"), text));
            }
        }
        ops.push(Op::Put("a", format!("tmp{round}"), "short lived".into()));
        ops.push(Op::Delete("a", format!("tmp{round}")));
        match round {
            // A schema change and a drop with unflushed records, which their commits flush (ROADMAP 6.3.10).
            1 => ops.extend([Op::Delete("a", "seed".into()), Op::Evolve("a")]),
            2 => ops.extend([
                Op::Delete("b", "b0-0".into()),
                Op::Drop("b"),
                Op::Create("b"),
                Op::Put("b", "b-new".into(), "after the drop".into()),
            ]),
            _ => {}
        }
        ops.push(Op::Commit);
    }
    ops.push(Op::Put("b", "tail".into(), "after the last commit".into()));
    ops
}

/// The database `run` builds the workload on: two committed collections and one committed segment.
fn base() -> (SimFile, State) {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [5; 16], options()).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.create_collection(schema("b")).unwrap();
    let mut state = State::new();
    for op in [
        Op::Put("a", "seed".into(), "seeded".into()),
        Op::Put("b", "seed".into(), "seeded".into()),
        Op::Commit,
    ] {
        run(&mut db, &op).unwrap();
        apply(&mut state, &op);
    }
    (file, state)
}

fn run(db: &mut Database<SimFile>, op: &Op) -> Result<(), DatabaseError> {
    match op {
        Op::Put(c, id, text) => db.put(c, &doc(id, text), Durability::Fsync),
        Op::Delete(c, id) => db.delete(c, id, Durability::Fsync),
        Op::Commit => db.commit().map(drop),
        Op::Create(c) => db.create_collection(schema(c)).map(drop),
        Op::Evolve(c) => db.evolve_collection(&evolved(c)).map(drop),
        Op::Drop(c) => db.drop_collection(c).map(drop),
    }
}

fn observed(snapshot: &Snapshot) -> State {
    let mut state = State::new();
    for name in ["a", "b", "c"] {
        // A collection whose create did not survive is absent from the state, not empty.
        let Some(c) = snapshot.collection(name) else {
            continue;
        };
        // A commit leaves at most one live copy of an id in the segments (SPEC §8.3, ADR-0030).
        let mut live = BTreeSet::new();
        for view in c.segment_views() {
            for docid in 0..view.doc_count() {
                if view.is_live(docid) {
                    let id = view.segment().id_of(docid).unwrap();
                    assert!(
                        live.insert(id.clone()),
                        "{name}: {id} is live in two segments"
                    );
                }
            }
        }
        let memtable = c.memtable();
        let mut ids = BTreeSet::new();
        for docid in 0..memtable.doc_count() {
            if memtable.is_live(docid) {
                ids.insert(memtable.document(docid).unwrap().id().to_owned());
            }
        }
        for s in c.segments() {
            for docid in 0..s.doc_count() {
                ids.insert(s.id_of(docid).unwrap());
            }
        }
        let docs = state.entry(name).or_default();
        for id in ids {
            if let Some(d) = c.get(&id).unwrap() {
                let Some(Value::String(text)) = d.fields().next().map(|(_, v)| v.clone()) else {
                    panic!("{id}: no body");
                };
                docs.insert(id, text);
            }
        }
    }
    state
}

/// Opens the database in `crashed`. A crash can leave unsynced sectors on disk out of order, a hole before non-zero
/// bytes: corruption that a read-write open without a path may not discard (SPEC §4.4, ADR-0014). Then the file is
/// opened by path on a simulated file system with `Repair::Quarantine`, which copies the tail aside and truncates it;
/// every acknowledged write precedes the hole. Returns the documents and `seq` it shows, after checking that a commit
/// and another reopen show the same documents (invariant 3).
fn reopen(crashed: &SimFile, ctx: &str, quarantined: &mut usize) -> (State, u64) {
    match Database::open_on(crashed.clone(), options()) {
        Ok(db) => {
            let found = (observed(&db.snapshot()), db.seq());
            recommit(db, ctx, &found.0);
            let db = Database::open_on(crashed.crash(CrashLoss::All), options()).unwrap();
            assert_eq!(
                observed(&db.snapshot()),
                found.0,
                "{ctx}: after commit and reopen"
            );
            found
        }
        Err(DatabaseError::Container(ContainerError::CorruptTail { .. })) => {
            *quarantined += 1;
            let fs = SimFs::new();
            let path = Path::new("/db.persia");
            let file = fs.create_new(path).unwrap();
            file.write_all_at(&crashed.bytes(), 0).unwrap();
            file.sync_data().unwrap();
            fs.sync_dir(Path::new("/")).unwrap();
            let vfs: Arc<dyn Vfs<File = _>> = Arc::new(fs.clone());
            let container = Container::open_in_with(
                Arc::clone(&vfs),
                path,
                Mode::ReadWrite,
                Repair::Quarantine,
            )
            .unwrap_or_else(|e| panic!("{ctx}: quarantining open: {e}"));
            assert!(container.recovery().quarantined.is_some(), "{ctx}");
            let db = Database::open_container(container, options())
                .unwrap_or_else(|e| panic!("{ctx}: open after quarantine: {e}"));
            let found = (observed(&db.snapshot()), db.seq());
            recommit(db, ctx, &found.0);
            let vfs: Arc<dyn Vfs<File = _>> = Arc::new(fs.crash(CrashLoss::All));
            let container = Container::open_in(vfs, path, Mode::ReadWrite).unwrap();
            let db = Database::open_container(container, options()).unwrap();
            assert_eq!(
                observed(&db.snapshot()),
                found.0,
                "{ctx}: after commit and reopen"
            );
            found
        }
        Err(e) => panic!("{ctx}: reopen failed: {e}"),
    }
}

/// Flushes what the reopen replayed, which must keep the documents.
fn recommit<F: ContainerFile + Sync>(mut db: Database<F>, ctx: &str, state: &State) {
    db.commit()
        .unwrap_or_else(|e| panic!("{ctx}: commit after reopen: {e}"));
    assert_eq!(&observed(&db.snapshot()), state, "{ctx}: after the commit");
}

#[test]
fn a_crash_at_any_write_of_the_write_path_keeps_every_acknowledged_operation() {
    let (base_file, base_state) = base();
    let base_bytes = base_file.durable_bytes();
    let ops = workload();

    // The number of mutating calls the workload makes, without faults.
    let total = {
        let file = SimFile::from_bytes(base_bytes.clone());
        let mut db = Database::open_on(file.clone(), options()).unwrap();
        let start = file.mutations();
        for op in &ops {
            run(&mut db, op).unwrap();
        }
        file.mutations() - start
    };
    assert!(total > 40, "{total} mutating calls");

    let mut checked = 0;
    let mut quarantined = 0;
    for point in 0..total {
        for apply_first in [false, true] {
            let file = SimFile::from_bytes(base_bytes.clone());
            let mut db = Database::open_on(file.clone(), options()).unwrap();
            let at = file.mutations() + point;
            file.set_fault(Some(if apply_first {
                SimFault::ApplyThenFail(at)
            } else {
                SimFault::FailAt(at)
            }));
            let mut acknowledged = base_state.clone();
            let mut failed = None;
            let mut seq = db.seq();
            for op in &ops {
                seq = db.seq();
                match run(&mut db, op) {
                    Ok(()) => apply(&mut acknowledged, op),
                    Err(e) => {
                        failed = Some((op.clone(), e));
                        break;
                    }
                }
            }
            let (failed, error) = failed.unwrap_or_else(|| panic!("no fault at call {point}"));
            let mut with_failed = acknowledged.clone();
            apply(&mut with_failed, &failed);
            drop(db);

            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(point)] {
                let ctx = format!(
                    "call {point}, apply first {apply_first}, {loss:?}, {failed:?}: {error}"
                );
                let (state, reopened_seq) = reopen(&file.crash(loss), &ctx, &mut quarantined);
                assert!(
                    state == acknowledged || state == with_failed,
                    "{ctx}\n  observed {state:?}\n  acknowledged {acknowledged:?}"
                );
                let committing = matches!(
                    failed,
                    Op::Commit | Op::Evolve(_) | Op::Drop(_) | Op::Create(_)
                );
                assert!(
                    reopened_seq == seq || (committing && reopened_seq == seq + 1),
                    "{ctx}: seq {reopened_seq}, before the failed operation {seq}"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, total * 6);
    // Deterministic; shown so a change in how often the fallback runs is visible.
    eprintln!("{quarantined} of {checked} crashes left a corrupt tail (quarantined)");
}

/// Options whose every memtable is full after one operation, so each `put` flushes and commits the memtable before
/// logging (SPEC §8.2 step 3).
fn flushing_options() -> DatabaseOptions {
    DatabaseOptions {
        policy: CommitPolicy {
            flush: FlushPolicy {
                memtable_bytes: 1,
                ..FlushPolicy::default()
            },
            ..CommitPolicy::default()
        },
        ..options()
    }
}

/// Invariant 4 (ROADMAP 6.3.5): a fault at every mutating call of `create_collection` and of the size-triggered
/// flush inside `put`. The reopen shows a collection whose create was acknowledged, and one whose create failed
/// either fully or not at all; and every acknowledged put.
#[test]
fn a_crash_inside_create_collection_or_a_size_triggered_flush_keeps_every_acknowledged_operation() {
    let base_bytes = {
        let file = SimFile::new();
        let mut db = Database::create_on(file.clone(), [6; 16], flushing_options()).unwrap();
        db.create_collection(schema("a")).unwrap();
        file.durable_bytes()
    };
    let base_state: State = BTreeMap::from([("a", BTreeMap::new())]);
    // No explicit commit: every segment comes from a size-triggered flush or rides on a create.
    let ops = vec![
        Op::Put("a", "a0".into(), "first".into()),
        Op::Put("a", "a1".into(), "second".into()),
        Op::Create("b"),
        Op::Put("b", "b0".into(), "bravo".into()),
        Op::Put("a", "a0".into(), "first replaced".into()),
        Op::Put("b", "b1".into(), "bravo two".into()),
        Op::Create("c"),
        Op::Put("a", "a2".into(), "third".into()),
        Op::Put("b", "b0".into(), "bravo replaced".into()),
    ];

    let total = {
        let file = SimFile::from_bytes(base_bytes.clone());
        let mut db = Database::open_on(file.clone(), flushing_options()).unwrap();
        let start = file.mutations();
        for op in &ops {
            run(&mut db, op).unwrap();
        }
        // The puts did flush: `a` and `b` hold segments though nothing called `commit`.
        let snapshot = db.snapshot();
        assert!(snapshot.collection("a").unwrap().segments().len() >= 2);
        assert!(!snapshot.collection("b").unwrap().segments().is_empty());
        file.mutations() - start
    };
    assert!(total > 40, "{total} mutating calls");

    let mut checked = 0;
    let mut quarantined = 0;
    for point in 0..total {
        for apply_first in [false, true] {
            let file = SimFile::from_bytes(base_bytes.clone());
            let mut db = Database::open_on(file.clone(), flushing_options()).unwrap();
            let at = file.mutations() + point;
            file.set_fault(Some(if apply_first {
                SimFault::ApplyThenFail(at)
            } else {
                SimFault::FailAt(at)
            }));
            let mut acknowledged = base_state.clone();
            let mut failed = None;
            let mut seq = db.seq();
            for op in &ops {
                seq = db.seq();
                match run(&mut db, op) {
                    Ok(()) => apply(&mut acknowledged, op),
                    Err(e) => {
                        failed = Some((op.clone(), e));
                        break;
                    }
                }
            }
            let (failed, error) = failed.unwrap_or_else(|| panic!("no fault at call {point}"));
            let mut with_failed = acknowledged.clone();
            apply(&mut with_failed, &failed);
            drop(db);

            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(point)] {
                let ctx = format!(
                    "call {point}, apply first {apply_first}, {loss:?}, {failed:?}: {error}"
                );
                let (state, reopened_seq) = reopen(&file.crash(loss), &ctx, &mut quarantined);
                assert!(
                    state == acknowledged || state == with_failed,
                    "{ctx}\n  observed {state:?}\n  acknowledged {acknowledged:?}"
                );
                // A put may have committed its flush before its own record failed; a create commits.
                assert!(
                    reopened_seq == seq || reopened_seq == seq + 1,
                    "{ctx}: seq {reopened_seq}, before the failed operation {seq}"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, total * 6);
    eprintln!("{quarantined} of {checked} crashes left a corrupt tail (quarantined)");
}
