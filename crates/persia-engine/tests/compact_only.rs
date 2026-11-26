//! The compaction-only open (ROADMAP 10.4.4; SPEC §4.6, §8.6; ADR-0068): a database whose schema names a registered
//! analyzer, compacted by a handle that lacks it.
//!
//! Invariants:
//! 1. It compacts: dead frames are reclaimed, and the database reopened with the real analyzer holds every document,
//!    the committed ones and the WAL's, and finds them by the registered field's terms (the WAL records were never
//!    flushed with the stand-in, so the real analyzer indexes them at that reopen). A deletion in the WAL stays one.
//! 2. It refuses, before writing anything, an open whose replay would flush a memtable of a collection whose analysis
//!    it skips (a backlog over the memtable size), and lets a collection without a skipped field flush as a writer's
//!    open does.
//! 3. Without a skipped field it is the writer's open: a backlog over the memtable size is flushed.
//! 4. It takes the writer lock: a live writer makes it fail with `Locked`; a read-only container is refused.
//! 5. A crash or a failed write at any point of its compaction leaves the old or the new database, with every
//!    acknowledged write, found by the real analyzer.
//! 6. A cut commit (a Manifest after `log_end`) is retired by its open, flushing nothing, as a writer's open does.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::indexing_slicing,
    clippy::assert_is_empty,
    clippy::result_large_err,
    missing_docs
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_analysis::{Analyzer, AnalyzerRegistry, LowercaseFilter, WhitespaceTokenizer};
use persia_engine::db::{
    CompactOnlyDatabase, CompactionOptions, Database, DatabaseError, DatabaseOptions, ManualClock,
    SearchRequest, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::Memtable;
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::schema::CollectionSchema;
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::wal::Durability;
use persia_format::frame::FrameKind;
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

/// `notes` names the registered analyzer; `plain` the built-ins only.
fn notes() -> CollectionSchema {
    CollectionSchema::from_json_in(
        r#"{"name": "notes", "fields": {"tags": {"type": "text", "analyzer": "tags"}, "body": {"type": "text"}}}"#,
        &registry(),
    )
    .unwrap()
}

fn plain() -> CollectionSchema {
    CollectionSchema::from_json(r#"{"name": "plain", "fields": {"body": {"type": "text"}}}"#)
        .unwrap()
}

fn note(id: &str, tags: &str) -> Document {
    let schema = notes();
    Document::new(id)
        .with(
            schema.field("tags").unwrap().id(),
            Value::String(tags.into()),
        )
        .with(
            schema.field("body").unwrap().id(),
            Value::String(format!("body of {id}")),
        )
}

fn plain_doc(id: &str) -> Document {
    Document::new(id).with(
        plain().field("body").unwrap().id(),
        Value::String(format!("plain {id}")),
    )
}

/// Inline merges of two segments, no automatic compaction, a seeded clock and ids; `registry` and `memtable_bytes`.
fn options(registry: AnalyzerRegistry, memtable_bytes: usize) -> DatabaseOptions {
    let mut options = DatabaseOptions {
        registry,
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(7),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        compaction: CompactionOptions {
            automatic: false,
            dead_space_percent: 40,
            min_dead_bytes: 0,
        },
        ..DatabaseOptions::default()
    };
    options.policy.flush.memtable_bytes = memtable_bytes;
    options
}

/// The application's options: the registered analyzer, the default memtable size.
fn app() -> DatabaseOptions {
    options(registry(), 64 << 20)
}

/// The command line's: built-ins only.
fn builtin() -> DatabaseOptions {
    options(AnalyzerRegistry::builtin(), 64 << 20)
}

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

fn open_app(fs: &SimFs) -> Result<Database<SimFsFile>, DatabaseError> {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadWrite)?;
    Database::open_container(container, app())
}

fn compact_only(
    fs: &SimFs,
    options: DatabaseOptions,
) -> Result<CompactOnlyDatabase<SimFsFile>, DatabaseError> {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadWrite)?;
    CompactOnlyDatabase::open_container(container, options)
}

