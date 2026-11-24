//! Mapped reads of read-only databases (engine side of ROADMAP 10.2.4; SPEC §4.7; ADR-0017, amended 2026-11-23).
//!
//! Invariants:
//! 1. `ReadOnlyDatabase::open_with_options` maps its container where it is on a local disk, unless its options force
//!    ordinary reads; the older opens, which take a registry alone, read with ordinary reads as before.
//! 2. A read-write `Database` never maps: not when created, opened, committed to, or compacted.
//! 3. The mapping follows a refresh: after a commit it covers the new `log_end`, after a compaction it is of the new
//!    file. A snapshot taken before keeps reading what it showed.
//! 4. Mapped or not, a reader shows the same documents.
//! 5. The default is `MappedReads::Never` on Windows and `MappedReads::IfLocal` elsewhere; the tests that need a
//!    mapping ask for `IfLocal`.
//!
//! The files live in temporary directories, which are on a local disk on the platforms the mapped tests run on (the
//! storage crate's `a_container_on_a_local_disk_is_mapped_automatically` asserts it), so the mapped path really runs.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are fixed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, ManualClock, MappedReads, ReadOnlyDatabase, ReadOnlyOptions,
    Refreshed, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::Durability;
use persia_format::superblock::FIRST_FRAME_OFFSET;

fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn doc(id: &str) -> Document {
    let body = schema().field("body").unwrap().id();
    Document::new(id).with(body, Value::String(format!("text of {id}")))
}

/// No merges: every commit leaves its segment.
fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(24),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: usize::MAX,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        ..DatabaseOptions::default()
    }
}

fn path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("app.persia")
}

/// A database at `path` with one committed segment holding `a` and `b`.
fn writer(path: &Path) -> Database {
    let mut db = Database::create(path, options()).unwrap();
    db.create_collection(schema()).unwrap();
    commit(&mut db, &["a", "b"]);
    db
}

fn commit(db: &mut Database, ids: &[&str]) {
    for id in ids {
        db.put("c", &doc(id), Durability::Fsync).unwrap();
    }
    db.commit().unwrap();
}

/// Invariant 4: `snapshot` holds `ids` and none of `absent`.
fn holds(snapshot: &Snapshot, ids: &[&str], absent: &[&str]) {
    let c = snapshot.collection("c").unwrap();
    for id in ids {
        assert_eq!(c.get(id).unwrap().unwrap(), doc(id), "{id}");
    }
    for id in absent {
        assert_eq!(c.get(id).unwrap(), None, "{id}");
    }
}

/// Options that ask for mapped reads, whatever the platform's default.
fn mapping() -> ReadOnlyOptions {
    ReadOnlyOptions::new().mapped_reads(MappedReads::IfLocal)
}

/// Invariant 5: the default is `Never` on Windows and `IfLocal` elsewhere (SPEC §4.7, ADR-0017 amended).
#[test]
fn the_default_is_never_on_windows_and_if_local_elsewhere() {
    let expected = if cfg!(windows) {
        MappedReads::Never
    } else {
        MappedReads::IfLocal
    };
    assert_eq!(MappedReads::default(), expected);
    #[cfg(windows)]
    assert_eq!(MappedReads::default(), MappedReads::Never);
    // And a default read-only open on Windows reads with ordinary reads.
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let _db = writer(&path);
    let reader = ReadOnlyDatabase::open_with_options(&path, &ReadOnlyOptions::new()).unwrap();
    if cfg!(windows) {
        assert!(!reader.container().is_mapped());
    }
    holds(&reader.snapshot(), &["a", "b"], &["z"]);
}

/// The committed frames of `reader`'s current container: what a mapping of it covers.
fn committed(reader: &ReadOnlyDatabase) -> std::ops::Range<u64> {
    FIRST_FRAME_OFFSET..reader.container().superblock().log_end
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn an_open_with_options_maps_a_container_on_a_local_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let db = writer(&path);
    let reader = ReadOnlyDatabase::open_with_options(&path, &mapping()).unwrap();
    let container = reader.container();
    assert!(container.is_on_local_disk().unwrap());
    assert!(container.is_mapped());
    assert_eq!(container.mapped_range(), Some(committed(&reader)));
    assert_eq!(container.mapping_failures(), 0);
    holds(&reader.snapshot(), &["a", "b"], &["z"]);
    // The default maps too, except on Windows (invariant 5).
    let default = ReadOnlyDatabase::open_with_options(&path, &ReadOnlyOptions::new()).unwrap();
    assert_eq!(default.container().is_mapped(), !cfg!(windows));
    holds(&default.snapshot(), &["a", "b"], &["z"]);
    // The writer next to it is not mapped.
    assert!(!db.container().is_mapped());
}

