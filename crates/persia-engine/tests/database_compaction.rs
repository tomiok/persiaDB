//! Container compaction driven by the database (ROADMAP 6.5.8; SPEC §4.6, §8.5, §8.7.2).
//!
//! Invariants:
//! 1. A compaction keeps every document: the committed ones, the unflushed ones in the WAL range, and the
//!    acknowledged ones after `log_end`; a reopen (after a crash, too) sees the same, and the file verifies.
//! 2. It reclaims every dead frame: right after it nothing is dead but what a reader still holds.
//! 3. Everything that names an offset follows the new file: later writes, commits, merges in flight and reopens work.
//! 4. An obsolete frame a reader holds is kept, at its new location; one no reader holds is reclaimed.
//! 5. A crash or a failed write at any point of a compaction leaves the old or the new database, never a mix, with
//!    every acknowledged write.
//! 6. It is due at the configured ratio and floor, runs on its own from `merge_step`, and is skipped for a
//!    container without a path.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::result_large_err,
    clippy::assert_is_empty,
    missing_docs
)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    CompactionOptions, Database, DatabaseError, DatabaseOptions, ManualClock, MergeOutcome,
    ReadOnlyDatabase, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::manifest::Location;
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::{CollectionSchema, FieldId, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::Durability;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Container, Error as ContainerError, Mode};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};

const PATH: &str = "/db.persia";

fn schema() -> CollectionSchema {
    SchemaBuilder::new("a")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn body() -> FieldId {
    schema().field("body").unwrap().id()
}

fn doc(id: &str, text: &str) -> Document {
    Document::new(id).with(body(), Value::String(text.into()))
}

/// Merges two segments at a time, inline; compacts only when asked, unless `automatic`.
fn options(automatic: bool) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(11),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        compaction: CompactionOptions {
            automatic,
            dead_space_percent: 40,
            min_dead_bytes: 0,
        },
        ..DatabaseOptions::default()
    }
}

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

fn create(fs: &SimFs, automatic: bool) -> Database<SimFsFile> {
    let container = Container::create_in(vfs(fs), PATH).unwrap();
    let mut db = Database::create_container(container, options(automatic)).unwrap();
    db.create_collection(schema()).unwrap();
    db
}

fn open(fs: &SimFs) -> Result<Database<SimFsFile>, DatabaseError> {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadWrite)?;
    Database::open_container(container, options(false))
}

/// Every live document's body, by id.
type Docs = BTreeMap<String, String>;

fn text(d: &Document) -> String {
    match d.get(body()) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("body {other:?}"),
    }
}

/// The documents `ids` read, by id; `None` for one that is not live.
fn read<F: persia_storage::container::ContainerFile + Sync>(
    db: &Database<F>,
    ids: impl IntoIterator<Item = String>,
) -> BTreeMap<String, Option<String>> {
    let snapshot = db.snapshot();
    let c = snapshot.collection("a").unwrap();
    ids.into_iter()
        .map(|id| {
            let found = c.get(&id).unwrap().map(|d| text(&d));
            (id, found)
        })
        .collect()
}

fn assert_docs<F: persia_storage::container::ContainerFile + Sync>(
    db: &Database<F>,
    expected: &Docs,
    gone: &[String],
    ctx: &str,
) {
    let all = expected.keys().cloned().chain(gone.iter().cloned());
    let found = read(db, all);
    for (id, body) in &found {
        assert_eq!(body.as_ref(), expected.get(id), "{ctx}: {id}");
    }
}