/// What the database holds: each collection's live ids with the tags (notes) or body (plain) they carry.
type Expected = BTreeMap<(&'static str, String), String>;

/// A database with dead frames (rounds of replaced notes, each committed, merged), a committed `plain` document, and
/// a WAL after the last commit: a new note, a replaced one, a deleted one and a `plain` document. Closed.
fn populated(fs: &SimFs) -> (Expected, Vec<String>) {
    let container = Container::create_in(vfs(fs), PATH).unwrap();
    let mut db = Database::create_container(container, app()).unwrap();
    db.create_collection(notes()).unwrap();
    db.create_collection(plain()).unwrap();
    let mut expected = Expected::new();
    for round in 0..4 {
        for i in 0..4 {
            let tags = format!("Round{round} item{i} committed");
            db.put(
                "notes",
                &note(&format!("n{i}"), &tags),
                Durability::Buffered,
            )
            .unwrap();
            expected.insert(("notes", format!("n{i}")), tags);
        }
        db.commit().unwrap();
        db.merge_step().unwrap();
    }
    db.put("plain", &plain_doc("p0"), Durability::Buffered)
        .unwrap();
    expected.insert(("plain", "p0".into()), "plain p0".into());
    db.commit().unwrap();
    db.finish_merges().unwrap();
    // The WAL after the last commit, synced.
    db.put("notes", &note("w0", "Walonly fresh"), Durability::Buffered)
        .unwrap();
    expected.insert(("notes", "w0".into()), "Walonly fresh".into());
    db.put(
        "notes",
        &note("n0", "Walonly replaced"),
        Durability::Buffered,
    )
    .unwrap();
    expected.insert(("notes", "n0".into()), "Walonly replaced".into());
    db.delete("notes", "n1", Durability::Buffered).unwrap();
    expected.remove(&("notes", "n1".into()));
    db.put("plain", &plain_doc("p1"), Durability::Fsync)
        .unwrap();
    expected.insert(("plain", "p1".into()), "plain p1".into());
    drop(db);
    (expected, vec!["n1".to_owned()])
}

/// The ids `text` finds in `collection`, analyzed with `registry`, sorted (a page of up to 1000 hits).
fn search(
    snapshot: &Snapshot,
    collection: &str,
    registry: &AnalyzerRegistry,
    text: &str,
) -> Vec<String> {
    let c = snapshot.collection(collection).unwrap();
    let compiler = QueryCompiler::new(c.schema(), registry).unwrap();
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

/// The database `db` holds exactly `expected`, read by id, `gone` is not live, and the registered field's terms find
/// both the committed notes and the WAL's: the latter were indexed by the real analyzer.
fn assert_holds(db: &Database<SimFsFile>, expected: &Expected, gone: &[String], ctx: &str) {
    let snapshot = db.snapshot();
    let full = registry().with_builtins();
    for ((collection, id), text) in expected {
        let c = snapshot.collection(collection).unwrap();
        let doc = c
            .get(id)
            .unwrap()
            .unwrap_or_else(|| panic!("{ctx}: {collection}/{id} is missing"));
        let field = if *collection == "notes" {
            "tags"
        } else {
            "body"
        };
        let value = doc.get(c.schema().field(field).unwrap().id());
        assert_eq!(
            value,
            Some(&Value::String(text.clone())),
            "{ctx}: {collection}/{id}"
        );
    }
    let notes = snapshot.collection("notes").unwrap();
    for id in gone {
        assert!(notes.get(id).unwrap().is_none(), "{ctx}: {id} came back");
    }
    assert_eq!(
        search(&snapshot, "notes", &full, "walonly"),
        ["n0", "w0"],
        "{ctx}"
    );
    assert_eq!(
        search(&snapshot, "notes", &full, "committed"),
        ["n2", "n3"],
        "{ctx}"
    );
    assert_eq!(
        search(&snapshot, "plain", &full, "plain"),
        ["p0", "p1"],
        "{ctx}"
    );
}

/// Invariant 1.
#[test]
fn a_handle_without_the_registered_analyzer_compacts_and_keeps_every_document() {
    let fs = SimFs::new();
    let (expected, gone) = populated(&fs);
    let before_seq = open_app(&fs).unwrap().seq();
    let mut db = compact_only(&fs, builtin()).unwrap();
    let skipped = db.skipped_analysis();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(
        (
            skipped[0].collection.as_str(),
            skipped[0].field.as_str(),
            skipped[0].analyzer.as_str()
        ),
        ("notes", "tags", "tags@1")
    );
    // Its open committed nothing: the WAL backlog is under the memtable size.
    assert_eq!(db.seq(), before_seq);
    let done = db.compact().unwrap();
    assert!(done.after.total < done.before.total, "{done:?}");
    assert_eq!(done.after.dead(), 0, "{done:?}");
    assert_eq!(done.seq, before_seq + 1);
    assert_eq!(db.seq(), done.seq);
    assert_eq!(db.space_usage().unwrap(), done.after);
    drop(db);
    let reopened = open_app(&fs).unwrap();
    assert_eq!(
        reopened.seq(),
        done.seq,
        "the reopen flushed nothing either"
    );
    assert_holds(&reopened, &expected, &gone, "after the compaction");
    // And again on the crashed file system: the compaction was durable.
    drop(reopened);
    let crashed = fs.crash(CrashLoss::All);
    assert_holds(
        &open_app(&crashed).unwrap(),
        &expected,
        &gone,
        "after a crash",
    );
}

/// Invariant 1 without a WAL backlog: nothing after the last commit.
#[test]
fn a_database_without_a_wal_backlog_compacts_too() {
    let fs = SimFs::new();
    let (expected, gone) = populated(&fs);
    // The application commits the backlog: the WAL from `wal_start` holds no record.
    let mut app_db = open_app(&fs).unwrap();
    app_db.commit().unwrap();
    let seq = app_db.seq();
    drop(app_db);
    let mut db = compact_only(&fs, builtin()).unwrap();
    assert_eq!(db.seq(), seq);
    let done = db.compact().unwrap();
    assert_eq!(done.after.dead(), 0);
    drop(db);
    assert_holds(&open_app(&fs).unwrap(), &expected, &gone, "no backlog");
}

/// A closed database with `notes` and `plain`, one committed note (`committed`), then a WAL backlog of `docs` documents
/// of `collection` (`notes` or `plain`) and one more of it synced (`last`): the application had a 1 GiB memtable, so it
/// flushed none of them.
fn with_a_backlog(fs: &SimFs, collection: &str, docs: usize) {
    let container = Container::create_in(vfs(fs), PATH).unwrap();
    let mut db = Database::create_container(container, options(registry(), 1 << 30)).unwrap();
    db.create_collection(notes()).unwrap();
    db.create_collection(plain()).unwrap();
    db.put("notes", &note("committed", "kept"), Durability::Buffered)
        .unwrap();
    db.commit().unwrap();
    for i in 0..docs {
        let doc = if collection == "notes" {
            note(&format!("b{i:03}"), "backlog tag")
        } else {
            plain_doc(&format!("b{i:03}"))
        };
        db.put(collection, &doc, Durability::Buffered).unwrap();
    }
    db.put(
        collection,
        &if collection == "notes" {
            note("last", "synced")
        } else {
            plain_doc("last")
        },
        Durability::Fsync,
    )
    .unwrap();
}

/// Invariant 2: a backlog of `notes` over the (small) memtable size would have to be flushed with the stand-in. The
/// open refuses, naming the collection, field and analyzer, and leaves every byte of the file as it was.
#[test]
fn an_open_that_would_flush_a_skipped_collection_is_refused_before_writing() {
    let fs = SimFs::new();
    with_a_backlog(&fs, "notes", 64);
    let before = fs.files();
    let err = compact_only(&fs, options(AnalyzerRegistry::builtin(), 1024)).unwrap_err();
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
    assert!(err.to_string().contains("tags@1"), "{err}");
    assert_eq!(fs.files(), before, "nothing was written");
    // The application, with the analyzer, flushes the backlog itself; then the command line compacts.
    let reopened = Container::open_in(vfs(&fs), PATH, Mode::ReadWrite).unwrap();
    let mut db = Database::open_container(reopened, options(registry(), 1024)).unwrap();
    db.commit().unwrap();
    drop(db);
    let mut db = compact_only(&fs, options(AnalyzerRegistry::builtin(), 1024)).unwrap();
    db.compact().unwrap();
    drop(db);
    let db = open_app(&fs).unwrap();
    let full = registry().with_builtins();
    assert_eq!(search(&db.snapshot(), "notes", &full, "backlog").len(), 64);
    assert_eq!(search(&db.snapshot(), "notes", &full, "synced"), ["last"]);
}

/// Invariant 2, the other side: a backlog of `plain` (no skipped field) flushes as in any writer's open while `notes`
/// has nothing unflushed, and the compaction then runs.
#[test]
fn a_collection_without_a_skipped_field_flushes_its_backlog_as_a_writer_does() {
    let fs = SimFs::new();
    with_a_backlog(&fs, "plain", 64);
    let seq = open_app(&fs).map(|db| db.seq()).unwrap();
    // (`open_app` replays with a 64 MiB memtable and does not flush.)
    let mut db = compact_only(&fs, options(AnalyzerRegistry::builtin(), 1024)).unwrap();
    assert!(
        db.seq() > seq,
        "the replay flushed the plain backlog: {} after {seq}",
        db.seq()
    );
    db.compact().unwrap();
    drop(db);
    let db = open_app(&fs).unwrap();
    let full = registry().with_builtins();
    let snapshot = db.snapshot();
    assert_eq!(search(&snapshot, "plain", &full, "plain").len(), 65);
    assert_eq!(search(&snapshot, "notes", &full, "kept"), ["committed"]);
}

/// Invariant 3: with every analyzer, nothing is skipped and nothing refused; a backlog over the memtable size is
/// flushed, as `Database::open` flushes it.
#[test]
fn with_every_analyzer_it_is_the_writers_open() {
    let fs = SimFs::new();
    with_a_backlog(&fs, "notes", 64);
    let seq = open_app(&fs).map(|db| db.seq()).unwrap();
    let mut db = compact_only(&fs, options(registry(), 1024)).unwrap();
    assert!(db.skipped_analysis().is_empty());
    assert!(db.seq() > seq);
    db.compact().unwrap();
    drop(db);
    let db = open_app(&fs).unwrap();
    let full = registry().with_builtins();
    assert_eq!(search(&db.snapshot(), "notes", &full, "backlog").len(), 64);
}

/// Invariant 4.
#[test]
fn it_takes_the_writer_lock_and_needs_a_read_write_container() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut writer = Database::create(&path, app()).unwrap();
    writer.create_collection(notes()).unwrap();
    let err = CompactOnlyDatabase::open(&path, builtin()).unwrap_err();
    assert!(
        matches!(err, DatabaseError::Container(ContainerError::Locked)),
        "{err:?}"
    );
    drop(writer);
    let read_only = Container::open(&path, Mode::ReadOnly).unwrap();
    let err = CompactOnlyDatabase::open_container(read_only, builtin()).unwrap_err();
    assert!(
        matches!(
            err,
            DatabaseError::Container(ContainerError::ReadOnly { .. })
        ),
        "{err:?}"
    );
    // On the real file system, as `persia compact` runs it.
    let mut db = CompactOnlyDatabase::open(&path, builtin()).unwrap();
    assert_eq!(db.skipped_analysis().len(), 1);
    db.compact().unwrap();
}

/// Invariant 5: a fault at each mutating call of the compaction (a failed write, or one applied then reported
/// failed), then a crash: the database reopens, with the real analyzer, holding every acknowledged document.
#[test]
fn a_crash_or_failure_at_any_write_of_its_compaction_keeps_every_document() {
    let calls = {
        let fs = SimFs::new();
        populated(&fs);
        let mut db = compact_only(&fs, builtin()).unwrap();
        let start = fs.mutations();
        db.compact().unwrap();
        fs.mutations() - start
    };
    assert!(calls > 5, "{calls}");
    for n in 0..calls {
        for apply_first in [false, true] {
            let ctx = format!("call {n}, apply_first {apply_first}");
            let fs = SimFs::new();
            let (expected, gone) = populated(&fs);
            let mut db = compact_only(&fs, builtin()).unwrap();
            let at = fs.mutations() + n;
            fs.set_fault(Some(if apply_first {
                SimFault::ApplyThenFail(at)
            } else {
                SimFault::FailAt(at)
            }));
            let result = db.compact();
            fs.set_fault(None);
            assert!(result.is_err(), "{ctx}: the fault fired");
            drop(db);
            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(n)] {
                let crashed = fs.crash(loss);
                let db = open_app(&crashed).unwrap_or_else(|e| panic!("{ctx}, {loss:?}: {e}"));
                assert_holds(&db, &expected, &gone, &format!("{ctx}, {loss:?}"));
            }
        }
    }
}

