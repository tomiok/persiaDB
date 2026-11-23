//! Refreshing a read-only database in place (ROADMAP 6.6.6, SPEC §8.6, ADR-0060).
//!
//! Invariants:
//! 1. After any refresh, the reader shows what a fresh read-only open of the same file shows: the manifest, the
//!    replayed record count, the WAL end, and every document.
//! 2. WAL logged since, and commits that flushed none of the records the reader holds and dropped or changed no
//!    collection (a merge, a created collection), are taken incrementally; segments whose entry is unchanged are
//!    shared with the snapshot before. Anything else reloads; nothing new is `Unchanged`.
//! 3. A snapshot or view taken before keeps the commit it showed; no view sees part of a refresh (the memtables are
//!    held while records apply).
//! 4. Another database at the path, or an older commit, is refused and the reader keeps its state. A refresh at a
//!    path follows a compaction that renamed a new file over it.
//! 5. The open's registry and analysis hold across refreshes: a created collection naming an analyzer the registry
//!    lacks fails the refresh with `UnregisteredAnalyzer` and keeps the state, and a skipping open lists its fields.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use persia_analysis::{AnalyzerRegistry, EdgeNgramFilter, LowercaseFilter, WhitespaceTokenizer};
use persia_engine::db::{
    Database, DatabaseError, DatabaseOptions, ManualClock, ReadOnlyDatabase, Refreshed, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::Durability;
use persia_storage::container::ContainerFile;
use persia_storage::container::sim::SimFile;

fn schema(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn doc(id: &str, text: &str) -> Document {
    let body = schema("a").field("body").unwrap().id();
    Document::new(id).with(body, Value::String(text.into()))
}

/// Inline merges, two segments per tier.
fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(3),
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

fn writer() -> (SimFile, Database<SimFile>) {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [4; 16], options()).unwrap();
    db.create_collection(schema("a")).unwrap();
    (file, db)
}

fn reader(file: &SimFile) -> ReadOnlyDatabase<SimFile> {
    ReadOnlyDatabase::open_on(file.clone(), &AnalyzerRegistry::builtin()).unwrap()
}

/// Invariant 1: `reader` shows what a fresh read-only open of its file shows, for every id in `ids`.
fn assert_fresh<F: ContainerFile + Sync + 'static>(
    reader: &ReadOnlyDatabase<F>,
    fresh: &ReadOnlyDatabase<F>,
    ids: &[&str],
) {
    assert_eq!(reader.seq(), fresh.seq());
    assert_eq!(*reader.manifest(), *fresh.manifest());
    assert_eq!(reader.replayed(), fresh.replayed(), "replayed records");
    assert_eq!(reader.wal_end(), fresh.wal_end(), "WAL end");
    assert_eq!(reader.skipped_analysis(), fresh.skipped_analysis());
    let (got, want) = (reader.snapshot(), fresh.snapshot());
    let names = |s: &persia_engine::db::Snapshot| {
        s.collections()
            .map(|(n, _)| n.to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&got), names(&want));
    for (name, collection) in want.collections() {
        let mine = got.collection(name).unwrap();
        assert_eq!(
            mine.memtable().live_count(),
            collection.memtable().live_count(),
            "{name}"
        );
        assert_eq!(mine.segments().len(), collection.segments().len(), "{name}");
        for id in ids {
            assert_eq!(
                mine.get(id).unwrap(),
                collection.get(id).unwrap(),
                "{name}/{id}"
            );
        }
    }
}

#[test]
fn a_put_since_is_replayed_incrementally_and_earlier_views_keep_theirs() {
    let (file, mut db) = writer();
    db.put("a", &doc("d1", "one"), Durability::Fsync).unwrap();
    let reader = reader(&file);
    let before = reader.snapshot();
    let view = before.collection("a").unwrap().memtable();
    db.put("a", &doc("d2", "two"), Durability::Fsync).unwrap();
    db.delete("a", "d1", Durability::Async).unwrap();
    assert_eq!(
        reader.refresh().unwrap(),
        Refreshed::Incremental {
            seq: before.seq(),
            records: 2
        }
    );
    assert_fresh(&reader, &self::reader(&file), &["d1", "d2"]);
    let now = reader.snapshot();
    assert!(now.collection("a").unwrap().get("d2").unwrap().is_some());
    assert!(now.collection("a").unwrap().get("d1").unwrap().is_none());
    // A view taken before sees what it saw; the snapshot shares the memtables, as a writer's does (SPEC §8.4).
    assert_eq!(view.live_count(), 1);
    assert!(view.lookup("d2").is_none());
    assert_eq!(reader.replayed(), 3);
}

