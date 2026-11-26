//! The mark-only open (ROADMAP 10.4.7; SPEC §9.5, §8.6; ADR-0069): a database whose schema names a registered analyzer,
//! opened for GC's mark by a handle that lacks it.
//!
//! Invariants:
//! 1. Its mark is the writer's: `referenced_blobs` equals the application's (`Database::open` with the analyzer), every
//!    reference of the segments and of the skipped collection's WAL records (put, replaced, in a later chunk) included,
//!    and the open writes nothing to a cleanly closed database.
//! 2. It refuses, before writing anything, an open whose replay would flush a memtable of a collection whose analysis
//!    it skips (the same refusal as the compaction-only open: one function).
//! 3. Without a skipped field it is the writer's open: a backlog over the memtable size is flushed.
//! 4. It takes the writer lock (a live writer makes it fail with `Locked`), needs a read-write container, and gives
//!    the writer proof of the container it holds.
//! 5. A cut commit with skipped records in the WAL is retired with one commit that flushes nothing and is not refused;
//!    the records stay WAL records and their blobs are marked.
//! 6. A refusal after the replay committed flushes of another collection loses nothing: a second open is refused alike,
//!    and the application reaches every document and marks every blob.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err,
    missing_docs
)]

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use persia_analysis::{Analyzer, AnalyzerRegistry, LowercaseFilter, WhitespaceTokenizer};
use persia_engine::db::{Database, DatabaseError, DatabaseOptions, MarkOnlyDatabase};
use persia_engine::document::{Document, Value};
use persia_engine::schema::CollectionSchema;
use persia_engine::wal::Durability;
use persia_format::frame::FrameKind;
use persia_format::{BlobId, BlobRef};
use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::container::{Container, Error as ContainerError, Mode};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};

const PATH: &str = "/db.persia";

/// `tags@1`: whitespace, lowercased. Not a built-in, so a handle with the built-ins only skips it.
fn registry() -> AnalyzerRegistry {
    let mut registry = AnalyzerRegistry::new();
    let analyzer = Analyzer::builder(WhitespaceTokenizer)
        .filter(LowercaseFilter)
        .build();
    registry.register("tags", 1, analyzer).unwrap();
    registry
}

fn notes() -> CollectionSchema {
    CollectionSchema::from_json_in(
        r#"{"name": "notes", "fields": {"tags": {"type": "text", "analyzer": "tags"}}}"#,
        &registry(),
    )
    .unwrap()
}

fn plain() -> CollectionSchema {
    CollectionSchema::from_json(r#"{"name": "plain", "fields": {"body": {"type": "text"}}}"#)
        .unwrap()
}

/// A blob reference to a made-up blob `n` (the engine never looks one up, SPEC §9.5).
fn blob(n: u8) -> BlobRef {
    BlobRef::new(
        BlobId::from_bytes([n; 32]),
        u64::from(n) + 1,
        "application/octet-stream",
        1 << 20,
        BTreeMap::new(),
    )
    .unwrap()
}

/// Note `id` with tags and the reference to blob `n`.
fn note(id: &str, n: u8) -> Document {
    let mut doc = Document::new(id).with(
        notes().field("tags").unwrap().id(),
        Value::String(format!("Tag{n} shared")),
    );
    doc.set_blob_ref(Some(blob(n)));
    doc
}

fn plain_doc(id: &str, n: u8) -> Document {
    let mut doc = Document::new(id).with(
        plain().field("body").unwrap().id(),
        Value::String(format!("plain {id}")),
    );
    doc.set_blob_ref(Some(blob(n)));
    doc
}

fn options(registry: AnalyzerRegistry, memtable_bytes: usize) -> DatabaseOptions {
    let mut options = DatabaseOptions {
        registry,
        ..DatabaseOptions::default()
    };
    options.policy.flush.memtable_bytes = memtable_bytes;
    options
}

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

fn mark_only(
    fs: &SimFs,
    options: DatabaseOptions,
) -> Result<MarkOnlyDatabase<SimFsFile>, DatabaseError> {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadWrite)?;
    MarkOnlyDatabase::open_container(container, options)
}

fn open_app(fs: &SimFs) -> Database<SimFsFile> {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadWrite).unwrap();
    Database::open_container(container, options(registry(), 64 << 20)).unwrap()
}

