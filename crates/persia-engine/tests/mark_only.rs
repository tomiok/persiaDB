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
//! 7. A fault and a crash at every write of its open (the replay-time flushes of a collection without a skipped field,
//!    or the commit retiring a cut commit) lose nothing: the skipped records stay WAL records, and the next mark-only
//!    open succeeds and marks exactly what the writer marks, every live blob included.
//! 8. Repeated refusals (each re-flushing the replayed `plain` records of the chunk `wal_start` stayed at) lose and
//!    resurrect nothing and write no `notes` segment; the application's commit, merges and compaction then leave the
//!    same documents, the WAL `notes` indexed by the real analyzer, and no dead byte. The growth per refusal is
//!    printed (with `--nocapture`).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err,
    missing_docs
)]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use persia_analysis::{Analyzer, AnalyzerRegistry, LowercaseFilter, WhitespaceTokenizer};
use persia_engine::db::{
    Database, DatabaseError, DatabaseOptions, MarkOnlyDatabase, ReadOnlyDatabase, SearchRequest,
    Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::Memtable;
use persia_engine::merge::MergeOptions;
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::schema::CollectionSchema;
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::view::SearchView;
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

// ---------------------------------------------------------------------------------------------------------------
// Integration scenarios (ROADMAP 10.4.7): a crash at every write of the open, checked by the mark; repeated refusals.
// ---------------------------------------------------------------------------------------------------------------

/// The live documents of a state: `(collection, id)` to the blob each references.
type Docs = BTreeMap<(String, String), BlobId>;

/// Every live document of `notes` and `plain` in `snapshot`, each confirmed by a lookup (which applies tombstones and
/// replacements), with its blob.
fn live_docs(snapshot: &Snapshot) -> Docs {
    let mut out = Docs::new();
    for name in ["notes", "plain"] {
        let c = snapshot.collection(name).unwrap();
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
        for id in ids {
            if let Some(doc) = c.get(&id).unwrap() {
                let blob = doc.blob_ref().unwrap().blob_id();
                out.insert((name.to_owned(), id), blob);
            }
        }
    }
    out
}

/// A read-only open with the application's analyzers: writes nothing, so it observes a state without changing it.
fn read_only(fs: &SimFs) -> ReadOnlyDatabase<SimFsFile> {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadOnly).unwrap();
    ReadOnlyDatabase::open_container(container, &registry().with_builtins()).unwrap()
}

/// The ids `text` finds in `collection` with the application's analyzers, sorted (a page of up to 1000 hits).
fn search(snapshot: &Snapshot, collection: &str, text: &str) -> Vec<String> {
    let registry = registry().with_builtins();
    let c = snapshot.collection(collection).unwrap();
    let compiler = QueryCompiler::new(c.schema(), &registry).unwrap();
    let query = compiler
        .compile(
            text,
            &DefaultFields::all(c.schema()),
            &QueryLimits::default(),
        )
        .into_query();
    let memtable = c.memtable();
    let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
    let page = c
        .views(&memtable)
        .unwrap()
        .search(
            &plan,
            &SearchRequest {
                limit: 1000,
                ..SearchRequest::default()
            },
            Pruning::default(),
        )
        .unwrap();
    let mut ids: Vec<_> = page
        .hits
        .iter()
        .map(|h| h.document.id().to_owned())
        .collect();
    ids.sort();
    ids
}

/// The heap size of a `plain` memtable holding `docs` documents: a measured flush threshold, so the replay flushes
/// `plain` about every `docs` records whatever the memtable's overheads.
fn plain_threshold(docs: usize) -> usize {
    let full = registry().with_builtins();
    let mut memtable = Memtable::new(&plain(), &full).unwrap();
    for i in 0..docs {
        memtable.put(&plain_doc(&format!("m{i:03}"), 20)).unwrap();
    }
    memtable.size_in_bytes()
}

/// Records `doc` put in `collection` in `expected`.
fn expect_put(expected: &mut Docs, collection: &str, doc: &Document) {
    let blob = doc.blob_ref().unwrap().blob_id();
    expected.insert((collection.to_owned(), doc.id().to_owned()), blob);
}