#[test]
fn nothing_written_is_unchanged() {
    let (file, mut db) = writer();
    let reader = reader(&file);
    assert_eq!(reader.refresh().unwrap(), Refreshed::Unchanged);
    // Buffered: still in the writer's memory, not in the file.
    db.put("a", &doc("d1", "one"), Durability::Buffered)
        .unwrap();
    assert_eq!(reader.refresh().unwrap(), Refreshed::Unchanged);
    db.put("a", &doc("d2", "two"), Durability::Fsync).unwrap();
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Incremental { records: 2, .. }
    ));
    assert_eq!(reader.refresh().unwrap(), Refreshed::Unchanged);
    assert_fresh(&reader, &self::reader(&file), &["d1", "d2"]);
}

#[test]
fn a_flush_of_replayed_records_reloads_and_old_snapshots_keep_their_commit() {
    let (file, mut db) = writer();
    db.put("a", &doc("d1", "one"), Durability::Fsync).unwrap();
    let reader = reader(&file);
    let before = reader.snapshot();
    db.commit().unwrap();
    assert_eq!(
        reader.refresh().unwrap(),
        Refreshed::Reloaded { seq: db.seq() }
    );
    assert_fresh(&reader, &self::reader(&file), &["d1"]);
    assert_eq!(before.seq() + 1, reader.seq());
    let old = before.collection("a").unwrap();
    assert!(old.segments().is_empty());
    assert_eq!(old.memtable().live_count(), 1);
    assert_eq!(
        reader.snapshot().collection("a").unwrap().segments().len(),
        1
    );
    assert_eq!(
        reader
            .snapshot()
            .collection("a")
            .unwrap()
            .memtable()
            .live_count(),
        0
    );
}

#[test]
fn a_flush_of_records_never_replayed_is_incremental() {
    let (file, mut db) = writer();
    let reader = reader(&file);
    // Written out by the commit itself, after the reader's scan: the reader never held them.
    db.put("a", &doc("d1", "one"), Durability::Buffered)
        .unwrap();
    db.commit().unwrap();
    assert_eq!(
        reader.refresh().unwrap(),
        Refreshed::Incremental {
            seq: db.seq(),
            records: 0
        }
    );
    assert_fresh(&reader, &self::reader(&file), &["d1"]);
}

#[test]
fn a_merge_beside_unflushed_records_is_incremental_and_shares_unchanged_segments() {
    let (file, mut db) = writer();
    for group in [["s1", "s2"], ["s3", "s4"], ["s5", "s6"]] {
        for id in group {
            db.put("a", &doc(id, "alpha"), Durability::Buffered)
                .unwrap();
        }
        db.commit().unwrap();
    }
    // Holds `wal_start` across the merge commit.
    db.put("a", &doc("m1", "beta"), Durability::Fsync).unwrap();
    let reader = reader(&file);
    let before = reader.snapshot();
    assert_eq!(before.collection("a").unwrap().segments().len(), 3);
    let seq = db.seq();
    while db.seq() == seq {
        db.merge_step().unwrap();
    }
    db.put("a", &doc("m2", "gamma"), Durability::Fsync).unwrap();
    assert_eq!(
        reader.refresh().unwrap(),
        Refreshed::Incremental {
            seq: db.seq(),
            records: 1
        }
    );
    let ids = ["s1", "s2", "s3", "s4", "s5", "s6", "m1", "m2"];
    assert_fresh(&reader, &self::reader(&file), &ids);
    let (old, new) = (before.collection("a").unwrap(), reader.snapshot());
    let new = new.collection("a").unwrap();
    assert!(new.segments().len() < old.segments().len());
    let shared = new
        .segments()
        .iter()
        .filter(|s| old.segments().iter().any(|o| Arc::ptr_eq(o, s)))
        .count();
    assert_eq!(
        shared,
        new.segments().len() - 1,
        "only the merged segment is opened"
    );
    // The snapshot before still has its three segments and every document.
    assert_eq!(old.segments().len(), 3);
    for id in &ids[..6] {
        assert!(old.get(id).unwrap().is_some(), "{id}");
    }
}