/// Whether the container at `PATH` on `fs` has a Manifest frame after `log_end`: a commit cut before its superblock
/// slot (SPEC §4.5).
fn has_cut_commit(fs: &SimFs) -> bool {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadOnly).unwrap();
    let log_end = container.superblock().log_end;
    container
        .frames()
        .iter()
        .any(|f| f.kind == FrameKind::Manifest && f.offset >= log_end)
}

/// Invariant 6: a fault at each mutating call of a commit of the application, then a crash. Where that leaves a cut
/// commit (a Manifest after `log_end`), the compaction-only open retires it with exactly one commit, which flushes
/// nothing (the skipped collection's WAL records stay WAL records); and in every case it compacts, and the
/// application's reopen holds every acknowledged document.
#[test]
fn a_cut_commit_is_retired_by_its_open_without_a_flush() {
    let calls = {
        let fs = SimFs::new();
        populated(&fs);
        let mut db = open_app(&fs).unwrap();
        let start = fs.mutations();
        db.commit().unwrap();
        fs.mutations() - start
    };
    let mut cut = 0;
    for n in 0..calls {
        for loss in [CrashLoss::Nothing, CrashLoss::All] {
            let ctx = format!("call {n}, {loss:?}");
            let fs = SimFs::new();
            let (expected, gone) = populated(&fs);
            let mut db = open_app(&fs).unwrap();
            let seq = db.seq();
            fs.set_fault(Some(SimFault::FailAt(fs.mutations() + n)));
            assert!(db.commit().is_err(), "{ctx}: the fault fired");
            fs.set_fault(None);
            drop(db);
            let crashed = fs.crash(loss);
            let stray = has_cut_commit(&crashed);
            let mut compacting =
                compact_only(&crashed, builtin()).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            if stray {
                cut += 1;
                assert_eq!(compacting.seq(), seq + 1, "{ctx}: one retiring commit");
                assert!(!has_cut_commit(&crashed), "{ctx}: retired");
            }
            compacting.compact().unwrap();
            drop(compacting);
            let reopened = open_app(&crashed).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            assert_holds(&reopened, &expected, &gone, &ctx);
        }
    }
    assert!(cut > 0, "no fault left a cut commit");
}

