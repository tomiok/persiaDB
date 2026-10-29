//! Schema changes and drops through commit (ROADMAP 6.3.10; SPEC §8.2.1, §8.7.2, §6.2.4).
//!
//! Invariants:
//! 1. `evolve_collection` and `drop_collection` flush every memtable in their commit, so its `wal_start` is the end
//!    of the log: no record encoded under an earlier schema, or of a dropped collection, is replayed. A non-`strict`
//!    put with extra fields, then `strict`, then a crash, reopens with the document.
//! 2. A collection dropped and created again under the same name replays nothing of the dropped one.
//! 3. An evolved collection's segments are opened again with the new schema: a field added since reads as empty;
//!    snapshots taken before keep the old schema.
//! 4. A change §6.2.4 forbids, an unchanged schema and an unknown collection commit nothing.
//! 5. A drop retires the collection's segments and sidecars as obsolete objects.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err,
    missing_docs
)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_engine::db::{
    Database, DatabaseError, DatabaseOptions, ManualClock, MergeOutcome, ObsoleteKind, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::segment::ReadError;
use persia_engine::wal::Durability;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFile};

fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)),
        segment_ids: SegmentIds::seeded(11),
        ..DatabaseOptions::default()
    }
}

fn notes(strict: bool) -> CollectionSchema {
    SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .strict(strict)
        .build()
        .unwrap()
}

fn body_doc(id: &str, text: &str) -> Document {
    let body = notes(true).field("body").unwrap().id();
    Document::new(id).with(body, Value::String(text.into()))
}

fn with_extra(id: &str) -> Document {
    let mut doc = body_doc(id, "kept extras");
    doc.insert_extra("color", serde_json::json!("red"));
    doc
}

fn create(file: &SimFile, schemas: &[CollectionSchema]) -> Database<SimFile> {
    let mut db = Database::create_on(file.clone(), [9; 16], options()).unwrap();
    for s in schemas {
        db.create_collection(s.clone()).unwrap();
    }
    db
}

/// No `WalChunk` frame is at or after the committed `wal_start`: a reopen replays nothing.
fn assert_replays_nothing(db: &Database<SimFile>) {
    let wal_start = db.manifest().wal_start();
    let replayed = db
        .container()
        .frames()
        .iter()
        .filter(|f| f.kind == FrameKind::WalChunk && f.offset >= wal_start)
        .count();
    assert_eq!(replayed, 0, "wal_start {wal_start}");
}

fn reopen(file: &SimFile) -> Database<SimFile> {
    Database::open_on(file.crash(CrashLoss::All), options())
        .unwrap_or_else(|e| panic!("reopen: {e}"))
}

#[test]
fn a_non_strict_put_with_extras_then_strict_reopens_after_a_crash() {
    let file = SimFile::new();
    let mut db = create(&file, &[notes(false)]);
    db.put("notes", &with_extra("n1"), Durability::Fsync)
        .unwrap();
    // The record is logged, not flushed: only the schema commit's flush keeps replay from decoding it as strict.
    assert_eq!(
        db.snapshot().collection("notes").unwrap().segments().len(),
        0
    );

    let seq = db.evolve_collection(&notes(true)).unwrap();
    assert_eq!(seq, db.seq());
    assert_replays_nothing(&db);
    let snapshot = db.snapshot();
    let c = snapshot.collection("notes").unwrap();
    assert!(c.schema().strict());
    assert_eq!((c.segments().len(), c.memtable().live_count()), (1, 0));
    // The new schema applies to new writes.
    let rejected = db.put("notes", &with_extra("n2"), Durability::Fsync);
    assert!(
        matches!(rejected, Err(DatabaseError::Memtable(_))),
        "{rejected:?}"
    );
    drop(db);

    let db = reopen(&file);
    let snapshot = db.snapshot();
    let c = snapshot.collection("notes").unwrap();
    assert!(c.schema().strict());
    let n1 = c.get("n1").unwrap().unwrap();
    assert_eq!(n1.extra().get("color"), Some(&serde_json::json!("red")));
    assert!(c.get("n2").unwrap().is_none());
}

