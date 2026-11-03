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
    CompactionOptions, Database, DatabaseError, DatabaseOptions, ManualClock, ReadOnlyDatabase,
    SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::{CollectionSchema, FieldId, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::Durability;
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
            .any(|o| matches!(o, persia_engine::db::MergeOutcome::Committed { .. })),
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
    let frames = db.container().frames();
    for o in &moved {
        let persia_engine::manifest::Location::Frame { offset, .. } = o.location else {
            panic!("{o:?}");
        };
        // Below `wal_start`, kept frames are not listed by the container; reading checks they are segments there.
        assert!(
            offset < db.container().superblock().log_end,
            "{o:?} {frames:?}"
        );
    }
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
    db.put("a", &doc("d0", "x"), Durability::Buffered).unwrap();
    db.commit().unwrap();
    db.put("a", &doc("d0", "y"), Durability::Buffered).unwrap();
    db.commit().unwrap();
    assert!(db.space_usage().unwrap().dead() > 0);
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
/// then a crash. The database reopens with every acknowledged document, whether the old or the new file survived;
/// an open database whose compaction failed before the rename writes on.
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
            let (mut db, mut docs, gone) = populated(&fs);
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
            if db
                .put("a", &doc("after", "after"), Durability::Fsync)
                .is_ok()
            {
                docs.insert("after".into(), "after".into());
            }
            drop(db);
            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(n)] {
                let crashed = fs.crash(loss);
                let db = open(&crashed).unwrap_or_else(|e| panic!("{ctx}, {loss:?}: {e}"));
                assert_docs(&db, &docs, &gone, &format!("{ctx}, {loss:?}"));
            }
        }
    }
}