/// What [`mixed_wal`] leaves live: `notes` with its committed note replaced and four WAL notes, two committed notes
/// deleted in the WAL, and 64 `plain` documents in the WAL.
struct Mixed {
    notes: BTreeMap<String, String>,
    gone: Vec<String>,
    /// The `plain` documents in the WAL.
    plain: usize,
    /// The memtable size to open with: `notes`' memtable never reaches it, the `plain` backlog does, several times.
    threshold: usize,
}

/// The `notes` operations of [`mixed_wal`]'s WAL, in order: a put (`Some`) or a delete (`None`) of an id.
const NOTE_OPS: [(&str, Option<&str>); 7] = [
    ("w1", Some("Walnote one")),
    ("committed", Some("Walnote replaced")),
    ("gone1", None),
    ("w2", Some("Walnote two")),
    ("w3", Some("Walnote three")),
    ("gone2", None),
    ("w4", Some("Walnote four")),
];

/// A memtable size `notes`' memtable stays under with [`NOTE_OPS`] (measured with the real analyzer, which indexes
/// more than the stand-in), and how many `plain` documents make a memtable three times that size: measured, so the
/// test does not depend on the memtable's overheads.
fn mixed_sizes() -> (usize, usize) {
    let full = registry().with_builtins();
    let mut notes_memtable = Memtable::new(&notes(), &full).unwrap();
    for (id, tags) in NOTE_OPS {
        match tags {
            Some(tags) => drop(notes_memtable.put(&note(id, tags)).unwrap()),
            None => drop(notes_memtable.delete(id).unwrap()),
        }
    }
    let threshold = notes_memtable.size_in_bytes() + 1;
    let mut plain_memtable = Memtable::new(&plain(), &full).unwrap();
    let mut docs = 0;
    while plain_memtable.size_in_bytes() < 3 * threshold {
        plain_memtable
            .put(&plain_doc(&format!("p{docs:03}")))
            .unwrap();
        docs += 1;
    }
    assert!(
        docs < 1000,
        "{docs} plain documents: more than a search page"
    );
    (threshold, docs)
}