#[test]
fn the_options_can_force_ordinary_reads_and_the_older_opens_keep_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let mut db = writer(&path);
    let never = ReadOnlyOptions::new().mapped_reads(MappedReads::Never);
    let forced = ReadOnlyDatabase::open_with_options(&path, &never).unwrap();
    let older = ReadOnlyDatabase::open(&path, &AnalyzerRegistry::builtin()).unwrap();
    let skipping =
        ReadOnlyDatabase::open_skipping_analysis(&path, &AnalyzerRegistry::builtin()).unwrap();
    for reader in [&forced, &older, &skipping] {
        assert!(!reader.container().is_mapped());
        assert_eq!(reader.container().mapped_range(), None);
        holds(&reader.snapshot(), &["a", "b"], &["c"]);
    }
    // And after both kinds of refresh: still ordinary reads. Replaying `c` makes the flush commit a reload.
    db.put("c", &doc("c"), Durability::Fsync).unwrap();
    for reader in [&forced, &older, &skipping] {
        assert!(matches!(
            reader.refresh().unwrap(),
            Refreshed::Incremental { records: 1, .. }
        ));
        assert!(!reader.container().is_mapped());
    }
    db.commit().unwrap();
    for reader in [&forced, &older, &skipping] {
        assert!(matches!(
            reader.refresh().unwrap(),
            Refreshed::Reloaded { .. }
        ));
        assert!(!reader.container().is_mapped());
        holds(&reader.snapshot(), &["a", "b", "c"], &[]);
    }
}

#[test]
fn a_read_write_database_never_maps() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let mut db = writer(&path);
    let unmapped = |db: &Database| {
        assert!(!db.container().is_mapped());
        assert_eq!(db.container().mapped_range(), None);
    };
    unmapped(&db);
    commit(&mut db, &["c"]);
    unmapped(&db);
    // Deleting leaves dead space for the compaction to reclaim.
    db.delete("c", "a", Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.compact().unwrap();
    unmapped(&db);
    drop(db);
    let db = Database::open(&path, options()).unwrap();
    unmapped(&db);
    holds(&db.snapshot(), &["b", "c"], &["a"]);
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn a_refresh_maps_the_commits_since() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let mut db = writer(&path);
    let reader = ReadOnlyDatabase::open_with_options(&path, &mapping()).unwrap();
    let first = reader.snapshot();
    let before = committed(&reader);
    assert_eq!(reader.container().mapped_range(), Some(before.clone()));

    // Nothing new: nothing is mapped again, the state stays.
    let unchanged = reader.container();
    assert_eq!(reader.refresh().unwrap(), Refreshed::Unchanged);
    assert!(Arc::ptr_eq(&unchanged, &reader.container()));

    // WAL alone: an incremental refresh, on a container mapped anew over the same committed frames.
    db.put("c", &doc("w"), Durability::Fsync).unwrap();
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Incremental { records: 1, .. }
    ));
    assert!(!Arc::ptr_eq(&unchanged, &reader.container()));
    assert!(reader.container().is_mapped());
    assert_eq!(reader.container().mapped_range(), Some(before.clone()));
    holds(&reader.snapshot(), &["a", "b", "w"], &[]);

    // A commit: the mapping covers the frames it added, the new segment among them.
    db.commit().unwrap();
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Reloaded { .. }
    ));
    let after = committed(&reader);
    assert!(after.end > before.end);
    assert!(reader.container().is_mapped());
    assert_eq!(reader.container().mapped_range(), Some(after));
    assert_eq!(reader.container().mapping_failures(), 0);
    holds(&reader.snapshot(), &["a", "b", "w"], &[]);
    assert_eq!(
        reader.snapshot().collection("c").unwrap().segments().len(),
        2
    );
    // The snapshot taken first reads on, through the mapping it was opened on.
    assert_eq!(first.collection("c").unwrap().segments().len(), 1);
    holds(&first, &["a", "b"], &[]);
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn a_refresh_after_a_compaction_maps_the_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let mut db = writer(&path);
    commit(&mut db, &["c"]);
    let reader = ReadOnlyDatabase::open_with_options(&path, &mapping()).unwrap();
    let first = reader.snapshot();
    let old = reader.container();
    let before = committed(&reader);

    // The delete leaves a dead version behind; the compaction writes a new, smaller file and renames it over the path.
    db.delete("c", "a", Durability::Fsync).unwrap();
    db.commit().unwrap();
    let done = db.compact().unwrap();
    assert!(done.after.total < done.before.total, "{done:?}");

    reader.refresh().unwrap();
    let new = reader.container();
    assert!(!Arc::ptr_eq(&old, &new));
    assert!(new.is_mapped());
    assert_eq!(new.mapped_range(), Some(committed(&reader)));
    assert_eq!(new.superblock().seq, db.seq());
    assert_eq!(new.mapping_failures(), 0);
    holds(&reader.snapshot(), &["b", "c"], &["a"]);
    // The handle and the snapshot of the old file keep its mapping, and its bytes, though the path names another.
    assert_eq!(old.mapped_range(), Some(before));
    holds(&first, &["a", "b", "c"], &[]);
    // A reader opened now sees what the refreshed one sees.
    let fresh = ReadOnlyDatabase::open_with_options(&path, &mapping()).unwrap();
    assert_eq!(fresh.seq(), reader.seq());
    assert_eq!(*fresh.manifest(), *reader.manifest());
    holds(&fresh.snapshot(), &["b", "c"], &["a"]);
}