#[test]
fn a_schema_change_flushes_every_memtable() {
    let file = SimFile::new();
    let other = SchemaBuilder::new("other")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let mut db = create(&file, &[notes(false), other]);
    db.put("other", &body_doc("o1", "pending"), Durability::Fsync)
        .unwrap();
    db.evolve_collection(&notes(true)).unwrap();
    let snapshot = db.snapshot();
    let o = snapshot.collection("other").unwrap();
    assert_eq!((o.segments().len(), o.memtable().live_count()), (1, 0));
    assert_replays_nothing(&db);
    drop(db);
    let db = reopen(&file);
    assert!(
        db.snapshot()
            .collection("other")
            .unwrap()
            .get("o1")
            .unwrap()
            .is_some()
    );
}

#[test]
fn an_added_field_reads_as_empty_in_older_segments_and_older_snapshots_keep_their_schema() {
    let file = SimFile::new();
    let mut db = create(&file, &[notes(true)]);
    db.put("notes", &body_doc("n1", "first"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    let before = db.snapshot();

    let v2 = SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    db.evolve_collection(&v2).unwrap();
    let after = db.snapshot();
    let c = after.collection("notes").unwrap();
    let tag = c.schema().field("tag").unwrap().id();
    // The segment written before the field existed is open with the new schema.
    assert_eq!(c.segments()[0].terms(tag).unwrap().len(), 0);
    let old = before.collection("notes").unwrap();
    assert!(old.schema().field("tag").is_none());
    assert!(matches!(
        old.segments()[0].terms(tag),
        Err(ReadError::UnknownField { .. })
    ));

    let tagged = body_doc("n2", "second").with(tag, Value::String("red".into()));
    db.put("notes", &tagged, Durability::Fsync).unwrap();
    db.commit().unwrap();
    drop(db);
    let db = reopen(&file);
    let snapshot = db.snapshot();
    let c = snapshot.collection("notes").unwrap();
    assert_eq!(c.segments().len(), 2);
    assert_eq!(
        c.get("n2").unwrap().unwrap().get(tag),
        Some(&Value::String("red".into()))
    );
}

#[test]
fn a_forbidden_or_empty_change_commits_nothing() {
    let file = SimFile::new();
    let mut db = create(&file, &[notes(true)]);
    db.put("notes", &body_doc("n1", "pending"), Durability::Fsync)
        .unwrap();
    let seq = db.seq();

    let retyped = SchemaBuilder::new("notes")
        .field("body", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let e = db.evolve_collection(&retyped).unwrap_err();
    assert!(matches!(e, DatabaseError::Evolution { .. }), "{e:?}");
    assert!(!e.is_corruption());
    let unknown = SchemaBuilder::new("absent").build().unwrap();
    assert!(matches!(
        db.evolve_collection(&unknown),
        Err(DatabaseError::NoSuchCollection { .. })
    ));
    assert!(matches!(
        db.drop_collection("absent"),
        Err(DatabaseError::NoSuchCollection { .. })
    ));
    // The current schema again: nothing to commit, nothing flushed.
    assert_eq!(db.evolve_collection(&notes(true)).unwrap(), seq);
    assert_eq!(db.seq(), seq);
    let snapshot = db.snapshot();
    assert_eq!(
        snapshot
            .collection("notes")
            .unwrap()
            .memtable()
            .live_count(),
        1
    );
    // Still usable.
    db.commit().unwrap();
}

#[test]
fn a_dropped_collection_is_never_replayed_into_one_created_again() {
    let file = SimFile::new();
    let mut db = create(&file, &[notes(false)]);
    db.put("notes", &body_doc("old1", "committed"), Durability::Fsync)
        .unwrap();
    db.delete("notes", "old1", Durability::Fsync).unwrap();
    db.put("notes", &body_doc("old0", "committed"), Durability::Fsync)
        .unwrap();
    // Keeps the first segment listed once `old0` is deleted, with a sidecar (ROADMAP 6.4.6).
    db.put("notes", &body_doc("keep", "committed"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    db.put("notes", &with_extra("old2"), Durability::Fsync)
        .unwrap();
    db.delete("notes", "old0", Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("notes", &with_extra("old3"), Durability::Fsync)
        .unwrap();
    let reader = db.reader();
    let before = reader.snapshot();

    db.drop_collection("notes").unwrap();
    assert!(db.snapshot().collection("notes").is_none());
    assert_replays_nothing(&db);
    assert!(matches!(
        db.put("notes", &body_doc("x", "y"), Durability::Fsync),
        Err(DatabaseError::NoSuchCollection { .. })
    ));
    // Snapshots taken before still show it.
    assert!(
        before
            .collection("notes")
            .unwrap()
            .get("old3")
            .unwrap()
            .is_some()
    );
    // Its segments and their sidecars are obsolete from the drop on, and held by `before`.
    let obsolete: Vec<_> = db
        .obsolete()
        .map(|(o, held)| (o.kind, o.obsolete_since, held))
        .collect();
    let segments = obsolete
        .iter()
        .filter(|(k, ..)| matches!(k, ObsoleteKind::Segment(_)))
        .count();
    assert_eq!(segments, 2, "{obsolete:?}");
    assert!(
        obsolete
            .iter()
            .any(|(k, ..)| matches!(k, ObsoleteKind::Sidecar { .. }))
    );
    assert!(
        obsolete
            .iter()
            .all(|&(_, since, held)| since == db.seq() && held)
    );
    drop(before);
    assert_eq!(db.take_unreferenced().len(), obsolete.len());

    // The same name, now strict: the dropped collection's extras would fail its replay.
    db.create_collection(notes(true)).unwrap();
    db.put("notes", &body_doc("new1", "fresh"), Durability::Fsync)
        .unwrap();
    drop(db);
    let db = reopen(&file);
    let snapshot = db.snapshot();
    let c = snapshot.collection("notes").unwrap();
    assert_eq!((c.segments().len(), c.memtable().live_count()), (0, 1));
    for id in ["old0", "old1", "old2", "old3", "keep"] {
        assert!(c.get(id).unwrap().is_none(), "{id}");
    }
}

#[test]
fn a_drop_flushes_the_other_memtables() {
    let file = SimFile::new();
    let other = SchemaBuilder::new("other")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let mut db = create(&file, &[notes(true), other]);
    db.put("other", &body_doc("o1", "buffered"), Durability::Buffered)
        .unwrap();
    db.put("notes", &body_doc("n1", "dropped"), Durability::Fsync)
        .unwrap();
    db.drop_collection("notes").unwrap();
    let snapshot = db.snapshot();
    let o = snapshot.collection("other").unwrap();
    assert_eq!((o.segments().len(), o.memtable().live_count()), (1, 0));
    assert_replays_nothing(&db);
    drop(db);
    let db = reopen(&file);
    assert!(db.snapshot().collection("notes").is_none());
    assert!(
        db.snapshot()
            .collection("other")
            .unwrap()
            .get("o1")
            .unwrap()
            .is_some()
    );
}

#[test]
fn deletes_flushed_by_a_schema_change_apply_to_the_reopened_segments() {
    let file = SimFile::new();
    let mut db = create(&file, &[notes(true)]);
    for id in ["n1", "n2"] {
        db.put("notes", &body_doc(id, "committed"), Durability::Fsync)
            .unwrap();
    }
    db.commit().unwrap();
    // A memtable of deletes only: the flush writes no segment, only a sidecar of the reopened segment.
    db.delete("notes", "n1", Durability::Fsync).unwrap();
    db.evolve_collection(&notes(false)).unwrap();
    let check = |db: &Database<SimFile>| {
        let snapshot = db.snapshot();
        let c = snapshot.collection("notes").unwrap();
        assert!(!c.schema().strict());
        assert_eq!(c.segments().len(), 1);
        assert!(c.tombstones()[0].is_some());
        assert!(c.get("n1").unwrap().is_none());
        assert!(c.get("n2").unwrap().is_some());
    };
    check(&db);
    drop(db);
    check(&reopen(&file));
}

#[test]
fn a_segment_retired_after_a_schema_change_stays_held_by_snapshots_of_the_old_schema() {
    let file = SimFile::new();
    let mut db = create(&file, &[notes(true)]);
    db.put("notes", &body_doc("n1", "committed"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    // Holds the segment as opened with the first schema; the writer holds it reopened with the second.
    let before = db.snapshot();
    db.evolve_collection(&notes(false)).unwrap();
    // Deleting its only document retires the segment (at the flush commit, or by a drop merge).
    db.delete("notes", "n1", Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.finish_merges().unwrap();
    assert!(
        db.snapshot()
            .collection("notes")
            .unwrap()
            .segments()
            .is_empty()
    );
    let held = |db: &Database<SimFile>| {
        db.obsolete()
            .find(|(o, _)| matches!(o.kind, ObsoleteKind::Segment(_)))
            .map(|(_, held)| held)
    };
    assert_eq!(held(&db), Some(true));
    assert!(
        db.take_unreferenced()
            .iter()
            .all(|o| !matches!(o.kind, ObsoleteKind::Segment(_)))
    );
    drop(before);
    assert_eq!(held(&db), Some(false));
}

fn merging_options() -> DatabaseOptions {
    DatabaseOptions {
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads: 1,
        },
        ..options()
    }
}

/// A database whose `notes` has two segments, and a merge of them in flight.
fn merging() -> Database<SimFile> {
    let mut db = Database::create_on(SimFile::new(), [9; 16], merging_options()).unwrap();
    db.create_collection(notes(false)).unwrap();
    for id in ["n1", "n2"] {
        db.put(
            "notes",
            &body_doc(id, "one segment each"),
            Durability::Fsync,
        )
        .unwrap();
        db.commit().unwrap();
    }
    assert!(db.merge_step().unwrap().is_empty());
    assert_eq!(db.merges_running(), 1);
    db
}

#[test]
fn a_merge_in_flight_conflicts_with_a_schema_change_or_a_drop() {
    // The job in flight conflicts; `finish_merges` then plans again on the new state.
    let conflicted = |db: &mut Database<SimFile>| {
        let outcomes = db.finish_merges().unwrap();
        assert!(
            matches!(outcomes.first(), Some(MergeOutcome::Conflict { .. })),
            "{outcomes:?}"
        );
        outcomes.len()
    };

    let mut db = merging();
    db.evolve_collection(&notes(true)).unwrap();
    // Merged again, with the new schema.
    assert_eq!(conflicted(&mut db), 2);
    let snapshot = db.snapshot();
    let c = snapshot.collection("notes").unwrap();
    assert!(c.schema().strict());
    assert_eq!(c.segments().len(), 1);

    let mut db = merging();
    db.drop_collection("notes").unwrap();
    let seq = db.seq();
    assert_eq!(conflicted(&mut db), 1);
    assert_eq!(db.seq(), seq, "nothing committed");

    // Created again under the same name, with the same schema: still another collection.
    let mut db = merging();
    db.drop_collection("notes").unwrap();
    db.create_collection(notes(false)).unwrap();
    let seq = db.seq();
    assert_eq!(conflicted(&mut db), 1);
    assert_eq!(db.seq(), seq, "nothing committed");
    assert!(
        db.snapshot()
            .collection("notes")
            .unwrap()
            .segments()
            .is_empty()
    );
}