#[test]
fn a_created_collection_is_taken_and_a_dropped_or_evolved_one_reloads() {
    let (file, mut db) = writer();
    db.put("a", &doc("d1", "one"), Durability::Fsync).unwrap();
    let reader = reader(&file);
    db.create_collection(schema("b")).unwrap();
    db.put("b", &doc("b1", "bee"), Durability::Fsync).unwrap();
    assert_eq!(
        reader.refresh().unwrap(),
        Refreshed::Incremental {
            seq: db.seq(),
            records: 1
        }
    );
    assert_fresh(&reader, &self::reader(&file), &["d1", "b1"]);

    db.drop_collection("b").unwrap();
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Reloaded { .. }
    ));
    assert_fresh(&reader, &self::reader(&file), &["d1", "b1"]);

    db.put("a", &doc("d2", "two"), Durability::Fsync).unwrap();
    let evolved = SchemaBuilder::new("a")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("n", FieldKind::F64 { stored: true })
        .build()
        .unwrap();
    db.evolve_collection(&evolved).unwrap();
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Reloaded { .. }
    ));
    assert_fresh(&reader, &self::reader(&file), &["d1", "d2"]);
}

#[test]
fn views_never_see_part_of_a_refresh() {
    const BATCH: usize = 40;
    let (file, mut db) = writer();
    let reader = Arc::new(reader(&file));
    let done = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (reader, done) = (Arc::clone(&reader), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut seen = 0u64;
            while !done.load(Ordering::Relaxed) {
                let count = reader
                    .snapshot()
                    .collection("a")
                    .unwrap()
                    .memtable()
                    .live_count();
                assert_eq!(
                    count as usize % BATCH,
                    0,
                    "a view saw part of a chunk: {count}"
                );
                seen += 1;
            }
            seen
        })
    };
    for round in 0..60 {
        // One chunk of `BATCH` records per batch: the batch's documents are synced together.
        let docs: Vec<_> = (0..BATCH)
            .map(|i| doc(&format!("r{round}-{i}"), "word"))
            .collect();
        db.put_batch("a", &docs, Durability::Fsync).unwrap();
        reader.refresh().unwrap();
    }
    done.store(true, Ordering::Relaxed);
    assert!(watcher.join().unwrap() > 0);
    assert_eq!(
        reader
            .snapshot()
            .collection("a")
            .unwrap()
            .memtable()
            .live_count() as usize,
        60 * BATCH
    );
}