/// A closed database whose WAL, after the last commit, holds: a `plain` block (puts, replacements of WAL and committed
/// documents, deletions of both), then `notes` puts and the deletion of the committed note, then a second `plain`
/// block over the flush threshold. A mark-only open with that threshold flushes `plain` during the first block (no
/// `notes` record yet) and is refused in the second (the `notes` records are replayed). The application's memtable
/// was 1 GiB: it flushed none of them. Returns the live documents.
fn refusal_base(fs: &SimFs, chunk_target: usize, refuse: bool) -> Docs {
    let container = Container::create_in(vfs(fs), PATH).unwrap();
    let mut app = options(registry(), 1 << 30);
    app.wal_chunk_target = chunk_target;
    let mut db = Database::create_container(container, app).unwrap();
    db.create_collection(notes()).unwrap();
    db.create_collection(plain()).unwrap();
    let mut expected = Docs::new();
    let mut put = |db: &mut Database<SimFsFile>, collection: &str, doc: Document| {
        db.put(collection, &doc, Durability::Buffered).unwrap();
        expect_put(&mut expected, collection, &doc);
    };
    for i in 0..16 {
        put(&mut db, "plain", plain_doc(&format!("c{i:03}"), 50));
    }
    put(&mut db, "notes", note("committed", 1));
    db.commit().unwrap();
    for i in 0..32_u8 {
        put(&mut db, "plain", plain_doc(&format!("p{i:03}"), 20 + i % 4));
    }
    for i in 0..8 {
        put(&mut db, "plain", plain_doc(&format!("p{i:03}"), 60));
    }
    for i in 8..12 {
        put(&mut db, "plain", plain_doc(&format!("c{i:03}"), 61));
    }
    let mut gone = Vec::new();
    for id in (24..32)
        .map(|i| format!("p{i:03}"))
        .chain((0..8).map(|i| format!("c{i:03}")))
    {
        db.delete("plain", &id, Durability::Buffered).unwrap();
        gone.push(("plain".to_owned(), id));
    }
    for i in 0..16_u8 {
        put(&mut db, "notes", note(&format!("n{i:03}"), 30 + i % 8));
    }
    db.delete("notes", "committed", Durability::Buffered)
        .unwrap();
    gone.push(("notes".to_owned(), "committed".to_owned()));
    if refuse {
        for i in 0..32 {
            put(&mut db, "plain", plain_doc(&format!("q{i:03}"), 70));
        }
    }
    // The last record syncs the rest: in `plain` over the threshold (refused), else in `notes` (no flush is due).
    let (collection, last) = if refuse {
        ("plain", plain_doc("last", 71))
    } else {
        ("notes", note("last", 72))
    };
    db.put(collection, &last, Durability::Fsync).unwrap();
    drop(db);
    for key in gone {
        expected.remove(&key);
    }
    expect_put(&mut expected, collection, &last);
    expected
}

/// The number of `collection`'s segments and the documents they hold (deleted ones included) in `manifest`.
fn segment_counts(manifest: &persia_engine::manifest::Manifest, collection: &str) -> (usize, u64) {
    let entry = manifest.collection(collection).unwrap();
    let docs = entry
        .segments()
        .iter()
        .map(|s| u64::from(s.doc_count))
        .sum();
    (entry.segments().len(), docs)
}

/// The container file's length.
fn file_len(fs: &SimFs) -> u64 {
    fs.read(std::path::Path::new(PATH)).unwrap().len() as u64
}

