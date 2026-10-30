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

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    CommitPolicy, Database, DatabaseError, DatabaseOptions, ManualClock, ReadOnlyDatabase,
    SegmentIds, Snapshot,
};
use persia_engine::document::{Document, FieldSet, Value};
use persia_engine::memtable::FlushPolicy;
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::view::SearchView;
use persia_engine::wal::{Durability, WalRecord, WalWriter};
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Container, ContainerFile, Error as ContainerError, Mode, Repair};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::SimFs;

/// The test schema; `c` is not `strict`, so its puts may carry extra fields.
fn schema(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .strict(name != "c")
        .build()
        .unwrap()
}

/// The test schema with a field added and `strict` flipped (SPEC §6.2.4): `c` becomes `strict`, so a replayed put
/// of `c` with extras would fail the open if the change did not flush it (ROADMAP 6.3.10).
fn evolved(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Text(TextOptions::default()))
        .strict(name == "c")
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
    /// A put with an extra field, to a collection that is not `strict`.
    PutExtra(&'static str, String, String),
    Delete(&'static str, String),
    Commit,
    /// `create_collection` of a collection with the test schema.
    Create(&'static str),
    /// `evolve_collection` into [`evolved`] (ROADMAP 6.3.10).
    Evolve(&'static str),
    /// `drop_collection` (ROADMAP 6.3.10).
    Drop(&'static str),
}

/// A pseudo document id in [`State`] for a collection whose schema change survived: not a valid document id.
const EVOLVED: &str = "#evolved";

/// Documents by collection and id: their body text.
type State = BTreeMap<&'static str, BTreeMap<String, String>>;

fn apply(state: &mut State, op: &Op) {
    match op {
        Op::Put(c, id, text) | Op::PutExtra(c, id, text) => {
            state.entry(c).or_default().insert(id.clone(), text.clone());
        }
        Op::Delete(c, id) => {
            state.entry(c).or_default().remove(id);
        }
        Op::Commit => {}
        Op::Evolve(c) => {
            state
                .entry(c)
                .or_default()
                .insert(EVOLVED.into(), String::new());
        }
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
            0 => ops.extend([
                Op::Create("c"),
                Op::PutExtra("c", "c1".into(), "with extras".into()),
            ]),
            1 => ops.extend([
                Op::Delete("a", "seed".into()),
                Op::Evolve("a"),
                Op::PutExtra("c", "c2".into(), "unflushed extras".into()),
                Op::Evolve("c"),
            ]),
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
        Op::PutExtra(c, id, text) => {
            let mut d = doc(id, text);
            d.insert_extra("color", serde_json::json!("red"));
            db.put(c, &d, Durability::Fsync)
        }
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
        if c.schema().field("tag").is_some() {
            docs.insert(EVOLVED.into(), String::new());
        }
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

/// A commit cut after its Manifest frame but before its superblock slot leaves that Manifest after `log_end`. The
/// reopen must not let it hide what the next writer logs: puts acknowledged after the reopen, with no commit before
/// the next crash, are replayed by the writer's and by a read-only open after it (SPEC §8.6, ROADMAP 6.6.9). Found
/// by `database_process` (`kill_9_loop_through_replay_flushes_merges_and_open`), whose child killed mid-commit was
/// followed by one that only wrote. The reopen retires the Manifest with exactly one commit, only when there is
/// one; a fault at any call of that retirement leaves a state the same holds for.
#[test]
fn writes_after_a_reopen_survive_a_cut_commit_and_another_crash() {
    let created = SimFile::new();
    let mut db = Database::create_on(created.clone(), [4; 16], options()).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("a0", "zero"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("a", &doc("a1", "one"), Durability::Fsync).unwrap();
    let loaded = db.seq();
    let base = created.durable_bytes();
    drop(db);

    let commit_calls = {
        let file = SimFile::from_bytes(base.clone());
        let mut db = Database::open_on(file.clone(), options()).unwrap();
        let start = file.mutations();
        db.commit().unwrap();
        file.mutations() - start
    };
    let (mut stray, mut retirement_faults, mut torn) = (0, 0, 0);
    for point in 0..commit_calls {
        for apply_first in [false, true] {
            let file = SimFile::from_bytes(base.clone());
            let mut db = Database::open_on(file.clone(), options()).unwrap();
            file.set_fault(Some(fault(file.mutations() + point, apply_first)));
            assert!(db.commit().is_err(), "call {point}");
            drop(db);
            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(point)] {
                let ctx = format!("call {point}, apply first {apply_first}, {loss:?}");
                let crashed = file.crash(loss);
                let Some(found) = stray_manifest(&crashed) else {
                    // The container itself does not open read-only (a corrupt tail). This case needs a
                    // readable Manifest; `reopen` in the other crash tests quarantines those tails.
                    torn += 1;
                    continue;
                };
                if found {
                    stray += 1;
                    retirement_faults += cut_retirements(&ctx, &crashed);
                }
                let seq = write_crash_reopen(&ctx, &crashed);
                // The cut commit took effect (its slot written), or the reopen retired its Manifest: one more seq.
                assert!(
                    seq == loaded || seq == loaded + 1,
                    "{ctx}: seq {seq}, loaded {loaded}"
                );
                if found {
                    assert_eq!(seq, loaded + 1, "{ctx}: one retiring commit");
                }
            }
        }
    }
    // The cut that leaves a Manifest without its slot is among the points.
    assert!(
        stray > 0 && retirement_faults > 0,
        "stray {stray}, retirement faults {retirement_faults}"
    );
    eprintln!(
        "{stray} stray Manifests, {retirement_faults} cut retirements, {torn} corrupt tails skipped"
    );
}

/// WAL already logged behind a cut commit's Manifest — the kill-9 failure: the next writer appended
/// acknowledged puts there and died before committing — is visible to a read-only open while the
/// superblock still leaves that Manifest after `log_end`, and to the writer that retires it. Dropping
/// the writer without another flush must not move `wal_start` past that chunk (ROADMAP 6.6.9, SPEC §8.6).
#[test]
fn acknowledged_wal_behind_a_stray_manifest_is_replayed() {
    let crashed = one_stray_manifest();
    append_synced_put_behind(&crashed, "hidden", "behind");
    assert_eq!(
        stray_manifest(&crashed),
        Some(true),
        "the appended chunk stays behind the cut commit's Manifest"
    );

    let ids = ["a0", "a1", "hidden"];
    // No writer has retired the Manifest. Stopping here used to hide `hidden` and still pass `verify`.
    let reader = ReadOnlyDatabase::open_on(crashed.clone(), &AnalyzerRegistry::builtin())
        .unwrap_or_else(|e| panic!("read-only open of the damaged file: {e}"));
    reader
        .verify()
        .unwrap_or_else(|e| panic!("verify of the damaged file: {e}"));
    assert_has(&reader.snapshot(), &ids, "read-only open before retirement");
    drop(reader);

    let db = Database::open_on(crashed.clone(), options()).unwrap();
    assert_has(&db.snapshot(), &ids, "the open that retires the Manifest");
    assert_eq!(stray_manifest(&crashed), Some(false));
    // The retiring commit flushes nothing. A crash here keeps `hidden` only in its chunk: `wal_start`
    // must still cover that chunk, or the next open drops an acknowledged put.
    drop(db);
    let again = crashed.crash(CrashLoss::All);
    let reader = ReadOnlyDatabase::open_on(again.clone(), &AnalyzerRegistry::builtin())
        .unwrap_or_else(|e| panic!("read-only open after the retiring crash: {e}"));
    reader
        .verify()
        .unwrap_or_else(|e| panic!("verify after the retiring crash: {e}"));
    let writer = Database::open_on(again, options()).unwrap();
    assert_has(
        &reader.snapshot(),
        &ids,
        "read-only open after the retiring crash",
    );
    assert_has(
        &writer.snapshot(),
        &ids,
        "writer open after the retiring crash",
    );
}

/// Every `id` is live in collection `a`.
fn assert_has(snapshot: &Snapshot, ids: &[&str], ctx: &str) {
    let a = snapshot.collection("a").unwrap();
    for id in ids {
        assert!(a.get(id).unwrap().is_some(), "{ctx}: {id} missing");
    }
}

/// One file a commit cut before its superblock slot: a valid Manifest frame sits after `log_end`.
fn one_stray_manifest() -> SimFile {
    let created = SimFile::new();
    let mut db = Database::create_on(created.clone(), [4; 16], options()).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("a0", "zero"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("a", &doc("a1", "one"), Durability::Fsync).unwrap();
    let base = created.durable_bytes();
    drop(db);
    let commit_calls = {
        let file = SimFile::from_bytes(base.clone());
        let mut db = Database::open_on(file.clone(), options()).unwrap();
        let start = file.mutations();
        db.commit().unwrap();
        file.mutations() - start
    };
    for point in 0..commit_calls {
        for apply_first in [false, true] {
            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(point)] {
                let file = SimFile::from_bytes(base.clone());
                let mut db = Database::open_on(file.clone(), options()).unwrap();
                file.set_fault(Some(fault(file.mutations() + point, apply_first)));
                assert!(db.commit().is_err(), "call {point}");
                drop(db);
                let crashed = file.crash(loss);
                if stray_manifest(&crashed) == Some(true) {
                    return crashed;
                }
            }
        }
    }
    panic!("no cut commit left a Manifest after log_end");
}

/// Syncs one put of `id` onto `file` without committing, so the chunk lands after the stray Manifest.
fn append_synced_put_behind(file: &SimFile, id: &str, text: &str) {
    let container = Container::open_on(file.clone(), Mode::ReadWrite).unwrap();
    let mut wal = WalWriter::resume(container, BTreeMap::new());
    let encoded = doc(id, text).encode(&schema("a"), FieldSet::All).unwrap();
    wal.append(&WalRecord::put("a", &encoded).unwrap(), Durability::Fsync)
        .unwrap();
}

fn fault(at: u64, apply_first: bool) -> SimFault {
    if apply_first {
        SimFault::ApplyThenFail(at)
    } else {
        SimFault::FailAt(at)
    }
}

/// Whether `file` holds a valid Manifest frame after the committed `log_end`; `None` if a read-only container open
/// fails (a corrupt tail).
fn stray_manifest(file: &SimFile) -> Option<bool> {
    let container = Container::open_on(file.clone(), Mode::ReadOnly).ok()?;
    let log_end = container.superblock().log_end;
    Some(
        container
            .frames()
            .iter()
            .any(|f| f.kind == FrameKind::Manifest && f.offset >= log_end),
    )
}

/// The writer's open of `crashed`, then a put, a crash that keeps only synced bytes, and a read-only and a writer
/// reopen: every acknowledged document is there in both. Returns the `seq` of the first open.
fn write_crash_reopen(ctx: &str, crashed: &SimFile) -> u64 {
    let mut db = Database::open_on(crashed.clone(), options())
        .unwrap_or_else(|e| panic!("{ctx}: reopen: {e}"));
    let seq = db.seq();
    assert_eq!(
        stray_manifest(crashed),
        Some(false),
        "{ctx}: the open left a Manifest after log_end"
    );
    db.put("a", &doc("a2", "two"), Durability::Fsync).unwrap();
    drop(db);
    let again = crashed.crash(CrashLoss::All);
    let reader = ReadOnlyDatabase::open_on(again.clone(), &AnalyzerRegistry::builtin())
        .unwrap_or_else(|e| panic!("{ctx}: read-only open: {e}"));
    reader
        .verify()
        .unwrap_or_else(|e| panic!("{ctx}: verify: {e}"));
    let writer =
        Database::open_on(again, options()).unwrap_or_else(|e| panic!("{ctx}: second reopen: {e}"));
    for (what, snapshot) in [
        ("read-only", reader.snapshot()),
        ("writer", writer.snapshot()),
    ] {
        let a = snapshot.collection("a").unwrap();
        for id in ["a0", "a1", "a2"] {
            assert!(
                a.get(id).unwrap().is_some(),
                "{ctx}: {id} lost ({what} open)"
            );
        }
    }
    seq
}

/// A fault at each call of the open that retires `crashed`'s stray Manifest, a crash, and then
/// [`write_crash_reopen`]: a cut retirement is as harmless as the cut commit. Returns how many it checked.
fn cut_retirements(ctx: &str, crashed: &SimFile) -> usize {
    let calls = {
        let file = SimFile::from_bytes(crashed.bytes());
        let start = file.mutations();
        drop(Database::open_on(file.clone(), options()).unwrap());
        file.mutations() - start
    };
    assert!(calls > 0, "{ctx}: the retiring open wrote nothing");
    let mut checked = 0;
    for point in 0..calls {
        for apply_first in [false, true] {
            let file = SimFile::from_bytes(crashed.bytes());
            file.set_fault(Some(fault(file.mutations() + point, apply_first)));
            assert!(
                Database::open_on(file.clone(), options()).is_err(),
                "{ctx}: retirement call {point}"
            );
            for loss in [CrashLoss::All, CrashLoss::Nothing] {
                let ctx =
                    format!("{ctx}; retirement call {point}, apply first {apply_first}, {loss:?}");
                write_crash_reopen(&ctx, &file.crash(loss));
                checked += 1;
            }
        }
    }
    checked
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