/// A closed database whose WAL, after the last commit, holds a `plain` backlog over 1 KiB, then `notes` puts, a
/// replace and a delete in the same chunk (closed by a synced put), then more `notes` puts and a delete in a later
/// chunk. The application had a 1 GiB memtable, so it flushed none of them.
fn mixed_wal(fs: &SimFs) -> Mixed {
    let (threshold, plain_docs) = mixed_sizes();
    let container = Container::create_in(vfs(fs), PATH).unwrap();
    let mut db = Database::create_container(container, options(registry(), 1 << 30)).unwrap();
    db.create_collection(notes()).unwrap();
    db.create_collection(plain()).unwrap();
    for id in ["committed", "gone1", "gone2"] {
        db.put("notes", &note(id, "Committed note"), Durability::Buffered)
            .unwrap();
    }
    db.commit().unwrap();
    for i in 0..plain_docs {
        let doc = plain_doc(&format!("p{i:03}"));
        db.put("plain", &doc, Durability::Buffered).unwrap();
    }
    let mut notes = BTreeMap::new();
    let mut gone = Vec::new();
    for (i, (id, tags)) in NOTE_OPS.into_iter().enumerate() {
        // `w2` syncs and so closes the backlog's chunk: `w3`, `gone2` and `w4` are in a later one.
        let durability = if id == "w2" || i + 1 == NOTE_OPS.len() {
            Durability::Fsync
        } else {
            Durability::Buffered
        };
        if let Some(tags) = tags {
            db.put("notes", &note(id, tags), durability).unwrap();
            notes.insert(id.to_owned(), tags.to_owned());
        } else {
            db.delete("notes", id, durability).unwrap();
            gone.push(id.to_owned());
        }
    }
    drop(db);
    Mixed {
        notes,
        gone,
        plain: plain_docs,
        threshold,
    }
}