/// Committed notes (one replaced, one deleted) and a committed plain document, then a WAL after the last commit with
/// notes put, replaced and deleted across chunks, and a plain document. Closed.
fn populated(fs: &SimFs) {
    let container = Container::create_in(vfs(fs), PATH).unwrap();
    let mut app = options(registry(), 64 << 20);
    app.wal_chunk_target = 256; // several WAL chunks
    let mut db = Database::create_container(container, app).unwrap();
    db.create_collection(notes()).unwrap();
    db.create_collection(plain()).unwrap();
    for (id, n) in [("n1", 1), ("n2", 2), ("n3", 3)] {
        db.put("notes", &note(id, n), Durability::Buffered).unwrap();
    }
    db.put("plain", &plain_doc("p1", 4), Durability::Buffered)
        .unwrap();
    db.commit().unwrap();
    db.put("notes", &note("n2", 5), Durability::Buffered)
        .unwrap();
    db.delete("notes", "n3", Durability::Buffered).unwrap();
    db.commit().unwrap();
    // The WAL: a new note, a replacement, a deletion, a plain document, synced.
    for (id, n) in [("w1", 10), ("w2", 11), ("w1", 12)] {
        db.put("notes", &note(id, n), Durability::Buffered).unwrap();
    }
    db.delete("notes", "n1", Durability::Buffered).unwrap();
    db.put("plain", &plain_doc("p2", 13), Durability::Fsync)
        .unwrap();
}

/// Invariant 1.
#[test]
fn its_mark_is_the_writers_and_its_open_writes_nothing() {
    let fs = SimFs::new();
    populated(&fs);
    let before = fs.files();
    let db = mark_only(&fs, options(AnalyzerRegistry::builtin(), 64 << 20)).unwrap();
    let skipped = db.skipped_analysis();
    assert_eq!(skipped.len(), 1);
    assert_eq!(
        (skipped[0].collection.as_str(), skipped[0].field.as_str()),
        ("notes", "tags")
    );
    let marked = db.referenced_blobs().unwrap();
    let seq = db.seq();
    drop(db);
    assert_eq!(fs.files(), before, "the open wrote nothing");
    // Every reference a writer's view can still show: the WAL's records of `notes` (w1 twice, w2, and n1's segment
    // version, deleted only in the WAL), the replaced and deleted segment documents' sidecars applied.
    let expected: HashSet<BlobId> = [1, 5, 4, 10, 11, 12, 13]
        .into_iter()
        .map(|n| blob(n).blob_id())
        .collect();
    let app = open_app(&fs);
    assert_eq!(app.seq(), seq);
    let writer = app.referenced_blobs().unwrap();
    assert!(writer.is_superset(&expected), "{writer:?}");
    assert_eq!(
        marked, writer,
        "the mark-only open marks what the writer marks"
    );
}

fn with_a_backlog(fs: &SimFs, collection: &str, docs: usize) {
    let container = Container::create_in(vfs(fs), PATH).unwrap();
    let mut db = Database::create_container(container, options(registry(), 1 << 30)).unwrap();
    db.create_collection(notes()).unwrap();
    db.create_collection(plain()).unwrap();
    db.put("notes", &note("committed", 1), Durability::Buffered)
        .unwrap();
    db.commit().unwrap();
    for i in 0..docs {
        let id = format!("b{i:03}");
        let doc = if collection == "notes" {
            note(&id, 2)
        } else {
            plain_doc(&id, 2)
        };
        db.put(collection, &doc, Durability::Buffered).unwrap();
    }
    let last = if collection == "notes" {
        note("last", 3)
    } else {
        plain_doc("last", 3)
    };
    db.put(collection, &last, Durability::Fsync).unwrap();
}