/// A database with dead frames of every kind: replaced segments and sidecars, old manifests, consumed WAL chunks;
/// then unflushed puts in the WAL range and one acknowledged after `log_end`. Returns what it holds and what it
/// deleted.
fn populated(fs: &SimFs) -> (Database<SimFsFile>, Docs, Vec<String>) {
    let mut db = create(fs, false);
    let mut docs = Docs::new();
    for round in 0..6 {
        for i in 0..8 {
            let id = format!("d{i}");
            let body = format!("round {round} item {i} alpha");
            db.put("a", &doc(&id, &body), Durability::Buffered).unwrap();
            docs.insert(id, body);
        }
        db.commit().unwrap();
        db.merge_step().unwrap();
    }
    let gone = vec!["d1".to_owned(), "d5".to_owned()];
    for id in &gone {
        db.delete("a", id, Durability::Buffered).unwrap();
        docs.remove(id);
    }
    db.commit().unwrap();
    db.finish_merges().unwrap();
    // Unflushed and durable: they stay in the WAL from `wal_start`.
    for i in 0..3 {
        let id = format!("w{i}");
        let body = format!("wal {i}");
        db.put("a", &doc(&id, &body), Durability::Fsync).unwrap();
        docs.insert(id, body);
    }
    (db, docs, gone)
}

#[test]
fn compaction_reclaims_dead_frames_and_keeps_every_document() {
    let fs = SimFs::new();
    let (mut db, docs, gone) = populated(&fs);
    let before = db.space_usage().unwrap();
    assert!(before.dead() > 0, "{before:?}");
    let seq = db.seq();

    let done = db.compact().unwrap();
    assert_eq!(done.seq, seq + 1);
    assert_eq!(done.before, before);
    assert_eq!(done.after.dead(), 0, "{done:?}");
    // What was live, and the new Manifest frame (live: it is in the WAL range).
    let manifest = db.container().superblock().manifest_len;
    assert_eq!(done.after.total, before.live + manifest);
    assert_eq!(db.space_usage().unwrap(), done.after);
    assert_eq!(
        fs.read(Path::new(PATH)).unwrap().len() as u64,
        done.after.total
    );
    assert_docs(&db, &docs, &gone, "after compaction");

    // Only the database is left: no temporary file.
    let names: Vec<_> = fs.files().into_iter().map(|(p, _)| p).collect();
    assert_eq!(names, vec![Path::new(PATH).to_path_buf()]);

    drop(db);
    let db = open(&fs.crash(CrashLoss::All)).unwrap();
    assert_docs(&db, &docs, &gone, "reopened");
    drop(db);
    let container = Container::open_in(vfs(&fs), PATH, Mode::ReadOnly).unwrap();
    let ro = ReadOnlyDatabase::open_container(container, &AnalyzerRegistry::builtin()).unwrap();
    ro.verify().unwrap();
}

#[test]
fn writes_commits_and_merges_go_on_after_a_compaction() {
    let fs = SimFs::new();
    let (mut db, mut docs, gone) = populated(&fs);
    db.compact().unwrap();
    let compacted_end = db.container().superblock().log_end;
    // The WAL positions moved: a commit must advance `wal_start` over the relocated chunks, not stall below them.
    for i in 0..8 {
        let id = format!("n{i}");
        db.put("a", &doc(&id, "after"), Durability::Buffered)
            .unwrap();
        docs.insert(id, "after".into());
        db.commit().unwrap();
    }
    db.finish_merges().unwrap();
    // Every record is flushed: `wal_start` is past every frame of the compacted file.
    assert!(db.container().superblock().wal_start >= compacted_end);
    assert_docs(&db, &docs, &gone, "after more commits");
    let done = db.compact().unwrap();
    assert_eq!(done.after.dead(), 0);
    // Acknowledged after the last commit, then compacted: still there after a crash.
    db.put("a", &doc("tail", "tail"), Durability::Fsync)
        .unwrap();
    docs.insert("tail".into(), "tail".into());
    db.compact().unwrap();
    drop(db);
    let db = open(&fs.crash(CrashLoss::All)).unwrap();
    assert_docs(&db, &docs, &gone, "reopened");
}