/// The application commits, then merges until its policy plans none and compacts: what is left of the segments the
/// refusals wrote. Returns `(plain segments, plain segment documents, container bytes)` after that.
fn commit_merge_compact(fs: &SimFs, expected: &Docs, ctx: &str) -> (usize, u64, u64) {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadWrite).unwrap();
    let mut app = options(registry(), 64 << 20);
    app.merge = MergeOptions {
        threads: 0,
        ..MergeOptions::default()
    };
    let mut db = Database::open_container(container, app).unwrap();
    let seq = db.seq();
    assert!(
        db.commit().unwrap() > seq,
        "{ctx}: the application flushed the WAL"
    );
    assert_eq!(
        live_docs(&db.snapshot()),
        *expected,
        "{ctx}: after the commit"
    );
    db.finish_merges().unwrap();
    assert_eq!(
        live_docs(&db.snapshot()),
        *expected,
        "{ctx}: after the merges"
    );
    let done = db.compact().unwrap();
    assert_eq!(
        done.after.dead(),
        0,
        "{ctx}: the compaction left dead bytes"
    );
    let snapshot = db.snapshot();
    assert_eq!(
        live_docs(&snapshot),
        *expected,
        "{ctx}: after the compaction"
    );
    // The real analyzer indexed the `notes` records: their `tags` terms find them, and only them.
    let notes: Vec<String> = (0..16).map(|i| format!("n{i:03}")).collect();
    assert_eq!(search(&snapshot, "notes", "shared"), notes, "{ctx}");
    let (segments, docs) = segment_counts(db.manifest(), "plain");
    drop(db);
    (segments, docs, file_len(fs))
}

/// Candidate 3 of the 10.4.7 integration plan: `REFUSALS` refused mark-only opens in a row. Each replays the `plain`
/// block again from the chunk `wal_start` stayed at and flushes it again into new segments before the `notes` records
/// make the next flush refused (SPEC §8.6: harmless, the same documents). After every refusal a read-only open shows
/// exactly the documents the application wrote (none lost, no deleted or replaced one back), the `notes` records are
/// still WAL records and the `notes` segment is untouched. Then the application commits, merges and compacts: the
/// documents are the same, the real analyzer indexed the `notes` records, no dead byte is left, and the merged `plain`
/// segments hold at most a quarter more documents than are live (the policy's 20% deletion rewrite). A second
/// database, the same writes with no refusal, gives the baseline the growth is printed against.
#[test]
fn repeated_refusals_lose_and_resurrect_nothing_and_their_segments_are_reclaimable() {
    const REFUSALS: usize = 6;
    let threshold = plain_threshold(8);
    for chunk_target in [DatabaseOptions::default().wal_chunk_target, 512] {
        let ctx = format!("chunk target {chunk_target}");
        let baseline = SimFs::new();
        let expected = refusal_base(&baseline, chunk_target, true);
        let reclaimed_baseline =
            commit_merge_compact(&baseline, &expected, &format!("{ctx}, baseline"));

        let fs = SimFs::new();
        assert_eq!(refusal_base(&fs, chunk_target, true), expected);
        let notes_segments: Vec<_> = {
            let ro = read_only(&fs);
            ro.manifest()
                .collection("notes")
                .unwrap()
                .segments()
                .iter()
                .map(|s| s.id)
                .collect()
        };
        let mut rows = vec![(
            read_only(&fs).seq(),
            segment_counts(&read_only(&fs).manifest(), "plain"),
            file_len(&fs),
        )];
        for refusal in 1..=REFUSALS {
            let ctx = format!("{ctx}, refusal {refusal}");
            let err = mark_only(&fs, options(AnalyzerRegistry::builtin(), threshold)).unwrap_err();
            assert!(
                matches!(&err, DatabaseError::AnalysisSkipped { collection, .. } if collection == "notes"),
                "{ctx}: {err:?}"
            );
            let ro = read_only(&fs);
            let snapshot = ro.snapshot();
            assert_eq!(live_docs(&snapshot), expected, "{ctx}");
            let notes = snapshot.collection("notes").unwrap();
            assert_eq!(
                notes.memtable().live_count(),
                16,
                "{ctx}: the notes records are WAL records"
            );
            let ids: Vec<_> = ro
                .manifest()
                .collection("notes")
                .unwrap()
                .segments()
                .iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(ids, notes_segments, "{ctx}: no notes segment was written");
            rows.push((
                ro.seq(),
                segment_counts(&ro.manifest(), "plain"),
                file_len(&fs),
            ));
        }
        for (n, pair) in rows.windows(2).enumerate() {
            let ((seq_before, segments_before, len_before), (seq_after, segments_after, len_after)) =
                (pair[0], pair[1]);
            assert!(
                seq_after >= seq_before && len_after >= len_before,
                "{ctx}: nothing goes back"
            );
            eprintln!(
                "{ctx}: refusal {}: +{} commits, plain (segments, segment documents) {segments_before:?} -> \
                 {segments_after:?}, +{} bytes",
                n + 1,
                seq_after - seq_before,
                len_after - len_before
            );
        }
        let (segments, docs, len) = commit_merge_compact(&fs, &expected, &ctx);
        let live_plain = expected.keys().filter(|(c, _)| c == "plain").count() as u64;
        eprintln!(
            "{ctx}: after {REFUSALS} refusals {} bytes; the application's commit, merges and compaction: {segments} plain \
             segments of {docs} documents ({live_plain} live), {len} bytes; without refusals {} segments of {} \
             documents, {} bytes",
            rows.last().unwrap().2,
            reclaimed_baseline.0,
            reclaimed_baseline.1,
            reclaimed_baseline.2
        );
        assert!(
            4 * docs <= 5 * live_plain,
            "{ctx}: the merges left {docs} segment documents for {live_plain} live"
        );
        assert!(
            len < rows.last().unwrap().2,
            "{ctx}: the compaction reclaimed the refusals' segments"
        );
        // The mark-only open now opens (nothing of `notes` is left to flush) and marks what the application marks.
        let db = mark_only(&fs, options(AnalyzerRegistry::builtin(), threshold)).unwrap();
        let marked = db.referenced_blobs().unwrap();
        drop(db);
        assert_eq!(marked, open_app(&fs).referenced_blobs().unwrap(), "{ctx}");
        assert!(expected.values().all(|b| marked.contains(b)), "{ctx}");
    }
}

