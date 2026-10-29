//! Opening a database (ROADMAP 6.6.1, 6.6.2, 6.6.3, 6.6.5; SPEC §4.5.1, §8.2.1, §8.4, §8.6).
//!
//! Invariants:
//! 1. The open sequence is idempotent: opening read-write again and again without writing shows the same documents
//!    and `seq` and leaves the file's bytes as they were; a read-only open shows the same documents (6.6.1).
//! 2. One writer: a second read-write open of a file fails fast with `Locked` while the first is open, and succeeds
//!    once it is closed. A read-only open succeeds beside the writer, takes no lock, writes nothing (not even a torn
//!    tail's truncation), and keeps the snapshot it found while the writer commits (6.6.2).
//! 3. `verify` passes on a sound database, counting what it checked, reports a torn tail and stale frames (which a
//!    writer truncates too, so both agree), and fails as corruption on other corrupt bytes after `log_end` (6.6.3).
//! 4. A replayed backlog larger than a memtable's size threshold is flushed during the open, as writes would be, and
//!    shows exactly the documents of the model; the next open replays to the same state without committing again; a
//!    crash at any write of such an open, losing any unsynced part, reopens to the same documents (6.6.5).
//! 5. Replayed records stay needed until a flush covers them: a commit after the open that flushes nothing (a
//!    collection create) keeps `wal_start` before them, with or without flushes during the replay (6.6.5).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use persia_format::frame::FrameKind;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    CommitPolicy, Database, DatabaseError, DatabaseOptions, ManualClock, ReadOnlyDatabase,
    SegmentIds, Snapshot, Tail,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::FlushPolicy;
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{ContainerFile, Error as ContainerError, ReadOnlyReason};

type Docs = BTreeMap<String, BTreeMap<String, String>>;

fn schema(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn doc(id: &str, text: &str) -> Document {
    let body = schema("x").field("body").unwrap().id();
    Document::new(id).with(body, Value::String(text.into()))
}

fn options(memtable_bytes: usize) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(7),
        policy: CommitPolicy {
            flush: FlushPolicy {
                memtable_bytes,
                ..FlushPolicy::default()
            },
            ..CommitPolicy::default()
        },
        ..DatabaseOptions::default()
    }
}

const BIG: usize = 64 << 20;

/// Every live document of every collection `snapshot` shows, by collection and id, with its body.
fn observed(snapshot: &Snapshot) -> Docs {
    let mut all = Docs::new();
    for (name, c) in snapshot.collections() {
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
        let docs = all.entry(name.to_owned()).or_default();
        for id in ids {
            if let Some(d) = c.get(&id).unwrap() {
                let Some((_, Value::String(text))) = d.fields().next() else {
                    panic!("{id}: no body");
                };
                docs.insert(id, text.clone());
            }
        }
    }
    all
}

/// A database with committed segments in `a` and `b`, then a synced, uncommitted backlog of upserts, deletes and
/// fresh puts, mostly in `b`, that does not fit a memtable of a few KiB. Returns the file and the model.
fn backlog() -> (SimFile, Docs) {
    let file = SimFile::new();
    let mut model = Docs::new();
    let mut db = Database::create_on(file.clone(), [3; 16], options(BIG)).unwrap();
    for name in ["a", "b"] {
        db.create_collection(schema(name)).unwrap();
        model.insert(name.to_owned(), BTreeMap::new());
    }
    for i in 0..20 {
        put(
            &mut db,
            &mut model,
            "a",
            &format!("a{i}"),
            &format!("first alpha {i}"),
        );
        put(
            &mut db,
            &mut model,
            "b",
            &format!("b{i}"),
            &format!("first beta {i}"),
        );
    }
    db.commit().unwrap();
    // One record of `a` holds `wal_start` back for the whole backlog of `b`.
    put(&mut db, &mut model, "a", "a0", "held back");
    for i in 0..300 {
        let id = format!("b{}", i % 60);
        if i % 7 == 3 {
            db.delete("b", &id, Durability::Fsync).unwrap();
            model.get_mut("b").unwrap().remove(&id);
        } else {
            put(
                &mut db,
                &mut model,
                "b",
                &id,
                &format!("backlog beta {i} with a few more words"),
            );
        }
    }
    drop(db);
    (file.crash(CrashLoss::All), model)
}