/// Invariant 2: a backlog of `notes` over the (small) memtable size would have to be flushed with the stand-in. The
/// open refuses, naming the collection, field and analyzer, and leaves every byte as it was.
#[test]
fn an_open_that_would_flush_a_skipped_collection_is_refused_before_writing() {
    let fs = SimFs::new();
    with_a_backlog(&fs, "notes", 64);
    let before = fs.files();
    let err = mark_only(&fs, options(AnalyzerRegistry::builtin(), 1024)).unwrap_err();
    match &err {
        DatabaseError::AnalysisSkipped {
            collection,
            field,
            analyzer,
        } => assert_eq!(
            (collection.as_str(), field.as_str(), analyzer.as_str()),
            ("notes", "tags", "tags@1")
        ),
        other => panic!("{other:?}"),
    }
    assert!(!err.is_corruption());
    assert_eq!(fs.files(), before, "nothing was written");
    // With the default memtable size nothing is due, and the backlog's references are all marked.
    let db = mark_only(&fs, options(AnalyzerRegistry::builtin(), 64 << 20)).unwrap();
    let marked = db.referenced_blobs().unwrap();
    assert!(
        [1, 2, 3]
            .iter()
            .all(|&n| marked.contains(&blob(n).blob_id())),
        "{marked:?}"
    );
    drop(db);
    assert_eq!(fs.files(), before, "nothing was written");
}

/// Invariant 2, the other side, and invariant 3: a backlog of `plain` (no skipped field) flushes as in any writer's
/// open while `notes` holds nothing unflushed; with every analyzer, a `notes` backlog flushes too.
#[test]
fn without_a_skipped_backlog_it_flushes_as_the_writers_open() {
    for (collection, registry) in [
        ("plain", AnalyzerRegistry::builtin()),
        ("notes", registry()),
    ] {
        let fs = SimFs::new();
        with_a_backlog(&fs, collection, 64);
        let seq = open_app(&fs).seq();
        let db = mark_only(&fs, options(registry, 1024)).unwrap();
        assert!(
            db.seq() > seq,
            "{collection}: the replay flushed the backlog"
        );
        assert!(db.referenced_blobs().unwrap().contains(&blob(3).blob_id()));
    }
}

/// Invariant 4.
#[test]
fn it_takes_the_writer_lock_and_needs_a_read_write_container() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut writer = Database::create(&path, options(registry(), 64 << 20)).unwrap();
    writer.create_collection(notes()).unwrap();
    let builtin = || options(AnalyzerRegistry::builtin(), 64 << 20);
    let err = MarkOnlyDatabase::open(&path, builtin()).unwrap_err();
    assert!(
        matches!(err, DatabaseError::Container(ContainerError::Locked)),
        "{err:?}"
    );
    drop(writer);
    let read_only = Container::open(&path, Mode::ReadOnly).unwrap();
    let err = MarkOnlyDatabase::open_container(read_only, builtin()).unwrap_err();
    assert!(
        matches!(
            err,
            DatabaseError::Container(ContainerError::ReadOnly { .. })
        ),
        "{err:?}"
    );
    let db = MarkOnlyDatabase::open(&path, builtin()).unwrap();
    assert_eq!(db.skipped_analysis().len(), 1);
    let canonical = std::fs::canonicalize(&path).unwrap();
    assert_eq!(
        db.container_path()
            .map(std::fs::canonicalize)
            .transpose()
            .unwrap(),
        Some(canonical)
    );
    let uuid = db.writer_proof().db_uuid();
    drop(db);
    let reopened = Container::open(&path, Mode::ReadOnly).unwrap();
    assert_eq!(
        reopened.header().db_uuid,
        uuid,
        "the proof is of this container"
    );
}

fn builtin() -> DatabaseOptions {
    options(AnalyzerRegistry::builtin(), 64 << 20)
}

fn has_cut_commit(fs: &SimFs) -> bool {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadOnly).unwrap();
    let log_end = container.superblock().log_end;
    container
        .frames()
        .iter()
        .any(|f| f.kind == FrameKind::Manifest && f.offset >= log_end)
}