/// A flush threshold `notes`' memtable stays under with [`refusal_base`]'s `notes` records (measured with the real
/// analyzer, which indexes more than the stand-in) and that its first `plain` block crosses more than once.
fn flushing_threshold() -> usize {
    let full = registry().with_builtins();
    let mut memtable = Memtable::new(&notes(), &full).unwrap();
    for i in 0..16_u8 {
        memtable
            .put(&note(&format!("n{i:03}"), 30 + i % 8))
            .unwrap();
    }
    memtable.delete("committed").unwrap();
    memtable.put(&note("last", 72)).unwrap();
    let threshold = (memtable.size_in_bytes() + 1).max(plain_threshold(8));
    assert!(
        threshold < plain_threshold(30),
        "the plain block flushes at least once"
    );
    threshold
}

/// A fault at each mutating call of a mark-only open of a copy of `base` (a failed write, or one applied then
/// reported failed), then a crash with each loss. After it: a read-only open shows exactly `expected`, the `notes`
/// records still in its memtable (`notes_in_wal`); a mark-only open succeeds and marks exactly what the application's
/// open then marks, every blob of `expected` included. Returns the number of mutating calls of the open.
fn sweep_the_mark_only_open(
    base: &SimFs,
    options: impl Fn() -> DatabaseOptions,
    expected: &Docs,
    notes_in_wal: u32,
) -> u64 {
    let calls = {
        let copy = base.crash(CrashLoss::Nothing);
        let start = copy.mutations();
        drop(mark_only(&copy, options()).unwrap());
        copy.mutations() - start
    };
    for n in 0..calls {
        for apply_first in [false, true] {
            let ctx = format!("call {n} of {calls}, apply_first {apply_first}");
            let copy = base.crash(CrashLoss::Nothing);
            let at = copy.mutations() + n;
            copy.set_fault(Some(if apply_first {
                SimFault::ApplyThenFail(at)
            } else {
                SimFault::FailAt(at)
            }));
            let opened = mark_only(&copy, options());
            copy.set_fault(None);
            assert!(opened.is_err(), "{ctx}: the fault fired");
            drop(opened);
            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(n)] {
                let ctx = format!("{ctx}, {loss:?}");
                let crashed = copy.crash(loss);
                let ro = read_only(&crashed);
                let snapshot = ro.snapshot();
                assert_eq!(live_docs(&snapshot), *expected, "{ctx}: the documents");
                assert_eq!(
                    snapshot
                        .collection("notes")
                        .unwrap()
                        .memtable()
                        .live_count(),
                    notes_in_wal,
                    "{ctx}: the notes records are still WAL records"
                );
                drop((snapshot, ro));
                let db = mark_only(&crashed, options()).unwrap_or_else(|e| panic!("{ctx}: {e}"));
                let marked = db.referenced_blobs().unwrap();
                drop(db);
                let app = open_app(&crashed);
                assert_eq!(
                    live_docs(&app.snapshot()),
                    *expected,
                    "{ctx}: the application's open"
                );
                assert_eq!(
                    marked,
                    app.referenced_blobs().unwrap(),
                    "{ctx}: the mark is the writer's"
                );
                assert!(
                    expected.values().all(|b| marked.contains(b)),
                    "{ctx}: a live blob is unmarked"
                );
            }
        }
    }
    calls
}