#[test]
fn reopening_without_writes_changes_nothing() {
    let (file, model) = backlog();
    let bytes = file.bytes();
    let mut seq = None;
    for round in 0..3 {
        let db = Database::open_on(file.clone(), options(BIG)).unwrap();
        assert_eq!(observed(&db.snapshot()), model, "round {round}");
        assert_eq!(*seq.get_or_insert(db.seq()), db.seq(), "round {round}");
        drop(db);
        assert!(
            file.bytes() == bytes,
            "round {round}: the open changed the file"
        );
    }
    let reader = ReadOnlyDatabase::open_on(file.clone(), &AnalyzerRegistry::builtin()).unwrap();
    assert_eq!(observed(&reader.snapshot()), model);
    assert_eq!(Some(reader.seq()), seq);
    assert_eq!(reader.replayed(), 301);
}

/// Runs `hook` on the second `file_len`: after a read-only open has read the superblock slots and before it
/// samples the length the tail scan uses (SPEC §4.5.1).
struct OnSecondStat {
    file: SimFile,
    stats: AtomicU64,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for OnSecondStat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnSecondStat")
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

impl OnSecondStat {
    fn new(file: SimFile, hook: impl FnOnce() + Send + 'static) -> Self {
        Self {
            file,
            stats: AtomicU64::new(0),
            hook: Mutex::new(Some(Box::new(hook))),
        }
    }
}

impl ContainerFile for OnSecondStat {
    fn file_len(&self) -> io::Result<u64> {
        // The first stat sizes the header read. The second is the length the tail scan is bounded by, taken
        // after the slots were read (SPEC §4.5.1).
        if self.stats.fetch_add(1, Ordering::Relaxed) == 1
            && let Some(hook) = self.hook.lock().unwrap().take()
        {
            hook();
        }
        self.file.file_len()
    }
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_all_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
    fn try_lock(&self) -> io::Result<bool> {
        self.file.try_lock()
    }
}

/// A writer creates a collection and logs a put to it after the reader has read the superblock and before the
/// tail scan's length is taken (SPEC §8.4, §8.6). The open keeps the loaded snapshot: the older collection's
/// uncommitted put, not the new collection.
#[test]
fn a_read_only_open_stops_before_a_collection_created_during_its_scan() {
    let file = SimFile::new();
    let registry = AnalyzerRegistry::builtin();
    let mut db = Database::create_on(file.clone(), [3; 16], options(BIG)).unwrap();
    db.create_collection(schema("notes")).unwrap();
    db.put("notes", &doc("n1", "hello"), Durability::Fsync)
        .unwrap();
    let seq = db.seq();

    let racing = OnSecondStat::new(file.clone(), move || {
        db.create_collection(schema("extra")).unwrap();
        db.put("extra", &doc("e1", "during"), Durability::Fsync)
            .unwrap();
    });
    let reader = ReadOnlyDatabase::open_on(racing, &registry)
        .unwrap_or_else(|e| panic!("read-only open beside a creating writer: {e}"));
    assert_eq!(reader.seq(), seq);
    assert_eq!(reader.replayed(), 1);
    assert_eq!(
        observed(&reader.snapshot()),
        Docs::from([(
            "notes".into(),
            BTreeMap::from([("n1".into(), "hello".into())])
        )])
    );
    // The scan did see the newer commit: its Manifest, then the put logged after it.
    let uncommitted = reader.container().uncommitted_frames();
    let manifest = uncommitted
        .iter()
        .find(|f| f.kind == FrameKind::Manifest)
        .expect("the create's Manifest is in the scanned tail");
    assert!(
        uncommitted
            .iter()
            .any(|f| f.kind == FrameKind::WalChunk && f.offset >= manifest.end()),
        "the put logged after that Manifest was scanned: {uncommitted:?}"
    );
    reader.verify().unwrap();

    // Opened again, against the superblock the create published: both collections.
    let later = ReadOnlyDatabase::open_on(file, &registry).unwrap();
    assert!(later.seq() > seq);
    let docs = observed(&later.snapshot());
    assert_eq!(docs["notes"]["n1"], "hello");
    assert_eq!(docs["extra"]["e1"], "during");
    later.verify().unwrap();
}

#[test]
fn one_writer_and_read_only_readers_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let registry = AnalyzerRegistry::builtin();
    let mut db = Database::create(&path, options(BIG)).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("a1", "one"), Durability::Fsync).unwrap();

    assert!(matches!(
        Database::open(&path, options(BIG)),
        Err(DatabaseError::Container(ContainerError::Locked))
    ));
    let reader = ReadOnlyDatabase::open(&path, &registry).unwrap();
    let before = observed(&reader.snapshot());
    assert_eq!(before["a"].len(), 1);
    assert_eq!(reader.seq(), db.seq());
    // Several readers at once, none of them a writer.
    let second = ReadOnlyDatabase::open(&path, &registry).unwrap();
    assert_eq!(observed(&second.snapshot()), before);

    // The writer commits; the reader keeps what it found.
    db.put("a", &doc("a2", "two"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    assert_eq!(observed(&reader.snapshot()), before);
    assert!(reader.seq() < db.seq());
    let later = ReadOnlyDatabase::open(&path, &registry).unwrap();
    assert_eq!(later.seq(), db.seq());
    assert_eq!(observed(&later.snapshot())["a"].len(), 2);

    drop(db);
    // The readers hold no lock: the next writer opens beside them.
    let db = Database::open(&path, options(BIG)).unwrap();
    assert_eq!(observed(&db.snapshot()), observed(&later.snapshot()));
}

#[test]
fn a_read_only_open_never_writes_and_verify_reports_the_tail() {
    let (file, model) = backlog();
    let registry = AnalyzerRegistry::builtin();
    // Zeros after the last frame: a torn tail, which a read-write open would truncate.
    let mut bytes = file.bytes();
    let end = bytes.len();
    bytes.resize(end + 4096, 0);
    let torn = SimFile::from_bytes(bytes.clone());
    let reader = ReadOnlyDatabase::open_on(torn.clone(), &registry).unwrap();
    assert_eq!(observed(&reader.snapshot()), model);
    let report = reader.verify().unwrap();
    assert_eq!(report.tail, Tail::Torn { offset: end as u64 });
    assert_eq!(report.collections, 2);
    assert_eq!(
        (report.segments, report.documents, report.deleted),
        (2, 40, 0)
    );
    assert_eq!(report.replayed, 301);
    // Every backlog put was synced as its own chunk, after the last commit.
    assert_eq!(report.uncommitted_frames, 301);
    drop(reader);
    assert_eq!(torn.mutations(), 0);
    assert_eq!(torn.bytes(), bytes);

    // Non-zero bytes after it: corruption, read up to by a read-only open, failed by `verify`.
    bytes[end + 100] = 0x5a;
    let corrupt = SimFile::from_bytes(bytes.clone());
    let reader = ReadOnlyDatabase::open_on(corrupt.clone(), &registry).unwrap();
    assert_eq!(observed(&reader.snapshot()), model);
    let error = reader.verify().unwrap_err();
    assert!(
        matches!(error, DatabaseError::CorruptTail { offset } if offset == end as u64),
        "{error}"
    );
    assert!(error.is_corruption());
    assert_eq!(corrupt.mutations(), 0);
    assert_eq!(corrupt.bytes(), bytes);
}

#[test]
fn verify_counts_deletions_after_a_commit() {
    let (file, model) = backlog();
    let mut db = Database::open_on(file.clone(), options(BIG)).unwrap();
    db.commit().unwrap();
    drop(db);
    let reader = ReadOnlyDatabase::open_on(file, &AnalyzerRegistry::builtin()).unwrap();
    let report = reader.verify().unwrap();
    assert_eq!((report.replayed, report.uncommitted_frames), (0, 0));
    assert_eq!(report.tail, Tail::Clean);
    // The base segments lost every id the backlog touched; the new ones hold the backlog's live documents.
    assert_eq!(report.segments, 4);
    let live: usize = model.values().map(BTreeMap::len).sum();
    assert_eq!(report.documents - report.deleted, live as u64);
}

#[test]
fn a_read_write_open_of_a_read_only_container_is_refused() {
    let (file, _) = backlog();
    let container = persia_storage::container::Container::open_on(
        file,
        persia_storage::container::Mode::ReadOnly,
    )
    .unwrap();
    assert!(matches!(
        Database::open_container(container, options(BIG)),
        Err(DatabaseError::Container(ContainerError::ReadOnly {
            reason: ReadOnlyReason::OpenedReadOnly
        }))
    ));
}

#[test]
fn a_backlog_past_the_size_threshold_is_flushed_during_replay() {
    let (file, model) = backlog();
    let base = Database::open_on(file.clone(), options(BIG)).unwrap().seq();
    let small = 16 << 10;

    let db = Database::open_on(file.clone(), options(small)).unwrap();
    assert_eq!(observed(&db.snapshot()), model);
    let commits = db.seq() - base;
    assert!(commits >= 2, "{commits} commits during replay");
    assert!(db.manifest().wal_start() > 0);
    drop(db);

    // Losing nothing or every unsynced write: the next open replays from the last flush and does not commit again.
    for loss in [CrashLoss::All, CrashLoss::Nothing] {
        let again = file.crash(loss);
        let db = Database::open_on(again.clone(), options(small)).unwrap();
        assert_eq!(observed(&db.snapshot()), model, "{loss:?}");
        assert_eq!(db.seq(), base + commits, "{loss:?}");
        let reader = ReadOnlyDatabase::open_on(again, &AnalyzerRegistry::builtin()).unwrap();
        assert_eq!(observed(&reader.snapshot()), model, "{loss:?}");
        reader.verify().unwrap();
    }
}

#[test]
fn a_crash_while_replay_flushes_reopens_to_the_same_documents() {
    let (file, model) = backlog();
    let bytes = file.bytes();
    let small = 16 << 10;
    let total = {
        let file = SimFile::from_bytes(bytes.clone());
        let start = file.mutations();
        Database::open_on(file.clone(), options(small)).unwrap();
        file.mutations() - start
    };
    assert!(total > 10, "{total} mutating calls");
    for point in 0..total {
        for apply_first in [false, true] {
            let file = SimFile::from_bytes(bytes.clone());
            let at = file.mutations() + point;
            file.set_fault(Some(if apply_first {
                SimFault::ApplyThenFail(at)
            } else {
                SimFault::FailAt(at)
            }));
            assert!(
                Database::open_on(file.clone(), options(small)).is_err(),
                "call {point}: the fault did not fail the open"
            );
            // Out-of-order sector loss (`CrashLoss::Seeded`) is the container's quarantine path, tested in
            // `database_crash.rs`.
            for loss in [CrashLoss::All, CrashLoss::Nothing] {
                let ctx = format!("call {point}, apply first {apply_first}, {loss:?}");
                let crashed = file.crash(loss);
                let db = Database::open_on(crashed.clone(), options(small))
                    .unwrap_or_else(|e| panic!("{ctx}: {e}"));
                assert_eq!(observed(&db.snapshot()), model, "{ctx}");
                drop(db);
                let db = Database::open_on(crashed.crash(CrashLoss::All), options(BIG))
                    .unwrap_or_else(|e| panic!("{ctx}: second reopen: {e}"));
                assert_eq!(observed(&db.snapshot()), model, "{ctx}: second reopen");
            }
        }
    }
}

#[test]
fn replayed_records_hold_wal_start_until_a_flush() {
    let (file, model) = backlog();
    for memtable_bytes in [BIG, 16 << 10] {
        let crashed = file.crash(CrashLoss::All);
        let mut db = Database::open_on(crashed.clone(), options(memtable_bytes)).unwrap();
        // A commit that flushes nothing must keep the replayed records needed.
        db.create_collection(schema("c")).unwrap();
        drop(db);
        let db = Database::open_on(crashed.crash(CrashLoss::All), options(BIG)).unwrap();
        let mut expected = model.clone();
        expected.insert("c".to_owned(), BTreeMap::new());
        assert_eq!(observed(&db.snapshot()), expected, "{memtable_bytes}");
    }
}

fn put(db: &mut Database<SimFile>, model: &mut Docs, c: &str, id: &str, text: &str) {
    db.put(c, &doc(id, text), Durability::Fsync).unwrap();
    model
        .get_mut(c)
        .unwrap()
        .insert(id.to_owned(), text.to_owned());
}

#[test]
fn verify_agrees_with_a_writer_on_a_stale_tail() {
    let (file, model) = backlog();
    let registry = AnalyzerRegistry::builtin();
    // A copy of an earlier frame after the last one: valid, but with a lower `frame_seq` (ADR-0013), as a crash
    // that loses a truncation can leave. A writer truncates it like a torn tail.
    let mut bytes = file.bytes();
    let end = bytes.len();
    let stale = {
        let reader = ReadOnlyDatabase::open_on(file.clone(), &registry).unwrap();
        reader.container().uncommitted_frames()[0]
    };
    let (from, len) = (
        usize::try_from(stale.offset).unwrap(),
        usize::try_from(stale.len).unwrap(),
    );
    let copy = bytes[from..from + len].to_vec();
    bytes.extend_from_slice(&copy);

    let reader = ReadOnlyDatabase::open_on(SimFile::from_bytes(bytes.clone()), &registry).unwrap();
    assert!(reader.container().recovery().stale_tail);
    assert_eq!(observed(&reader.snapshot()), model);
    let report = reader.verify().unwrap();
    assert_eq!(report.tail, Tail::Stale { offset: end as u64 });

    let written = SimFile::from_bytes(bytes);
    let db = Database::open_on(written.clone(), options(BIG)).unwrap();
    assert_eq!(observed(&db.snapshot()), model);
    drop(db);
    let report = ReadOnlyDatabase::open_on(written, &registry)
        .unwrap()
        .verify()
        .unwrap();
    assert_eq!(report.tail, Tail::Clean);
}

/// `open_container` accepts a read-write container. One whose open already quarantined a garbage tail and
/// truncated it no longer holds those bytes, so `verify` reports a clean end (ADR-0014).
#[test]
fn verify_reports_clean_after_a_read_write_open_quarantined_the_tail() {
    use persia_format::frame::ScanEnd;
    use persia_storage::container::{Container, Mode};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let registry = AnalyzerRegistry::builtin();
    let mut db = Database::create(&path, options(BIG)).unwrap();
    db.create_collection(schema("notes")).unwrap();
    db.put("notes", &doc("n1", "hello"), Durability::Fsync)
        .unwrap();
    drop(db);

    let mut raw = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    let end = raw.metadata().unwrap().len();
    io::Write::write_all(&mut raw, &[0x5a; 64]).unwrap();
    raw.sync_all().unwrap();
    drop(raw);

    let container = Container::open(&path, Mode::ReadWrite).unwrap();
    assert!(
        matches!(container.recovery().scan_end, ScanEnd::Corrupt { offset, .. } if offset == end),
        "{:?}",
        container.recovery()
    );
    assert!(container.recovery().quarantined.is_some());
    assert!(container.recovery().truncated_from.is_some());
    assert!(!container.recovery().stale_tail);

    let reader = ReadOnlyDatabase::open_container(container, &registry).unwrap();
    let report = reader.verify().unwrap();
    assert_eq!(report.tail, Tail::Clean);
    assert_eq!(observed(&reader.snapshot())["notes"]["n1"], "hello");
}