/// Invariant 5: a fault at each mutating call of a commit of the application, then a crash. Where that leaves a cut
/// commit (a Manifest after `log_end`) while the WAL holds records of the skipped collection, the mark-only open
/// retires it with one commit, which flushes nothing and is not refused: those records stay WAL records (the
/// application's reopen finds them in its memtable), and their blobs are marked.
#[test]
fn a_cut_commit_is_retired_without_a_flush_and_the_skipped_records_stay_marked() {
    let calls = {
        let fs = SimFs::new();
        populated(&fs);
        let mut db = open_app(&fs);
        let start = fs.mutations();
        db.commit().unwrap();
        fs.mutations() - start
    };
    let mut cut = 0;
    for n in 0..calls {
        for loss in [CrashLoss::Nothing, CrashLoss::All] {
            let ctx = format!("call {n}, {loss:?}");
            let fs = SimFs::new();
            populated(&fs);
            let mut db = open_app(&fs);
            let seq = db.seq();
            fs.set_fault(Some(SimFault::FailAt(fs.mutations() + n)));
            assert!(db.commit().is_err(), "{ctx}: the fault fired");
            fs.set_fault(None);
            drop(db);
            let crashed = fs.crash(loss);
            if !has_cut_commit(&crashed) {
                continue;
            }
            cut += 1;
            let db = mark_only(&crashed, builtin()).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            assert_eq!(db.seq(), seq + 1, "{ctx}: one retiring commit");
            assert!(!has_cut_commit(&crashed), "{ctx}: retired");
            let marked = db.referenced_blobs().unwrap();
            for n in [10, 11, 12, 13] {
                assert!(marked.contains(&blob(n).blob_id()), "{ctx}: blob {n}");
            }
            drop(db);
            let app = open_app(&crashed);
            let snapshot = app.snapshot();
            let notes = snapshot.collection("notes").unwrap();
            assert_eq!(
                notes.memtable().live_count(),
                2,
                "{ctx}: w1 and w2 are still WAL records"
            );
            assert!(notes.get("w1").unwrap().is_some() && notes.get("w2").unwrap().is_some());
        }
    }
    assert!(cut > 0, "no fault left a cut commit");
}

/// Invariant 6: a refusal after part of the open committed. The replay flushes a `plain` backlog over the (small)
/// memtable size while `notes` holds nothing, then the `notes` records go past it and the open is refused. What the
/// flushes committed stays, a second mark-only open is refused the same way, and the application's open reaches every
/// document, and its mark every blob.
#[test]
fn a_refusal_after_a_flush_of_another_collection_loses_nothing() {
    let fs = SimFs::new();
    let container = Container::create_in(vfs(&fs), PATH).unwrap();
    let mut db = Database::create_container(container, options(registry(), 1 << 30)).unwrap();
    db.create_collection(notes()).unwrap();
    db.create_collection(plain()).unwrap();
    db.commit().unwrap();
    for i in 0..64 {
        db.put(
            "plain",
            &plain_doc(&format!("p{i:03}"), 20),
            Durability::Buffered,
        )
        .unwrap();
    }
    for i in 0..64_u8 {
        db.put(
            "notes",
            &note(&format!("n{i:03}"), 30 + i % 8),
            Durability::Buffered,
        )
        .unwrap();
    }
    db.put("notes", &note("last", 40), Durability::Fsync)
        .unwrap();
    drop(db);
    let before = open_app(&fs).seq();
    let refused = |ctx: &str| {
        let err = mark_only(&fs, options(AnalyzerRegistry::builtin(), 1024)).unwrap_err();
        assert!(
            matches!(&err, DatabaseError::AnalysisSkipped { collection, .. } if collection == "notes"),
            "{ctx}: {err:?}"
        );
    };
    refused("first open");
    let after = open_app(&fs).seq();
    assert!(
        after > before,
        "the replay committed the plain flushes: {before} -> {after}"
    );
    refused("second open");
    // Not `==`: the WAL chunk the first open was replaying when it flushed stays from `wal_start` on (SPEC §8.6), so the
    // second open replays its `plain` records again and flushes them again before it is refused (the same documents,
    // in new segments). Nothing goes back and nothing is lost (below).
    let again = open_app(&fs).seq();
    assert!(again >= after, "{after} -> {again}");

    let app = open_app(&fs);
    let snapshot = app.snapshot();
    let plain_c = snapshot.collection("plain").unwrap();
    let notes_c = snapshot.collection("notes").unwrap();
    for i in 0..64 {
        assert!(
            plain_c.get(&format!("p{i:03}")).unwrap().is_some(),
            "p{i:03}"
        );
        assert!(
            notes_c.get(&format!("n{i:03}")).unwrap().is_some(),
            "n{i:03}"
        );
    }
    assert!(notes_c.get("last").unwrap().is_some());
    let marked = app.referenced_blobs().unwrap();
    for n in (30..38).chain([20, 40]) {
        assert!(marked.contains(&blob(n).blob_id()), "blob {n}");
    }
}
