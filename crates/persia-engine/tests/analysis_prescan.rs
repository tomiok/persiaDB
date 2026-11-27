//! The pre-scan of an open that skips analysis (ROADMAP 10.4.8; SPEC §8.6; ADR-0068 and ADR-0069 amendments): before
//! its replay, a compaction-only or mark-only open with the built-in analyzers decodes the WAL from `wal_start` once,
//! and refuses (`AnalysisSkipped`) when it holds a *pending record* of a collection whose analysis it skips: a record
//! the replay would apply to that collection.
//!
//! Invariants:
//! 1. (property) Over generated sessions of the application (puts and deletes in two collections naming the registered
//!    `tags@1` and in `plain`, commits, a drop and re-creation, and an end that is clean or a commit cut by a fault and
//!    a crash), each open is refused iff the model holds an operation on a skipped collection after the last commit
//!    that took effect, whatever the open's flush threshold; the refusal names the first such collection. A refused
//!    open leaves every file as a bare read-write open of the container leaves it (its tail repair only); and the
//!    application's next open shows exactly the model's documents.
//! 2. Both orderings (a skipped record before, or after, a `plain` backlog that crosses the open's flush threshold):
//!    `N` refused opens in a row write nothing, so no segment and no commit; the application's next open shows every
//!    document; once it has committed, the open succeeds.
//! 3. A refused open's only writes are the container's tail repair: the same mutations and bytes as a bare open, and a
//!    fault at each of them, then a crash, loses nothing; the next refused open again writes only that repair.
//! 4. Corrupt WAL bytes the pre-scan reaches give the replay's error for them (`Corrupt`), the one the application's
//!    open gives too, and write nothing after a pending record; before any pending record the open is the writer's
//!    and fails as it does. A skipped collection's put that does not decode is not pending.
//! 5. Without a pending record the open is the writer's: the files are byte-identical to the application's open,
//!    a replay-time flush of `plain` and a retired cut commit included.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err,
    clippy::too_many_lines,
    missing_docs
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_analysis::{Analyzer, AnalyzerRegistry, LowercaseFilter, WhitespaceTokenizer};
use persia_engine::db::{
    CompactOnlyDatabase, CompactionOptions, Database, DatabaseError, DatabaseOptions, ManualClock,
    MarkOnlyDatabase, ReadOnlyDatabase, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::Memtable;
use persia_engine::merge::MergeOptions;
use persia_engine::schema::CollectionSchema;
use persia_engine::wal::{Durability, ReplayError, WalRecord, WalWriter};
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::container::{Container, ContainerFile, Mode};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use proptest::prelude::*;

const PATH: &str = "/db.persia";

/// The collections: two name the registered `tags@1` (skipped by a handle with the built-ins only), `plain` does not.
const COLLECTIONS: [&str; 3] = ["notes", "notes2", "plain"];

/// `tags@1`: whitespace, lowercased. Not a built-in.
fn registry() -> AnalyzerRegistry {
    let mut registry = AnalyzerRegistry::new();
    let analyzer = Analyzer::builder(WhitespaceTokenizer)
        .filter(LowercaseFilter)
        .build();
    registry.register("tags", 1, analyzer).unwrap();
    registry
}

fn schema(name: &str) -> CollectionSchema {
    if name == "plain" {
        CollectionSchema::from_json(r#"{"name": "plain", "fields": {"body": {"type": "text"}}}"#)
            .unwrap()
    } else {
        CollectionSchema::from_json_in(
            &format!(
                r#"{{"name": "{name}", "fields": {{"tags": {{"type": "text", "analyzer": "tags"}}, "body": {{"type": "text"}}}}}}"#
            ),
            &registry(),
        )
        .unwrap()
    }
}

fn is_skipped(collection: &str) -> bool {
    collection != "plain"
}

/// The text a document of `collection` holds at `version`, in its `tags` field (skipped ones) or `body` (`plain`).
fn text(id: &str, version: u32) -> String {
    format!("{id} version{version}")
}

fn doc(collection: &str, id: &str, version: u32) -> Document {
    let s = schema(collection);
    let field = if is_skipped(collection) {
        "tags"
    } else {
        "body"
    };
    Document::new(id).with(
        s.field(field).unwrap().id(),
        Value::String(text(id, version)),
    )
}

/// No automatic merge or compaction, a seeded clock and segment ids; `registry` and `memtable_bytes`.
fn options(registry: AnalyzerRegistry, memtable_bytes: usize) -> DatabaseOptions {
    let mut options = DatabaseOptions {
        registry,
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(7),
        merge: MergeOptions {
            threads: 0,
            ..MergeOptions::default()
        },
        compaction: CompactionOptions {
            automatic: false,
            ..CompactionOptions::default()
        },
        ..DatabaseOptions::default()
    };
    options.policy.flush.memtable_bytes = memtable_bytes;
    options
}

/// The application's memtable: large enough that its own sessions never flush on their own.
const APP_MEMTABLE: usize = 1 << 30;

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

fn open_app(fs: &SimFs, memtable_bytes: usize) -> Result<Database<SimFsFile>, DatabaseError> {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadWrite)?;
    Database::open_container(container, options(registry(), memtable_bytes))
}

fn create_app(fs: &SimFs) -> Database<SimFsFile> {
    let container = Container::create_in(vfs(fs), PATH).unwrap();
    let mut db = Database::create_container(container, options(registry(), APP_MEMTABLE)).unwrap();
    for name in COLLECTIONS {
        db.create_collection(schema(name)).unwrap();
    }
    db
}

/// Which handle opens with the built-in analyzers only.
#[derive(Debug, Clone, Copy)]
enum Handle {
    Compact,
    Mark,
}

/// Opens `fs`'s database with `handle`, the built-ins and `memtable_bytes`, and closes it.
fn open_skipping(fs: &SimFs, handle: Handle, memtable_bytes: usize) -> Result<(), DatabaseError> {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadWrite)?;
    let options = options(AnalyzerRegistry::builtin(), memtable_bytes);
    match handle {
        Handle::Compact => CompactOnlyDatabase::open_container(container, options).map(drop),
        Handle::Mark => MarkOnlyDatabase::open_container(container, options).map(drop),
    }
}