#[test]
fn a_merge_in_flight_commits_after_a_compaction() {
    let fs = SimFs::new();
    let container = Container::create_in(vfs(&fs), PATH).unwrap();
    let mut opts = options(false);
    opts.merge.threads = 1;
    let mut db = Database::create_container(container, opts).unwrap();
    db.create_collection(schema()).unwrap();
    let mut docs = Docs::new();
    for i in 0..2 {
        let id = format!("m{i}");
        db.put("a", &doc(&id, "merged"), Durability::Buffered)
            .unwrap();
        docs.insert(id, "merged".into());
        db.commit().unwrap();
    }
    db.merge_step().unwrap();
    assert_eq!(db.merges_running(), 1);
    db.compact().unwrap();
    let outcomes = db.finish_merges().unwrap();
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, MergeOutcome::Committed { .. })),
        "{outcomes:?}"
    );
    assert_eq!(db.snapshot().collection("a").unwrap().segments().len(), 1);
    assert_docs(&db, &docs, &[], "merged");
    drop(db);
    let db = open(&fs.crash(CrashLoss::All)).unwrap();
    assert_docs(&db, &docs, &[], "reopened");
}

#[test]
fn an_obsolete_frame_a_reader_holds_is_kept_and_moved() {
    let fs = SimFs::new();
    let mut db = create(&fs, false);
    for i in 0..2 {
        db.put("a", &doc(&format!("k{i}"), "kept"), Durability::Buffered)
            .unwrap();
        db.commit().unwrap();
    }
    let held = db.snapshot();
    db.finish_merges().unwrap();
    let obsolete: Vec<_> = db.obsolete().map(|(o, r)| (o.clone(), r)).collect();
    assert_eq!(obsolete.len(), 2, "{obsolete:?}");
    assert!(obsolete.iter().all(|(_, referenced)| *referenced));

    let done = db.compact().unwrap();
    // The held inputs are still in the file, so not everything is reclaimed.
    let moved: Vec<_> = db.obsolete().map(|(o, _)| o.clone()).collect();
    assert_eq!(moved.len(), 2);
    for (old, new) in obsolete.iter().map(|(o, _)| o).zip(&moved) {
        assert_eq!(
            (old.kind, old.obsolete_since),
            (new.kind, new.obsolete_since)
        );
        assert_ne!(old.location, new.location, "the frame moved");
    }
    // The new locations are the frames: each reads back as a whole Segment frame of the same length there.
    let check_frames = |db: &Database<SimFsFile>| {
        for o in db.obsolete().map(|(o, _)| o) {
            let Location::Frame { offset, len } = o.location else {
                panic!("{o:?}");
            };
            let read = db.container().read_frame(offset, usize::MAX).unwrap();
            assert_eq!(
                (read.frame.kind, read.frame.len),
                (FrameKind::Segment, len),
                "{o:?}"
            );
        }
    };
    check_frames(&db);
    // Compacting again while they are still held: the container takes each named range only if it is exactly one
    // valid frame, so this fails if a relocated offset is wrong.
    let kept = db.compact().unwrap();
    assert_eq!(db.obsolete().count(), 2);
    check_frames(&db);
    assert_eq!(kept.after.dead(), 0, "{kept:?}");
    let held_reads = held.collection("a").unwrap().get("k0").unwrap();
    assert!(held_reads.is_some());

    // Released: the next compaction reclaims them and stops tracking them.
    drop(held);
    let again = db.compact().unwrap();
    assert!(again.after.total < done.after.total, "{again:?} {done:?}");
    assert_eq!(db.obsolete().count(), 0);
    assert!(db.take_unreferenced().is_empty());
    drop(db);
    let db = open(&fs).unwrap();
    assert!(
        db.snapshot()
            .collection("a")
            .unwrap()
            .get("k1")
            .unwrap()
            .is_some()
    );
}