#[test]
fn another_database_or_an_older_commit_is_refused_and_a_compaction_is_followed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let older = dir.path().join("older.persia");
    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("d1", "one"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    std::fs::copy(&path, &older).unwrap();
    db.put("a", &doc("d2", "two"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    let reader = ReadOnlyDatabase::open(&path, &AnalyzerRegistry::builtin()).unwrap();
    let seq = reader.seq();

    // A compaction renames a new file over the path: the refresh follows it.
    db.put("a", &doc("d3", "three"), Durability::Fsync).unwrap();
    db.compact().unwrap();
    db.put("a", &doc("d4", "four"), Durability::Fsync).unwrap();
    reader.refresh().unwrap();
    let fresh = ReadOnlyDatabase::open(&path, &AnalyzerRegistry::builtin()).unwrap();
    assert_fresh(&reader, &fresh, &["d1", "d2", "d3", "d4"]);
    assert!(reader.seq() > seq);
    drop(db);

    // An older commit of this database at the path (a restore).
    let current = (reader.seq(), reader.replayed(), reader.wal_end());
    std::fs::rename(&older, &path).unwrap();
    let refused = reader.refresh().unwrap_err();
    assert!(
        matches!(refused, DatabaseError::RefreshRefused { other_database: false, current: c, found } if c == current.0 && found < c),
        "{refused:?}"
    );
    assert!(!refused.is_corruption());
    assert_eq!((reader.seq(), reader.replayed(), reader.wal_end()), current);
    assert!(
        reader
            .snapshot()
            .collection("a")
            .unwrap()
            .get("d4")
            .unwrap()
            .is_some()
    );

    // Another database at the path.
    std::fs::remove_file(&path).unwrap();
    let mut other = Database::create(&path, DatabaseOptions::default()).unwrap();
    other.create_collection(schema("a")).unwrap();
    drop(other);
    let refused = reader.refresh().unwrap_err();
    assert!(
        matches!(
            refused,
            DatabaseError::RefreshRefused {
                other_database: true,
                ..
            }
        ),
        "{refused:?}"
    );
    assert_eq!((reader.seq(), reader.replayed(), reader.wal_end()), current);
}

/// `prefix@1`, which no built-in is.
fn prefix_registry() -> AnalyzerRegistry {
    let mut registry = AnalyzerRegistry::new();
    let analyzer = persia_analysis::Analyzer::builder(WhitespaceTokenizer)
        .filter(LowercaseFilter)
        .filter(
            EdgeNgramFilter::new(NonZeroUsize::new(2).unwrap(), NonZeroUsize::new(4).unwrap())
                .unwrap(),
        )
        .build();
    registry.register("prefix", 1, analyzer).unwrap();
    registry
}

#[test]
fn the_open_registry_and_analysis_hold_across_refreshes() {
    let file = SimFile::new();
    let options = DatabaseOptions {
        registry: prefix_registry(),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(file.clone(), [6; 16], options).unwrap();
    db.create_collection(schema("a")).unwrap();
    let with = ReadOnlyDatabase::open_on(file.clone(), &prefix_registry()).unwrap();
    let without = ReadOnlyDatabase::open_on(file.clone(), &AnalyzerRegistry::builtin()).unwrap();
    let skipping = ReadOnlyDatabase::open_container_skipping_analysis(
        persia_storage::container::Container::open_on(
            file.clone(),
            persia_storage::container::Mode::ReadOnly,
        )
        .unwrap(),
        &AnalyzerRegistry::builtin(),
    )
    .unwrap();
    let notes = CollectionSchema::from_json_in(
        r#"{"name": "notes", "fields": {"title": {"type": "text", "analyzer": "prefix"}}}"#,
        &prefix_registry(),
    )
    .unwrap();
    let title = notes.field("title").unwrap().id();
    db.create_collection(notes).unwrap();
    db.put(
        "notes",
        &Document::new("n1").with(title, Value::String("Persia".into())),
        Durability::Fsync,
    )
    .unwrap();

    assert!(matches!(
        with.refresh().unwrap(),
        Refreshed::Incremental { records: 1, .. }
    ));
    assert!(
        with.snapshot()
            .collection("notes")
            .unwrap()
            .get("n1")
            .unwrap()
            .is_some()
    );
    assert_fresh(
        &with,
        &ReadOnlyDatabase::open_on(file.clone(), &prefix_registry()).unwrap(),
        &["n1"],
    );

    let seq = without.seq();
    let error = without.refresh().unwrap_err();
    assert!(
        matches!(&error, DatabaseError::UnregisteredAnalyzer { collection, field, analyzer }
        if collection == "notes" && field == "title" && analyzer == "prefix@1"),
        "{error:?}"
    );
    assert!(!error.is_corruption());
    assert_eq!(without.seq(), seq);
    assert!(without.snapshot().collection("notes").is_none());

    assert_eq!(skipping.skipped_analysis(), []);
    assert!(matches!(
        skipping.refresh().unwrap(),
        Refreshed::Incremental { records: 1, .. }
    ));
    let skipped = skipping.skipped_analysis();
    assert_eq!(skipped.len(), 1);
    assert_eq!(
        (
            skipped[0].collection.as_str(),
            skipped[0].field.as_str(),
            skipped[0].analyzer.as_str()
        ),
        ("notes", "title", "prefix@1")
    );
    assert!(
        skipping
            .snapshot()
            .collection("notes")
            .unwrap()
            .get("n1")
            .unwrap()
            .is_some()
    );
    // The same as a fresh skipping open, and so after a flush that reloads.
    db.commit().unwrap();
    assert!(matches!(
        skipping.refresh().unwrap(),
        Refreshed::Reloaded { .. }
    ));
    assert_eq!(skipping.skipped_analysis(), skipped);
    skipping.verify().unwrap();
}