/// The WAL chunk frames after `log_end` of the container at `PATH`.
fn chunks_after_commit(fs: &SimFs) -> usize {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadOnly).unwrap();
    let log_end = container.superblock().log_end;
    container
        .frames()
        .iter()
        .filter(|f| f.kind == FrameKind::WalChunk && f.offset >= log_end)
        .count()
}

/// `db` holds what [`mixed_wal`] wrote: the WAL notes by id and by their `tags` terms (the real analyzer indexed
/// them), the deleted ones gone, every `plain` document.
fn assert_mixed(db: &Database<SimFsFile>, mixed: &Mixed, ctx: &str) {
    let snapshot = db.snapshot();
    let notes = snapshot.collection("notes").unwrap();
    let tags = notes.schema().field("tags").unwrap().id();
    for (id, text) in &mixed.notes {
        let doc = notes
            .get(id)
            .unwrap()
            .unwrap_or_else(|| panic!("{ctx}: {id} is missing"));
        assert_eq!(
            doc.get(tags),
            Some(&Value::String(text.clone())),
            "{ctx}: {id}"
        );
    }
    for id in &mixed.gone {
        assert!(notes.get(id).unwrap().is_none(), "{ctx}: {id} came back");
    }
    let full = registry().with_builtins();
    let found = search(&snapshot, "notes", &full, "walnote");
    assert_eq!(
        found,
        mixed.notes.keys().cloned().collect::<Vec<_>>(),
        "{ctx}"
    );
    assert!(
        search(&snapshot, "notes", &full, "note").is_empty(),
        "{ctx}: only deleted notes held \"note\""
    );
    assert_eq!(
        search(&snapshot, "plain", &full, "plain").len(),
        mixed.plain,
        "{ctx}"
    );
}