#[test]
fn compaction_is_due_at_the_ratio_and_floor_and_runs_from_merge_step() {
    let fs = SimFs::new();
    let mut db = create(&fs, true);
    let mut compacted = 0;
    let mut last_total = u64::MAX;
    for round in 0..10 {
        for i in 0..4 {
            db.put(
                "a",
                &doc(&format!("d{i}"), &format!("r{round}")),
                Durability::Buffered,
            )
            .unwrap();
        }
        db.commit().unwrap();
        let before = db.seq();
        db.merge_step().unwrap();
        let usage = db.space_usage().unwrap();
        // After every step, compaction is no longer due: the step compacted if it was.
        assert!(!usage.is_due(40), "round {round}: {usage:?}");
        if usage.total < last_total && db.seq() > before {
            compacted += 1;
        }
        last_total = usage.total;
    }
    assert!(compacted > 0);

    // The floor: with 1 GiB required, nothing is due however much is dead.
    let mut opts = options(true);
    opts.compaction.min_dead_bytes = 1 << 30;
    drop(db);
    let container = Container::open_in(vfs(&fs), PATH, Mode::ReadWrite).unwrap();
    let mut db = Database::open_container(container, opts).unwrap();
    for round in 0..8 {
        db.put("a", &doc("d0", &format!("{round}")), Durability::Buffered)
            .unwrap();
        db.commit().unwrap();
    }
    // Due by the ratio alone: only the floor holds it back.
    assert!(db.space_usage().unwrap().is_due(40), "{:?}", db.space_usage());
    assert!(db.compact_if_due().unwrap().is_none());
}

#[test]
fn a_container_without_a_path_is_not_compacted() {
    let file = SimFile::new();
    let mut db = Database::create_on(file, [3; 16], options(true)).unwrap();
    db.create_collection(schema()).unwrap();
    for _ in 0..3 {
        db.put("a", &doc("x", "y"), Durability::Buffered).unwrap();
        db.commit().unwrap();
        db.merge_step().unwrap();
    }
    assert!(db.space_usage().unwrap().dead() > 0);
    assert!(db.compact_if_due().unwrap().is_none());
    let err = db.compact().unwrap_err();
    assert!(
        matches!(
            err,
            DatabaseError::Container(ContainerError::InvalidInput(_))
        ),
        "{err:?}"
    );
    // Not poisoned: it writes on.
    db.put("a", &doc("x", "z"), Durability::Fsync).unwrap();
    db.commit().unwrap();
}

/// Invariant 5: a fault at each mutating call of a compaction (a failed write, or one applied then reported failed),
/// then a crash. The database reopens with every acknowledged document, whether the old or the new file survived.
/// (A simulated fault kills the file system for good, so writing on after a failure is tested on the real one.)
#[test]
fn a_crash_or_failure_at_any_write_of_a_compaction_keeps_every_document() {
    let calls = {
        let fs = SimFs::new();
        let (mut db, _, _) = populated(&fs);
        let start = fs.mutations();
        db.compact().unwrap();
        fs.mutations() - start
    };
    assert!(calls > 5, "{calls}");
    for n in 0..calls {
        for apply_first in [false, true] {
            let ctx = format!("call {n}, apply_first {apply_first}");
            let fs = SimFs::new();
            let (mut db, docs, gone) = populated(&fs);
            let at = fs.mutations() + n;
            fs.set_fault(Some(if apply_first {
                SimFault::ApplyThenFail(at)
            } else {
                SimFault::FailAt(at)
            }));
            let result = db.compact();
            fs.set_fault(None);
            assert!(result.is_err(), "{ctx}: the fault fired");
            // Whatever happened, the handle still reads every document.
            assert_docs(&db, &docs, &gone, &ctx);
            drop(db);
            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(n)] {
                let crashed = fs.crash(loss);
                let db = open(&crashed).unwrap_or_else(|e| panic!("{ctx}, {loss:?}: {e}"));
                assert_docs(&db, &docs, &gone, &format!("{ctx}, {loss:?}"));
            }
        }
    }
}

/// Steps until the merge in flight settles; returns its outcomes.
fn settle(db: &mut Database<SimFsFile>) -> Vec<MergeOutcome> {
    loop {
        let outcomes = db.merge_step().unwrap();
        if !outcomes.is_empty() {
            return outcomes;
        }
        // No sleeps: the worker finishes when it does.
        std::thread::yield_now();
    }
}