/// Candidate 4 of the 10.4.7 integration plan, in the simulated file system: a crash at every write of a mark-only
/// open whose replay flushes `plain` (twice or more) while `notes` holds nothing, then replays the `notes` records
/// without flushing them. Nothing is lost, the `notes` records stay WAL records, and the next mark-only open marks
/// exactly what the writer marks.
#[test]
fn a_crash_at_any_write_of_its_replay_time_flushes_keeps_the_mark_the_writers() {
    let base = SimFs::new();
    let expected = refusal_base(&base, 512, false);
    let threshold = flushing_threshold();
    let seq = read_only(&base).seq();
    let flushed = {
        let copy = base.crash(CrashLoss::Nothing);
        drop(mark_only(&copy, options(AnalyzerRegistry::builtin(), threshold)).unwrap());
        read_only(&copy).seq()
    };
    assert!(
        flushed >= seq + 2,
        "the replay flushed plain at least twice: {seq} -> {flushed}"
    );
    let calls = sweep_the_mark_only_open(
        &base,
        || options(AnalyzerRegistry::builtin(), threshold),
        &expected,
        17,
    );
    eprintln!(
        "{calls} mutating calls in the open, {} commits",
        flushed - seq
    );
    assert!(calls > 3, "{calls}");
}

/// Candidate 4, the retiring commit: an application commit cut before its superblock slot (the first fault point
/// that leaves a Manifest after `log_end`, then a crash that loses nothing), then a crash at every write of the
/// mark-only open that retires it (one commit, which flushes nothing).
#[test]
fn a_crash_at_any_write_of_its_retiring_commit_keeps_the_mark_the_writers() {
    let (base, expected) = (0..)
        .find_map(|n| {
            let fs = SimFs::new();
            let expected = refusal_base(&fs, 512, false);
            let mut db = open_app(&fs);
            fs.set_fault(Some(SimFault::FailAt(fs.mutations() + n)));
            let committed = db.commit();
            fs.set_fault(None);
            drop(db);
            assert!(
                committed.is_err(),
                "no fault point of the commit leaves a cut commit"
            );
            let crashed = fs.crash(CrashLoss::Nothing);
            has_cut_commit(&crashed).then_some((crashed, expected))
        })
        .unwrap();
    let seq = read_only(&base).seq();
    let calls = sweep_the_mark_only_open(&base, builtin, &expected, 17);
    let copy = base.crash(CrashLoss::Nothing);
    drop(mark_only(&copy, builtin()).unwrap());
    assert_eq!(read_only(&copy).seq(), seq + 1, "one retiring commit");
    assert!(!has_cut_commit(&copy));
    eprintln!("{calls} mutating calls in the retiring open");
    assert!(calls > 0, "{calls}");
}