/// Review item 1: the replay flushes the `plain` backlog (no skipped field) while `notes` holds nothing yet, then
/// replays the `notes` records after it, in that chunk and a later one, without flushing them. The open succeeds and
/// commits (the flush); the compaction keeps the `notes` records as WAL records, which the application's open indexes
/// with the real analyzer.
#[test]
fn a_plain_flush_before_the_notes_records_keeps_them_in_the_wal() {
    let fs = SimFs::new();
    let mixed = mixed_wal(&fs);
    assert!(
        chunks_after_commit(&fs) >= 2,
        "the notes records span two chunks"
    );
    let seq = open_app(&fs).map(|db| db.seq()).unwrap();
    let mut db = compact_only(&fs, options(AnalyzerRegistry::builtin(), mixed.threshold)).unwrap();
    assert!(db.seq() > seq, "the replay flushed the plain backlog");
    db.compact().unwrap();
    drop(db);
    let reopened = open_app(&fs).unwrap();
    assert_mixed(&reopened, &mixed, "after the compaction");
    // The `notes` records are still WAL records: nothing of `notes` was flushed with the stand-in.
    let snapshot = reopened.snapshot();
    let notes = snapshot.collection("notes").unwrap();
    assert!(
        notes.memtable().live_count() >= 4,
        "the WAL notes are in the memtable"
    );
}

/// A fault at each mutating call of `open` (a failed write, or one applied then reported failed), on a copy of `base`,
/// then a crash with each loss: the application's open holds every acknowledged document (`holds`), and a second
/// compaction-only open, with `options`, succeeds and compacts.
fn sweep_the_open(
    base: &SimFs,
    options: impl Fn() -> DatabaseOptions,
    holds: impl Fn(&Database<SimFsFile>, &str),
) -> u64 {
    let calls = {
        let copy = base.crash(CrashLoss::Nothing);
        let start = copy.mutations();
        drop(compact_only(&copy, options()).unwrap());
        copy.mutations() - start
    };
    assert!(calls > 0, "the open writes");
    for n in 0..calls {
        for apply_first in [false, true] {
            let ctx = format!("call {n}, apply_first {apply_first}");
            let copy = base.crash(CrashLoss::Nothing);
            let at = copy.mutations() + n;
            copy.set_fault(Some(if apply_first {
                SimFault::ApplyThenFail(at)
            } else {
                SimFault::FailAt(at)
            }));
            let opened = compact_only(&copy, options());
            copy.set_fault(None);
            assert!(opened.is_err(), "{ctx}: the fault fired");
            drop(opened);
            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(n)] {
                let ctx = format!("{ctx}, {loss:?}");
                let crashed = copy.crash(loss);
                let db = open_app(&crashed).unwrap_or_else(|e| panic!("{ctx}: {e}"));
                holds(&db, &ctx);
                drop(db);
                let mut again =
                    compact_only(&crashed, options()).unwrap_or_else(|e| panic!("{ctx}: {e}"));
                again.compact().unwrap_or_else(|e| panic!("{ctx}: {e}"));
                drop(again);
                holds(&open_app(&crashed).unwrap(), &format!("{ctx}, compacted"));
            }
        }
    }
    calls
}

/// Review item 2 (b): a fault and a crash at every write of a compaction-only open whose replay flushes the `plain`
/// backlog with `notes` WAL records after it.
#[test]
fn a_crash_at_any_write_of_its_open_flushing_plain_keeps_every_document() {
    let base = SimFs::new();
    let mixed = mixed_wal(&base);
    let calls = sweep_the_open(
        &base,
        || options(AnalyzerRegistry::builtin(), mixed.threshold),
        |db, ctx| assert_mixed(db, &mixed, ctx),
    );
    assert!(calls > 3, "{calls}");
}

/// A file system holding [`populated`]'s database with a cut commit (a Manifest after `log_end`): the first fault
/// point of an application commit that leaves one, then a crash that loses nothing.
fn with_a_cut_commit() -> (SimFs, Expected, Vec<String>) {
    for n in 0.. {
        let fs = SimFs::new();
        let (expected, gone) = populated(&fs);
        let mut db = open_app(&fs).unwrap();
        fs.set_fault(Some(SimFault::FailAt(fs.mutations() + n)));
        let committed = db.commit();
        fs.set_fault(None);
        drop(db);
        assert!(
            committed.is_err(),
            "no fault point of the commit leaves a cut commit"
        );
        let crashed = fs.crash(CrashLoss::Nothing);
        if has_cut_commit(&crashed) {
            return (crashed, expected, gone);
        }
    }
    panic!("the fault points of a commit ran out without a cut commit");
}