/// A compaction between a merge's plan and its commit, with a delete committed into an input in between: the input's
/// superseded sidecar is obsolete and held by the job, so it is kept and moved; the merge then catches up with the
/// delete (ROADMAP 6.5.9) on the new file, and a later compaction reclaims what the job held.
#[test]
fn a_compaction_between_a_merge_plan_and_its_catch_up_keeps_the_deletes() {
    let fs = SimFs::new();
    let container = Container::create_in(vfs(&fs), PATH).unwrap();
    let mut opts = options(false);
    opts.merge.threads = 1;
    let mut db = Database::create_container(container, opts).unwrap();
    db.create_collection(schema()).unwrap();
    let mut docs = Docs::new();
    for (i, ids) in [["a0", "a1", "a2"], ["b0", "b1", "b2"]].iter().enumerate() {
        for id in ids {
            db.put("a", &doc(id, "x"), Durability::Buffered).unwrap();
            docs.insert((*id).to_owned(), "x".into());
        }
        // A first sidecar on the first segment before the plan, so the delete below supersedes one.
        if i == 0 {
            db.commit().unwrap();
            db.delete("a", "a2", Durability::Buffered).unwrap();
            docs.remove("a2");
        }
        db.commit().unwrap();
    }
    db.merge_step().unwrap();
    assert_eq!(db.merges_running(), 1);
    db.delete("a", "a0", Durability::Buffered).unwrap();
    docs.remove("a0");
    db.commit().unwrap();
    let held: Vec<_> = db
        .obsolete()
        .filter(|(_, r)| *r)
        .map(|(o, _)| o.clone())
        .collect();
    assert!(!held.is_empty(), "the job holds the superseded sidecar");
    db.compact().unwrap();
    let moved: Vec<_> = db.obsolete().map(|(o, _)| o.clone()).collect();
    for o in &held {
        assert!(
            moved
                .iter()
                .any(|m| m.kind == o.kind && m.location != o.location),
            "{o:?} {moved:?}"
        );
    }

    let outcomes = settle(&mut db);
    assert!(
        matches!(
            &outcomes[..],
            [MergeOutcome::Committed { reapplied: 1, .. }]
        ),
        "{outcomes:?}"
    );
    let gone = ["a0".to_owned(), "a2".to_owned()];
    assert_docs(&db, &docs, &gone, "merged");
    // The job is gone: what it held is reclaimed now.
    db.finish_merges().unwrap();
    db.compact().unwrap();
    assert!(db.obsolete().all(|(_, referenced)| referenced));
    drop(db);
    let db = open(&fs.crash(CrashLoss::All)).unwrap();
    assert_docs(&db, &docs, &gone, "reopened");
}

/// An automatic compaction that fails does not fail the merge step that started it: its merges are committed and
/// returned, the failure is counted, and the database reopens with every document.
#[test]
fn a_failed_automatic_compaction_does_not_fail_the_merge_step() {
    // The calls of a step that commits one merge and then compacts, without faults, on a copy.
    let prepared = || {
        let fs = SimFs::new();
        let mut db = create(&fs, true);
        // Replaced documents and old manifests: enough dead bytes for the 40% ratio. Each round empties the segment of
        // the round before (dropped by the flush, 6.4.6), so no merge is planned until the last two.
        for round in 0..6 {
            db.put("a", &doc("k0", &format!("{round}")), Durability::Buffered)
                .unwrap();
            db.commit().unwrap();
        }
        db.put("a", &doc("k1", "x"), Durability::Buffered).unwrap();
        db.commit().unwrap();
        (fs, db)
    };
    let (fs, mut db) = prepared();
    let start = fs.mutations();
    let outcomes = db.merge_step().unwrap();
    assert!(
        matches!(&outcomes[..], [MergeOutcome::Committed { .. }]),
        "{outcomes:?}"
    );
    assert_eq!(db.compaction_stats().compacted, 1);
    let total = fs.mutations() - start;
    drop(db);

    // Fail the rename (the compaction's second-to-last call, before the directory sync): before it, so the old file
    // is in place and the database not poisoned.
    let (fs, mut db) = prepared();
    fs.set_fault(Some(SimFault::FailAt(fs.mutations() + total - 2)));
    let outcomes = db.merge_step().unwrap();
    assert!(
        matches!(&outcomes[..], [MergeOutcome::Committed { .. }]),
        "{outcomes:?}"
    );
    let stats = db.compaction_stats();
    assert_eq!((stats.compacted, stats.failed), (0, 1), "{stats:?}");
    drop(db);
    let db = open(&fs.crash(CrashLoss::All)).unwrap();
    assert_eq!(
        db.snapshot().collection("a").unwrap().segments().len(),
        1,
        "the merge is durable"
    );
    let docs: Docs = [("k0", "5"), ("k1", "x")]
        .into_iter()
        .map(|(id, body)| (id.to_owned(), body.to_owned()))
        .collect();
    assert_docs(&db, &docs, &[], "reopened");
}