type Files = Vec<(PathBuf, Vec<u8>)>;

/// The files a bare read-write open of the container (its recovery: the tail repair, and nothing of the database)
/// leaves, on a copy of `fs`.
fn after_bare_open(fs: &SimFs) -> Files {
    let copy = fs.crash(CrashLoss::Nothing);
    drop(Container::open_in(vfs(&copy), PATH, Mode::ReadWrite).unwrap());
    copy.files()
}

/// The collection a refusal names, or the error's debug form.
fn refused(result: Result<(), DatabaseError>) -> Result<String, String> {
    match result {
        Err(DatabaseError::AnalysisSkipped {
            collection,
            field,
            analyzer,
        }) => {
            assert_eq!((field.as_str(), analyzer.as_str()), ("tags", "tags@1"));
            Ok(collection)
        }
        other => Err(format!("{other:?}")),
    }
}

/// The documents of `ids` in every collection, as `db` shows them (by lookup: tombstones and replacements applied).
type Docs = BTreeMap<(String, String), String>;

fn shown(db: &Database<SimFsFile>, ids: &[String]) -> Docs {
    let snapshot = db.snapshot();
    let mut out = Docs::new();
    for name in COLLECTIONS {
        let c = snapshot.collection(name).unwrap();
        let field = if is_skipped(name) { "tags" } else { "body" };
        let field = c.schema().field(field).unwrap().id();
        for id in ids {
            if let Some(d) = c.get(id).unwrap() {
                let Some(Value::String(t)) = d.get(field) else {
                    panic!("{name}/{id}: no text");
                };
                out.insert((name.to_owned(), id.clone()), t.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------
// 1. The property
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Step {
    Put {
        c: usize,
        id: u8,
        fsync: bool,
    },
    Delete {
        c: usize,
        id: u8,
        fsync: bool,
    },
    Commit,
    /// Drops the collection and creates it again: two commits.
    Recreate {
        c: usize,
    },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (0..3_usize, 0..5_u8, any::<bool>()).prop_map(|(c, id, fsync)| Step::Put { c, id, fsync }),
        3 => (0..3_usize, 0..5_u8, any::<bool>()).prop_map(|(c, id, fsync)| Step::Delete { c, id, fsync }),
        1 => Just(Step::Commit),
        1 => (0..3_usize).prop_map(|c| Step::Recreate { c }),
    ]
}

/// How the session ends: closed cleanly, or a commit failed at its `n`-th mutating call and then a crash (losing
/// every unsynced write, or nothing).
#[derive(Debug, Clone)]
enum End {
    Clean,
    Cut { n: u64, lose_all: bool },
}

fn end() -> impl Strategy<Value = End> {
    prop_oneof![
        1 => Just(End::Clean),
        2 => (0..12_u64, any::<bool>()).prop_map(|(n, lose_all)| End::Cut { n, lose_all }),
    ]
}

/// The pool of ids every collection's documents use.
fn pool() -> Vec<String> {
    (0..5).map(|i| format!("d{i}")).collect()
}

/// Runs `steps` and `end` as the application, on a new file system; returns it with the model: the documents, and the
/// skipped collections with an operation after the last commit that took effect.
fn session(steps: &[Step], end: &End) -> (SimFs, Docs, BTreeSet<String>) {
    let fs = SimFs::new();
    let mut db = create_app(&fs);
    let mut docs = Docs::new();
    let mut pending = BTreeSet::new();
    let mut version = 0_u32;
    let last = steps.len().saturating_sub(1);
    for (i, s) in steps.iter().enumerate() {
        // The last operation syncs every record before it (SPEC §8.2.1): a crash at the end loses none.
        let durability = |fsync: bool| {
            if fsync || i == last {
                Durability::Fsync
            } else {
                Durability::Buffered
            }
        };
        match *s {
            Step::Put { c, id, fsync } => {
                let (name, id) = (COLLECTIONS[c], format!("d{id}"));
                version += 1;
                db.put(name, &doc(name, &id, version), durability(fsync))
                    .unwrap();
                docs.insert((name.to_owned(), id.clone()), text(&id, version));
                if is_skipped(name) {
                    pending.insert(name.to_owned());
                }
            }
            Step::Delete { c, id, fsync } => {
                let (name, id) = (COLLECTIONS[c], format!("d{id}"));
                db.delete(name, &id, durability(fsync)).unwrap();
                docs.remove(&(name.to_owned(), id));
                if is_skipped(name) {
                    pending.insert(name.to_owned());
                }
            }
            Step::Commit => {
                db.commit().unwrap();
                pending.clear();
            }
            Step::Recreate { c } => {
                let name = COLLECTIONS[c];
                db.drop_collection(name).unwrap();
                db.create_collection(schema(name)).unwrap();
                docs.retain(|(collection, _), _| collection != name);
                pending.clear();
            }
        }
    }
    match *end {
        End::Clean => {
            drop(db);
            (fs, docs, pending)
        }
        End::Cut { n, lose_all } => {
            let seq = db.seq();
            fs.set_fault(Some(SimFault::FailAt(fs.mutations() + n)));
            let committed = db.commit();
            fs.set_fault(None);
            drop(db);
            let crashed = fs.crash(if lose_all {
                CrashLoss::All
            } else {
                CrashLoss::Nothing
            });
            let container = Container::open_in(vfs(&crashed), PATH, Mode::ReadOnly).unwrap();
            let ro =
                ReadOnlyDatabase::open_container(container, &registry().with_builtins()).unwrap();
            // A commit that took effect flushed every memtable: nothing is pending.
            if committed.is_ok() || ro.seq() > seq {
                pending.clear();
            }
            drop(ro);
            (crashed, docs, pending)
        }
    }
}

#[cfg(not(feature = "slow"))]
const CASES: u32 = 64;
#[cfg(feature = "slow")]
const CASES: u32 = 1024;

proptest! {
    #![proptest_config(ProptestConfig { cases: CASES, ..ProptestConfig::default() })]

    /// Invariant 1.
    #[test]
    fn an_open_is_refused_iff_a_skipped_collection_has_a_pending_record(
        steps in prop::collection::vec(step(), 0..40),
        end in end(),
        threshold in prop_oneof![Just(64_usize << 20), 1..4096_usize],
        mark in any::<bool>(),
    ) {
        let handle = if mark { Handle::Mark } else { Handle::Compact };
        let (fs, docs, pending) = session(&steps, &end);
        let bare = after_bare_open(&fs);
        let result = open_skipping(&fs, handle, threshold);
        match pending.first() {
            Some(first) => {
                prop_assert_eq!(refused(result), Ok(first.clone()));
                prop_assert!(fs.files() == bare, "a refused open wrote beyond its tail repair");
            }
            None => prop_assert!(result.is_ok(), "{:?}", result),
        }
        let app = open_app(&fs, APP_MEMTABLE).unwrap();
        prop_assert_eq!(shown(&app, &pool()), docs);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 2. Both orderings, repeated
// ---------------------------------------------------------------------------------------------------------------

/// The heap size of a `plain` memtable holding `docs` documents: a measured flush threshold.
fn plain_threshold(docs: usize) -> usize {
    let mut memtable = Memtable::new(&schema("plain"), &registry().with_builtins()).unwrap();
    for i in 0..docs {
        memtable.put(&doc("plain", &format!("m{i:03}"), 1)).unwrap();
    }
    memtable.size_in_bytes()
}

/// Where the skipped record sits relative to the `plain` backlog.
#[derive(Debug, Clone, Copy)]
enum Order {
    /// One `notes` put, then the backlog.
    Before,
    /// The backlog, then one `notes` put.
    After,
}

/// A closed database: one committed note, then a WAL of 64 `plain` documents (crossing [`plain_threshold`]`(8)`
/// several times) and one `notes` put at the place `order` says. Returns the documents and their ids.
fn ordered(fs: &SimFs, order: Order) -> (Docs, Vec<String>) {
    let mut db = create_app(fs);
    let mut docs = Docs::new();
    let mut put = |db: &mut Database<SimFsFile>, name: &str, id: &str, durability| {
        db.put(name, &doc(name, id, 1), durability).unwrap();
        docs.insert((name.to_owned(), id.to_owned()), text(id, 1));
    };
    put(&mut db, "notes", "committed", Durability::Buffered);
    db.commit().unwrap();
    if let Order::Before = order {
        put(&mut db, "notes", "skipped", Durability::Buffered);
    }
    for i in 0..64 {
        put(&mut db, "plain", &format!("p{i:03}"), Durability::Buffered);
    }
    if let Order::After = order {
        put(&mut db, "notes", "skipped", Durability::Buffered);
    }
    put(&mut db, "plain", "last", Durability::Fsync);
    drop(db);
    let mut ids: Vec<String> = (0..64).map(|i| format!("p{i:03}")).collect();
    ids.extend(["committed", "skipped", "last"].map(String::from));
    (docs, ids)
}

/// Invariant 2, for both orderings and both handles.
#[test]
fn repeated_refusals_in_either_order_write_nothing_and_lose_nothing() {
    const REFUSALS: usize = 4;
    let threshold = plain_threshold(8);
    for order in [Order::Before, Order::After] {
        for handle in [Handle::Compact, Handle::Mark] {
            let ctx = format!("{order:?}, {handle:?}");
            let fs = SimFs::new();
            let (docs, ids) = ordered(&fs, order);
            let before = fs.files();
            // What a bare read-write open of the container does (its lock; no tail to repair here).
            let bare_calls = {
                let copy = fs.crash(CrashLoss::Nothing);
                let start = copy.mutations();
                drop(Container::open_in(vfs(&copy), PATH, Mode::ReadWrite).unwrap());
                copy.mutations() - start
            };
            for n in 0..REFUSALS {
                let start = fs.mutations();
                let result = open_skipping(&fs, handle, threshold);
                assert_eq!(
                    refused(result),
                    Ok("notes".to_owned()),
                    "{ctx}, refusal {n}"
                );
                assert!(fs.files() == before, "{ctx}, refusal {n}: a byte changed");
                assert_eq!(
                    fs.mutations() - start,
                    bare_calls,
                    "{ctx}, refusal {n}: a bare open's calls only"
                );
            }
            // The application's next open shows every document, and its commit lets the open through.
            let mut app = open_app(&fs, APP_MEMTABLE).unwrap();
            assert_eq!(shown(&app, &ids), docs, "{ctx}");
            app.commit().unwrap();
            drop(app);
            open_skipping(&fs, handle, threshold).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            let app = open_app(&fs, APP_MEMTABLE).unwrap();
            assert_eq!(shown(&app, &ids), docs, "{ctx}: after the open");
        }
    }
}

/// Invariant 2, the other side: without the `notes` put, the same backlog flushes during the open as in a writer's.
#[test]
fn the_same_backlog_without_a_skipped_record_flushes_during_the_open() {
    let fs = SimFs::new();
    let mut db = create_app(&fs);
    db.put("notes", &doc("notes", "committed", 1), Durability::Buffered)
        .unwrap();
    db.commit().unwrap();
    for i in 0..64 {
        let id = format!("p{i:03}");
        db.put("plain", &doc("plain", &id, 1), Durability::Fsync)
            .unwrap();
    }
    let seq = db.seq();
    drop(db);
    let container = Container::open_in(vfs(&fs), PATH, Mode::ReadWrite).unwrap();
    let opened = CompactOnlyDatabase::open_container(
        container,
        options(AnalyzerRegistry::builtin(), plain_threshold(8)),
    )
    .unwrap();
    assert!(
        opened.seq() >= seq + 2,
        "the replay flushed plain: {seq} -> {}",
        opened.seq()
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 3. Only the tail repair
// ---------------------------------------------------------------------------------------------------------------

/// Zeros after the last frame of the container on `fs`: a torn tail, which a read-write open truncates (SPEC §4.4).
fn tear_tail(fs: &SimFs) {
    let file = fs.open(Path::new(PATH), true).unwrap();
    let len = file.file_len().unwrap();
    file.write_all_at(&[0; 3000], len).unwrap();
    file.sync_all().unwrap();
}

/// Invariant 3.
#[test]
fn a_refused_open_writes_only_the_tail_repair_and_a_fault_there_loses_nothing() {
    let threshold = plain_threshold(8);
    for order in [Order::Before, Order::After] {
        let base = SimFs::new();
        let (docs, ids) = ordered(&base, order);
        tear_tail(&base);
        let repaired = after_bare_open(&base);
        let bare_calls = {
            let copy = base.crash(CrashLoss::Nothing);
            let start = copy.mutations();
            drop(Container::open_in(vfs(&copy), PATH, Mode::ReadWrite).unwrap());
            copy.mutations() - start
        };
        assert!(bare_calls > 0, "{order:?}: the tail repair writes");
        for handle in [Handle::Compact, Handle::Mark] {
            let ctx = format!("{order:?}, {handle:?}");
            let copy = base.crash(CrashLoss::Nothing);
            let start = copy.mutations();
            assert_eq!(
                refused(open_skipping(&copy, handle, threshold)),
                Ok("notes".to_owned()),
                "{ctx}"
            );
            assert_eq!(
                copy.mutations() - start,
                bare_calls,
                "{ctx}: the same writes as the tail repair"
            );
            assert!(
                copy.files() == repaired,
                "{ctx}: the same bytes as the tail repair"
            );
            for n in 0..bare_calls {
                for apply_first in [false, true] {
                    let ctx = format!("{ctx}, call {n}, apply_first {apply_first}");
                    let copy = base.crash(CrashLoss::Nothing);
                    let at = copy.mutations() + n;
                    copy.set_fault(Some(if apply_first {
                        SimFault::ApplyThenFail(at)
                    } else {
                        SimFault::FailAt(at)
                    }));
                    let result = open_skipping(&copy, handle, threshold);
                    copy.set_fault(None);
                    assert!(result.is_err(), "{ctx}: {result:?}");
                    for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(n)] {
                        let ctx = format!("{ctx}, {loss:?}");
                        let crashed = copy.crash(loss);
                        let repaired_again = after_bare_open(&crashed);
                        let start = crashed.mutations();
                        let again = open_skipping(&crashed, handle, threshold);
                        assert_eq!(refused(again), Ok("notes".to_owned()), "{ctx}");
                        assert!(
                            crashed.files() == repaired_again,
                            "{ctx}: only the tail repair again"
                        );
                        assert!(crashed.mutations() - start <= bare_calls, "{ctx}");
                        let app = open_app(&crashed, APP_MEMTABLE)
                            .unwrap_or_else(|e| panic!("{ctx}: {e}"));
                        assert_eq!(shown(&app, &ids), docs, "{ctx}");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 4. Corrupt WAL bytes
// ---------------------------------------------------------------------------------------------------------------

/// What a damaged WAL holds, after a committed note and one `plain` document in the WAL.
#[derive(Debug, Clone, Copy)]
enum Damage {
    /// A put of a collection the manifest lacks (SPEC §8.2.1: corruption).
    UnknownCollection,
    /// A `notes` put whose document does not decode with its schema.
    BadDocument,
    /// A `WalChunk` frame whose payload is not a chunk.
    BadChunk,
}

/// Appends `damage` to the WAL of the closed database on `fs`, as a synced frame after `log_end`.
fn damage_wal(fs: &SimFs, damage: Damage) {
    let mut container = Container::open_in(vfs(fs), PATH, Mode::ReadWrite).unwrap();
    match damage {
        Damage::BadChunk => {
            container
                .append(FrameKind::WalChunk, b"not a chunk", Compression::None)
                .unwrap();
            container.sync().unwrap();
        }
        Damage::UnknownCollection | Damage::BadDocument => {
            let mut wal = WalWriter::resume(container, BTreeMap::new());
            let bytes = doc("plain", "x", 1)
                .encode(&schema("plain"), persia_engine::document::FieldSet::All)
                .unwrap();
            let record = match damage {
                Damage::UnknownCollection => WalRecord::put("ghost", &bytes).unwrap(),
                _ => WalRecord::put("notes", b"\xff\xff not a document").unwrap(),
            };
            wal.append_batch(&[record], Durability::Fsync).unwrap();
        }
    }
}

/// The replay error's kind and position, to compare two opens' errors.
fn replay_error(err: &DatabaseError) -> (u8, u64, Option<u64>) {
    match err {
        DatabaseError::Replay(ReplayError::UnknownCollection { chunk, offset, .. }) => {
            (0, *chunk, Some(*offset))
        }
        DatabaseError::Replay(ReplayError::Document { chunk, offset, .. }) => {
            (1, *chunk, Some(*offset))
        }
        DatabaseError::Replay(ReplayError::Chunk { chunk, .. }) => (2, *chunk, None),
        other => panic!("not a replay error: {other:?}"),
    }
}

/// Invariant 4.
#[test]
fn corrupt_wal_bytes_give_the_replays_error_and_write_nothing_after_a_pending_record() {
    for damage in [
        Damage::UnknownCollection,
        Damage::BadDocument,
        Damage::BadChunk,
    ] {
        for pending_first in [true, false] {
            for handle in [Handle::Compact, Handle::Mark] {
                let ctx = format!("{damage:?}, pending first {pending_first}, {handle:?}");
                let fs = SimFs::new();
                let mut db = create_app(&fs);
                db.put("notes", &doc("notes", "committed", 1), Durability::Buffered)
                    .unwrap();
                db.commit().unwrap();
                db.put("plain", &doc("plain", "p", 1), Durability::Buffered)
                    .unwrap();
                if pending_first {
                    db.put("notes", &doc("notes", "w", 1), Durability::Fsync)
                        .unwrap();
                } else {
                    db.put("plain", &doc("plain", "q", 1), Durability::Fsync)
                        .unwrap();
                }
                drop(db);
                damage_wal(&fs, damage);
                if !pending_first {
                    // A pending record after the damage: the replay never applies it.
                    let mut wal = WalWriter::resume(
                        Container::open_in(vfs(&fs), PATH, Mode::ReadWrite).unwrap(),
                        BTreeMap::new(),
                    );
                    let bytes = doc("notes", "late", 1)
                        .encode(&schema("notes"), persia_engine::document::FieldSet::All)
                        .unwrap();
                    wal.append_batch(
                        &[WalRecord::put("notes", &bytes).unwrap()],
                        Durability::Fsync,
                    )
                    .unwrap();
                }
                let app_err = open_app(&fs.crash(CrashLoss::Nothing), APP_MEMTABLE).unwrap_err();
                assert!(app_err.is_corruption(), "{ctx}: {app_err:?}");
                let before = fs.files();
                let err = open_skipping(&fs, handle, 64 << 20).unwrap_err();
                assert!(err.is_corruption(), "{ctx}: {err:?}");
                assert_eq!(
                    replay_error(&err),
                    replay_error(&app_err),
                    "{ctx}: the writer's error"
                );
                assert!(fs.files() == before, "{ctx}: nothing was written");
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 5. Without a pending record, the writer's open
// ---------------------------------------------------------------------------------------------------------------

/// Whether the container at `PATH` on `fs` has a Manifest frame after `log_end`: a commit cut before its slot.
fn has_cut_commit(fs: &SimFs) -> bool {
    let container = Container::open_in(vfs(fs), PATH, Mode::ReadOnly).unwrap();
    let log_end = container.superblock().log_end;
    container
        .frames()
        .iter()
        .any(|f| f.kind == FrameKind::Manifest && f.offset >= log_end)
}

/// Invariant 5: a `plain` backlog that the open flushes (no `notes` record after the last commit), then the same with a
/// commit of the application cut before its slot; each open's files equal the application's open's.
#[test]
fn without_a_pending_record_the_files_are_the_writers_byte_for_byte() {
    let threshold = plain_threshold(8);
    let base = SimFs::new();
    let mut db = create_app(&base);
    db.put("notes", &doc("notes", "committed", 1), Durability::Buffered)
        .unwrap();
    db.commit().unwrap();
    for i in 0..40 {
        let id = format!("p{i:03}");
        db.put("plain", &doc("plain", &id, 1), Durability::Buffered)
            .unwrap();
    }
    db.put("plain", &doc("plain", "last", 1), Durability::Fsync)
        .unwrap();
    drop(db);
    // A cut commit over the same WAL: the first fault point of a commit that leaves one.
    let cut = (0..)
        .find_map(|n| {
            let fs = base.crash(CrashLoss::Nothing);
            let mut db = open_app(&fs, APP_MEMTABLE).unwrap();
            fs.set_fault(Some(SimFault::FailAt(fs.mutations() + n)));
            assert!(db.commit().is_err(), "no fault point leaves a cut commit");
            fs.set_fault(None);
            drop(db);
            let crashed = fs.crash(CrashLoss::Nothing);
            has_cut_commit(&crashed).then_some(crashed)
        })
        .unwrap();
    for (name, fs) in [("backlog", &base), ("cut commit", &cut)] {
        let writer = fs.crash(CrashLoss::Nothing);
        let writer_seq = open_app(&writer, threshold).unwrap().seq();
        for handle in [Handle::Compact, Handle::Mark] {
            let ctx = format!("{name}, {handle:?}");
            let copy = fs.crash(CrashLoss::Nothing);
            open_skipping(&copy, handle, threshold).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            assert!(
                copy.files() == writer.files(),
                "{ctx}: other bytes than the writer's open"
            );
            assert_eq!(
                open_app(&copy, APP_MEMTABLE).unwrap().seq(),
                writer_seq,
                "{ctx}"
            );
        }
        assert!(!has_cut_commit(&writer), "{name}: retired");
    }
}