/// Review item 2 (a): a fault and a crash at every write of a compaction-only open that retires a cut commit.
#[test]
fn a_crash_at_any_write_of_its_retiring_open_keeps_every_document() {
    let (base, expected, gone) = with_a_cut_commit();
    let calls = sweep_the_open(&base, builtin, |db, ctx| {
        assert_holds(db, &expected, &gone, ctx);
    });
    assert!(calls > 0, "{calls}");
}

fn notes2() -> CollectionSchema {
    CollectionSchema::from_json_in(
        r#"{"name": "notes2", "fields": {"tags": {"type": "text", "analyzer": "tags"}, "body": {"type": "text"}}}"#,
        &registry(),
    )
    .unwrap()
}

/// A closed database with `notes` and `notes2`, both naming `tags@1`, one committed note in each, and `backlog` WAL
/// notes in `collection` plus one in the other.
fn two_skipped(fs: &SimFs, collection: &str, backlog: usize) {
    let container = Container::create_in(vfs(fs), PATH).unwrap();
    let mut db = Database::create_container(container, options(registry(), 1 << 30)).unwrap();
    db.create_collection(notes()).unwrap();
    db.create_collection(notes2()).unwrap();
    for c in ["notes", "notes2"] {
        db.put(
            c,
            &note("committed", "Shared committed"),
            Durability::Buffered,
        )
        .unwrap();
    }
    db.commit().unwrap();
    let other = if collection == "notes" {
        "notes2"
    } else {
        "notes"
    };
    db.put(
        other,
        &note("single", "Shared single"),
        Durability::Buffered,
    )
    .unwrap();
    for i in 0..backlog {
        db.put(
            collection,
            &note(&format!("b{i:03}"), "Shared backlog"),
            Durability::Buffered,
        )
        .unwrap();
    }
    db.put(collection, &note("last", "Shared last"), Durability::Fsync)
        .unwrap();
}

/// Review item 12: two collections naming the same registered analyzer. The second finds it already replaced by the
/// stand-in (`skip_unregistered`'s `replaced` branch) and is skipped too; a flush of either is refused, naming it; and
/// with small backlogs both compact and keep their WAL records.
#[test]
fn two_collections_sharing_a_skipped_analyzer_are_both_guarded() {
    for collection in ["notes", "notes2"] {
        let fs = SimFs::new();
        two_skipped(&fs, collection, 64);
        let before = fs.files();
        let err = compact_only(&fs, options(AnalyzerRegistry::builtin(), 1024)).unwrap_err();
        match &err {
            DatabaseError::AnalysisSkipped {
                collection: named,
                field,
                analyzer,
            } => {
                // The commit that would flush the backlog also holds the other collection's record: either may be named, and the
                // first in collection order is.
                assert_eq!(named, "notes", "{collection}: {err}");
                assert_eq!((field.as_str(), analyzer.as_str()), ("tags", "tags@1"));
            }
            other => panic!("{collection}: {other:?}"),
        }
        assert_eq!(fs.files(), before, "{collection}: nothing was written");
    }
    // Small backlogs: both skipped, nothing refused, every record kept for the application.
    let fs = SimFs::new();
    two_skipped(&fs, "notes2", 3);
    let mut db = compact_only(&fs, builtin()).unwrap();
    let skipped: Vec<(&str, &str, &str)> = db
        .skipped_analysis()
        .iter()
        .map(|s| (s.collection.as_str(), s.field.as_str(), s.analyzer.as_str()))
        .collect();
    assert_eq!(
        skipped,
        [("notes", "tags", "tags@1"), ("notes2", "tags", "tags@1")]
    );
    db.compact().unwrap();
    drop(db);
    let db = open_app(&fs).unwrap();
    let full = registry().with_builtins();
    assert_eq!(
        search(&db.snapshot(), "notes", &full, "shared"),
        ["committed", "single"]
    );
    assert_eq!(search(&db.snapshot(), "notes2", &full, "shared").len(), 5);
}