/// On the real file system: compaction by path, a reopen, and a compaction that fails before its rename (the path
/// no longer names the database) leaves the database writable.
#[test]
fn compaction_works_on_the_real_file_system() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut db = Database::create(&path, options(false)).unwrap();
    db.create_collection(schema()).unwrap();
    let mut docs = Docs::new();
    for round in 0..4 {
        db.put("a", &doc("r", &format!("{round}")), Durability::Fsync)
            .unwrap();
        docs.insert("r".into(), format!("{round}"));
        db.commit().unwrap();
        db.finish_merges().unwrap();
    }
    let done = db.compact().unwrap();
    assert_eq!(done.after.dead(), 0);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), done.after.total);
    drop(db);
    let mut db = Database::open(&path, options(false)).unwrap();
    assert_docs(&db, &docs, &[], "reopened");

    // Moved away from under the writer: the rename is refused, and the database writes on (into the moved file).
    let moved = dir.path().join("moved.persia");
    std::fs::rename(&path, &moved).unwrap();
    db.put("a", &doc("r", "x"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    let err = db.compact().unwrap_err();
    assert!(matches!(err, DatabaseError::Container(_)), "{err:?}");
    assert_eq!(db.compaction_stats().failed, 1);
    db.put("a", &doc("r", "y"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    docs.insert("r".into(), "y".into());
    drop(db);
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        names,
        vec![std::ffi::OsString::from("moved.persia")],
        "no temporary file is left"
    );
    let db = Database::open(&moved, options(false)).unwrap();
    assert_docs(&db, &docs, &[], "moved");
}

/// The WAL positions follow the new file (invariant 3): records still unflushed at a compaction hold `wal_start` back
/// at their relocated chunks, so a later commit that flushes nothing (a merge commit) keeps them, and a crash after
/// it loses none.
#[test]
fn unflushed_records_survive_a_merge_commit_after_a_compaction() {
    let fs = SimFs::new();
    let mut db = create(&fs, false);
    let mut docs = Docs::new();
    // Dead bytes first, so the compaction moves the WAL chunks down.
    for round in 0..4 {
        db.put("a", &doc("old", &format!("{round}")), Durability::Buffered)
            .unwrap();
        docs.insert("old".into(), format!("{round}"));
        db.commit().unwrap();
    }
    db.put("a", &doc("second", "s"), Durability::Buffered).unwrap();
    docs.insert("second".into(), "s".into());
    db.commit().unwrap();
    // Unflushed and durable.
    for i in 0..3 {
        let id = format!("u{i}");
        db.put("a", &doc(&id, "unflushed"), Durability::Fsync).unwrap();
        docs.insert(id, "unflushed".into());
    }
    let before = db.container().superblock().wal_start;
    db.compact().unwrap();
    assert!(db.container().superblock().wal_start < before, "the WAL range moved down");
    let outcomes = db.merge_step().unwrap();
    assert!(matches!(&outcomes[..], [MergeOutcome::Committed { .. }]), "{outcomes:?}");
    drop(db);
    let db = open(&fs.crash(CrashLoss::All)).unwrap();
    assert_docs(&db, &docs, &[], "reopened after the merge commit");
}
