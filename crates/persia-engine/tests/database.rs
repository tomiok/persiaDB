//! The write path of a database (ROADMAP 6.3; SPEC §4.5, §8.2, §8.7, §10.1): flush and commit, snapshot publication,
//! the commit policy, logging only what replay can apply, the container's object view and the checks on open.
//!
//! Invariants:
//! 1. A commit flushes every memtable with operations into one segment of its live documents and publishes it: the
//!    documents a snapshot shows are the same before and after, now from the segment (6.3.1, 6.3.2).
//! 2. Reopening (also after losing every unsynced write) shows the committed segments plus the replayed WAL: every
//!    acknowledged `Fsync` operation (SPEC §8.6).
//! 3. A reader thread that loads snapshots while the writer commits sees each document exactly once in every
//!    snapshot: in the memtable before the commit, in a segment after (6.3.3).
//! 4. `tick` commits exactly when the policy says: size, idle, and the interval for writes that never go idle; an
//!    explicit `commit` with nothing to flush keeps `seq` (6.3.4).
//! 5. An operation the memtable rejects, or one for a collection that is not committed, is not logged: no WAL frame
//!    is written and a reopen does not see it (6.3.9).
//! 6. The object view lists the manifest, the segments and the WAL chunks under the SPEC §10.4 keys, serves their
//!    payloads (and ranges of them), and refuses writes (6.3.6).
//! 7. Open fails as corruption when the manifest's `seq`, `fence_token` or `wal_start` differ from the slot's, when
//!    a frame location names a frame of another kind or length or one past `log_end`, or when it is an object
//!    location; `check_layout` rejects frame locations in the remote layout (6.3.7).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use persia_engine::db::{
    CommitPolicy, Database, DatabaseError, DatabaseOptions, ManualClock, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::manifest::{
    CollectionDiff, Layout, Location, Manifest, ManifestDiff, SegmentEntry, SegmentId,
};
use persia_engine::memtable::{FlushPolicy, IdState, MemtableError};
use persia_engine::schema::{CollectionSchema, FieldId, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::segment::SegmentError;
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_format::Corruption;
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFile};
use persia_storage::container::{Commit, Container, Mode};
use persia_storage::{ErrorKind, Key, KeyPrefix, Storage};

fn schema(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn body(schema: &CollectionSchema) -> FieldId {
    schema.field("body").unwrap().id()
}

fn doc(id: &str, text: &str) -> Document {
    Document::new(id).with(body(&schema("x")), Value::String(text.into()))
}

fn options(clock: &Arc<ManualClock>) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::clone(clock) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    }
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(1_700_000_000_000_000))
}

/// The document with id `id` as `snapshot` shows it: the memtable's version hides the segments', and newer segments
/// hide older ones (SPEC §8.3).
fn get(snapshot: &Snapshot, collection: &str, id: &str) -> Option<Document> {
    let c = snapshot.collection(collection)?;
    let memtable = c.memtable();
    match memtable.lookup(id) {
        Some(IdState::Live(docid)) => return Some(memtable.document(docid).unwrap()),
        Some(IdState::Deleted) => return None,
        None => {}
    }
    c.segments()
        .iter()
        .rev()
        .find_map(|s| s.document_by_id(id).unwrap().map(|(_, d)| d))
}

/// Every live document `snapshot` shows, by id.
fn documents(snapshot: &Snapshot, collection: &str) -> Vec<(String, Document)> {
    let c = snapshot.collection(collection).unwrap();
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
    ids.into_iter()
        .filter_map(|id| get(snapshot, collection, &id).map(|d| (id, d)))
        .collect()
}

fn wal_frames(c: &Container<SimFile>) -> usize {
    c.frames()
        .iter()
        .filter(|f| f.kind == FrameKind::WalChunk)
        .count()
}

#[test]
fn commit_flushes_into_segments_and_reopen_recovers() {
    let clock = clock();
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [1; 16], options(&clock)).unwrap();
    assert_eq!(db.seq(), 1);
    assert_eq!(db.create_collection(schema("a")).unwrap(), 2);
    assert_eq!(db.create_collection(schema("b")).unwrap(), 3);
    assert!(matches!(
        db.create_collection(schema("a")),
        Err(DatabaseError::CollectionExists { .. })
    ));
    for i in 0..10 {
        db.put(
            "a",
            &doc(&format!("a{i}"), &format!("alpha {i}")),
            Durability::Fsync,
        )
        .unwrap();
    }
    db.put("a", &doc("a3", "replaced"), Durability::Fsync)
        .unwrap();
    db.delete("a", "a4", Durability::Fsync).unwrap();
    let before = documents(&db.snapshot(), "a");
    assert_eq!(before.len(), 9);

    // `b` has no operations: only `a` gets a segment.
    assert_eq!(db.commit().unwrap(), 4);
    let after = db.snapshot();
    assert_eq!(after.seq(), 4);
    let a = after.collection("a").unwrap();
    assert_eq!((a.segments().len(), a.memtable().live_count()), (1, 0));
    assert_eq!(a.segments()[0].doc_count(), 9);
    assert!(after.collection("b").unwrap().segments().is_empty());
    assert_eq!(documents(&after, "a"), before);
    assert_eq!(get(&after, "a", "a3").unwrap(), doc("a3", "replaced"));
    // Every record is flushed: the WAL starts at the Manifest frame.
    assert_eq!(
        db.manifest().wal_start(),
        db.container().superblock().manifest_off
    );
    // Nothing to flush: the same seq, no new frame.
    let end = db.container().end();
    assert_eq!(db.commit().unwrap(), 4);
    assert_eq!(db.container().end(), end);

    // Writes after the commit live in the WAL only.
    db.put("a", &doc("a10", "after"), Durability::Fsync)
        .unwrap();
    db.put("b", &doc("b0", "bravo"), Durability::Fsync).unwrap();
    db.put("a", &doc("a0", "new zero"), Durability::Fsync)
        .unwrap();
    let expected_a = documents(&db.snapshot(), "a");
    let expected_b = documents(&db.snapshot(), "b");
    drop(db);

    for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(7)] {
        let db = Database::open_on(file.crash(loss), options(&clock)).unwrap();
        let snapshot = db.snapshot();
        assert_eq!(snapshot.seq(), 4);
        assert_eq!(documents(&snapshot, "a"), expected_a, "{loss:?}");
        assert_eq!(documents(&snapshot, "b"), expected_b, "{loss:?}");
        assert_eq!(get(&snapshot, "a", "a0").unwrap(), doc("a0", "new zero"));
    }

    // A reopened database flushes the replayed records, and wal_start moves past them.
    let mut db = Database::open_on(file.crash(CrashLoss::All), options(&clock)).unwrap();
    assert!(db.manifest().wal_start() < db.container().end());
    assert_eq!(db.commit().unwrap(), 5);
    assert_eq!(
        db.manifest().wal_start(),
        db.container().superblock().manifest_off
    );
    assert_eq!(db.snapshot().collection("a").unwrap().segments().len(), 2);
    assert_eq!(documents(&db.snapshot(), "a"), expected_a);
    drop(db);
    let db = Database::open_on(file.crash(CrashLoss::All), options(&clock)).unwrap();
    assert_eq!(documents(&db.snapshot(), "a"), expected_a);
    assert_eq!(documents(&db.snapshot(), "b"), expected_b);
}

#[test]
fn readers_see_each_document_once_across_commits() {
    let clock = clock();
    let mut db = Database::create_on(SimFile::new(), [2; 16], options(&clock)).unwrap();
    db.create_collection(schema("a")).unwrap();
    let reader = db.reader();
    // Documents whose put returned, and whose put started.
    let applied = Arc::new(AtomicU64::new(0));
    let started = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let checker = {
        let (applied, started, stop) = (
            Arc::clone(&applied),
            Arc::clone(&started),
            Arc::clone(&stop),
        );
        std::thread::spawn(move || {
            let mut seen = BTreeSet::new();
            while !stop.load(Ordering::SeqCst) {
                let low = applied.load(Ordering::SeqCst);
                let snapshot = reader.snapshot();
                let a = snapshot.collection("a").unwrap();
                let in_segments: u32 = a.segments().iter().map(|s| s.doc_count()).sum();
                let total = u64::from(in_segments + a.memtable().live_count());
                let high = started.load(Ordering::SeqCst);
                // Ids are unique and never deleted: a snapshot holding both a flushed memtable and its segment
                // would count documents twice, one holding neither would miss them.
                assert!(
                    (low..=high).contains(&total),
                    "seq {}: {total} documents, {low}..={high} applied",
                    snapshot.seq()
                );
                seen.insert(snapshot.seq());
            }
            seen
        })
    };
    for round in 0..20 {
        let docs: Vec<Document> = (0..50)
            .map(|i| doc(&format!("{round}-{i}"), "text"))
            .collect();
        started.fetch_add(50, Ordering::SeqCst);
        db.put_batch("a", &docs, Durability::Buffered).unwrap();
        applied.fetch_add(50, Ordering::SeqCst);
        db.commit().unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    let seen = checker.join().unwrap();
    assert!(!seen.is_empty());
    assert_eq!(db.reader().seq(), db.seq());
    let a = db.snapshot();
    assert_eq!(a.collection("a").unwrap().segments().len(), 20);
}

#[test]
fn a_held_snapshot_keeps_its_state() {
    let clock = clock();
    let mut db = Database::create_on(SimFile::new(), [3; 16], options(&clock)).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("x", "one"), Durability::Fsync).unwrap();
    let old = db.snapshot();
    db.commit().unwrap();
    db.put("a", &doc("y", "two"), Durability::Fsync).unwrap();
    // The old snapshot still has the flushed memtable, which takes no more writes.
    assert_eq!(old.seq(), 2);
    assert_eq!(documents(&old, "a").len(), 1);
    assert!(old.collection("a").unwrap().segments().is_empty());
    let new = db.snapshot();
    assert_eq!(new.seq(), 3);
    assert_eq!(documents(&new, "a").len(), 2);
}

#[test]
fn tick_follows_the_commit_policy() {
    let clock = clock();
    let policy = CommitPolicy {
        flush: FlushPolicy {
            memtable_bytes: 1 << 20,
            idle: Duration::from_secs(10),
        },
        interval: Duration::from_secs(30),
    };
    let opts = DatabaseOptions {
        policy,
        ..options(&clock)
    };
    let mut db = Database::create_on(SimFile::new(), [4; 16], opts).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.create_collection(schema("b")).unwrap();
    assert_eq!(db.tick().unwrap(), None);
    clock.advance(Duration::from_secs(100));
    assert_eq!(db.tick().unwrap(), None, "empty memtables are never due");
    // Any commit restarts the interval.
    db.create_collection(schema("c")).unwrap();
    let seq = db.seq();

    // Idle: `a` stops at t, `b` keeps being written.
    db.put("a", &doc("a1", "x"), Durability::Buffered).unwrap();
    db.put("b", &doc("b1", "x"), Durability::Buffered).unwrap();
    clock.advance(Duration::from_secs(9));
    db.put("b", &doc("b2", "x"), Durability::Buffered).unwrap();
    assert_eq!(db.tick().unwrap(), None);
    clock.advance(Duration::from_secs(1));
    assert_eq!(db.tick().unwrap(), Some(seq + 1));
    let s = db.snapshot();
    assert_eq!(s.collection("a").unwrap().segments().len(), 1);
    assert!(s.collection("b").unwrap().segments().is_empty());

    // Interval: `b` is written every 5 s, never idle for 10 s, and is flushed 30 s after the last commit.
    for _ in 0..5 {
        clock.advance(Duration::from_secs(5));
        db.put("b", &doc("b3", "x"), Durability::Buffered).unwrap();
        assert_eq!(db.tick().unwrap(), None);
    }
    clock.advance(Duration::from_secs(5));
    db.put("b", &doc("b4", "x"), Durability::Buffered).unwrap();
    assert_eq!(db.tick().unwrap(), Some(seq + 2));
    assert_eq!(db.snapshot().collection("b").unwrap().segments().len(), 1);
    assert_eq!(db.tick().unwrap(), None);
}

#[test]
fn a_full_memtable_is_flushed_before_the_next_write() {
    let clock = clock();
    let opts = DatabaseOptions {
        policy: CommitPolicy {
            flush: FlushPolicy {
                memtable_bytes: 64 << 10,
                idle: Duration::from_secs(10),
            },
            interval: Duration::from_secs(3600),
        },
        ..options(&clock)
    };
    let mut db = Database::create_on(SimFile::new(), [5; 16], opts).unwrap();
    db.create_collection(schema("a")).unwrap();
    let text = "lorem ipsum dolor sit amet ".repeat(40);
    for i in 0..200 {
        db.put("a", &doc(&format!("d{i}"), &text), Durability::Buffered)
            .unwrap();
    }
    let s = db.snapshot();
    let a = s.collection("a").unwrap();
    assert!(a.segments().len() >= 2, "{} segments", a.segments().len());
    let total: u32 = a.segments().iter().map(|s| s.doc_count()).sum();
    assert_eq!(total + a.memtable().live_count(), 200);
}

#[test]
fn rejected_operations_are_never_logged() {
    let clock = clock();
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [6; 16], options(&clock)).unwrap();
    db.create_collection(schema("a")).unwrap();
    let frames = wal_frames(db.container());
    let end = db.container().end();

    let bad = Document::new("bad").with(body(&schema("a")), Value::I64(1));
    assert!(matches!(
        db.put("a", &bad, Durability::Fsync),
        Err(DatabaseError::Memtable(MemtableError::Segment(
            SegmentError::Document(_)
        )))
    ));
    assert!(matches!(
        db.put("nope", &doc("x", "y"), Durability::Fsync),
        Err(DatabaseError::NoSuchCollection { .. })
    ));
    assert!(matches!(
        db.delete("a", "", Durability::Fsync),
        Err(DatabaseError::Memtable(_))
    ));
    assert_eq!(
        (wal_frames(db.container()), db.container().end()),
        (frames, end)
    );
    // A batch: the documents before the rejected one are applied and durable, the rest are not.
    let batch = [doc("ok1", "x"), bad.clone(), doc("ok2", "x")];
    assert!(matches!(
        db.put_batch("a", &batch, Durability::Fsync),
        Err(DatabaseError::BatchRejected { index: 1, .. })
    ));
    // The database is still usable.
    db.put("a", &doc("ok3", "x"), Durability::Fsync).unwrap();
    drop(db);
    let db = Database::open_on(file.crash(CrashLoss::All), options(&clock)).unwrap();
    let ids: Vec<String> = documents(&db.snapshot(), "a")
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids, ["ok1", "ok3"]);
}

#[test]
fn objects_view_names_the_committed_frames() {
    let clock = clock();
    let opts = DatabaseOptions {
        wal_chunk_target: 64,
        ..options(&clock)
    };
    let mut db = Database::create_on(SimFile::new(), [7; 16], opts).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("x", "one"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("a", &doc("y", "two"), Durability::Fsync).unwrap();
    db.create_collection(schema("b")).unwrap(); // commits the chunk holding `y`

    let objects = db.objects();
    let keys: Vec<String> = objects
        .list(&KeyPrefix::new("").unwrap())
        .unwrap()
        .into_iter()
        .map(|m| m.key.as_str().to_owned())
        .collect();
    let entry = *db
        .manifest()
        .collection("a")
        .unwrap()
        .segments()
        .first()
        .unwrap();
    let segment = format!("segments/a/{}.seg", entry.id);
    assert_eq!(keys.len(), 3, "{keys:?}");
    assert_eq!(keys[0], "manifests/00000000000000000004.mf");
    assert_eq!(keys[1], segment);
    assert!(
        keys[2].starts_with("wal/")
            && std::path::Path::new(&keys[2]).extension() == Some("wal".as_ref())
    );

    let manifest = objects.get(&Key::new(keys[0].as_str()).unwrap()).unwrap();
    assert_eq!(&Manifest::decode(&manifest).unwrap(), db.manifest());
    let key = Key::new(segment).unwrap();
    let whole = objects.get(&key).unwrap();
    assert_eq!(whole.len() as u64, entry.size);
    assert_eq!(&whole[whole.len() - 4..], b"PSEG");
    let range = objects.get_range(&key, 8..20).unwrap();
    assert_eq!((range.base_offset(), &range[..]), (8, &whole[8..20]));
    let past = objects.get_range(&key, 0..entry.size + 1).unwrap_err();
    assert_eq!(past.kind(), ErrorKind::Corrupt);
    let listed = objects
        .list(&KeyPrefix::new("segments/a/").unwrap())
        .unwrap();
    assert_eq!((listed.len(), listed[0].size), (1, entry.size));
    assert_eq!(
        objects
            .list(&KeyPrefix::new("tombstones/").unwrap())
            .unwrap(),
        []
    );
    let missing = Key::new("segments/a/0.seg").unwrap();
    assert_eq!(
        objects.get(&missing).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    for err in [
        objects.put(&key, Vec::new().into()).unwrap_err(),
        objects.put_if_absent(&key, Vec::new().into()).unwrap_err(),
        objects.delete(&key).unwrap_err(),
    ] {
        assert_eq!(err.kind(), ErrorKind::Unsupported);
    }
    assert_eq!(
        objects.capabilities(),
        persia_storage::Capabilities::default()
    );
}

/// A container whose committed manifest is `manifest`, after `frames` (kind, payload) appended before it.
fn container_with(
    frames: &[(FrameKind, &[u8])],
    manifest: impl FnOnce(&[u64]) -> Manifest,
) -> SimFile {
    let file = SimFile::new();
    let mut c = Container::create_on(file.clone(), [9; 16]).unwrap();
    let mut at = Vec::new();
    for (kind, payload) in frames {
        let f = c.append(*kind, payload, Compression::None).unwrap();
        at.push(f.offset);
        at.push(f.len);
    }
    c.sync().unwrap();
    let wal_start = c.end();
    let m = manifest(&at);
    let frame = c
        .append(FrameKind::Manifest, &m.encode().unwrap(), Compression::None)
        .unwrap();
    c.commit(Commit {
        manifest: frame,
        wal_start,
        fence_token: m.fence_token(),
    })
    .unwrap();
    file
}

/// The manifest of one collection `a` with one segment at `location`, `wal_start` and `fence_token` as given.
fn manifest_with(location: Location, size: u64, wal_start: u64, fence_token: u64) -> Manifest {
    let entry = SegmentEntry {
        id: SegmentId::new(1),
        doc_count: 1,
        size,
        location,
        tombstones: None,
    };
    let mut diff = ManifestDiff {
        wal_start: Some(wal_start),
        fence_token: Some(fence_token),
        ..ManifestDiff::default()
    };
    diff.collections.insert(
        "a".into(),
        CollectionDiff::Create {
            schema: schema("a"),
            segments: vec![entry],
        },
    );
    Manifest::empty().apply(&diff).unwrap()
}

/// Where a test puts a location, given the offsets and lengths of the frames before the manifest.
type At = Box<dyn Fn(&[u64]) -> Location>;

fn open_err(file: SimFile) -> DatabaseError {
    let clock = clock();
    Database::open_on(file, options(&clock)).unwrap_err()
}

#[test]
fn open_checks_the_manifest_against_the_container() {
    // A real segment frame, to point locations at.
    let clock = clock();
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [8; 16], options(&clock)).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("x", "one"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    let entry = *db
        .manifest()
        .collection("a")
        .unwrap()
        .segments()
        .first()
        .unwrap();
    let Location::Frame { offset, len } = entry.location else {
        panic!()
    };
    let segment = db
        .container()
        .read_frame(offset, usize::MAX)
        .unwrap()
        .payload;
    drop(db);
    Database::open_on(file, options(&clock)).unwrap();

    let frames: &[(FrameKind, &[u8])] =
        &[(FrameKind::Segment, &segment), (FrameKind::WalChunk, b"x")];
    let wal_start_of = |at: &[u64]| at[2] + at[3];
    let size = segment.len() as u64;

    // Consistent: opens (the segment's id is not 1, so it fails only on its footer).
    let ok = container_with(frames, |at| {
        manifest_with(
            Location::Frame {
                offset: at[0],
                len: at[1],
            },
            size,
            wal_start_of(at),
            0,
        )
    });
    assert!(matches!(
        open_err(ok),
        DatabaseError::SegmentMismatch {
            field: "segment_id",
            ..
        }
    ));
    // wal_start and fence_token differ from the slot's.
    let wal = container_with(frames, |at| {
        manifest_with(
            Location::Frame {
                offset: at[0],
                len: at[1],
            },
            size,
            at[0],
            0,
        )
    });
    let e = open_err(wal);
    assert!(e.is_corruption());
    assert!(
        matches!(
            e,
            DatabaseError::SlotMismatch {
                field: "wal_start",
                ..
            }
        ),
        "{e}"
    );
    // The slot carries the manifest's fence token; break `seq` instead by committing a stale manifest.
    let seq = SimFile::new();
    {
        let mut c = Container::create_on(seq.clone(), [9; 16]).unwrap();
        let m = Manifest::empty()
            .apply(&ManifestDiff::default())
            .unwrap()
            .apply(&ManifestDiff {
                wal_start: Some(c.end()),
                ..ManifestDiff::default()
            })
            .unwrap(); // seq 2, committed as the container's first commit
        let frame = c
            .append(FrameKind::Manifest, &m.encode().unwrap(), Compression::None)
            .unwrap();
        c.commit(Commit {
            manifest: frame,
            wal_start: m.wal_start(),
            fence_token: 0,
        })
        .unwrap();
    }
    assert!(matches!(
        open_err(seq),
        DatabaseError::SlotMismatch { field: "seq", .. }
    ));

    // A location naming the WAL chunk, a wrong length, past `log_end`, and an object location.
    let cases: [(&str, u8, At); 3] = [
        (
            "wal chunk",
            1,
            Box::new(|at| Location::Frame {
                offset: at[2],
                len: at[3],
            }),
        ),
        (
            "length",
            2,
            Box::new(|at| Location::Frame {
                offset: at[0],
                len: at[1] + 8,
            }),
        ),
        (
            "past log_end",
            0,
            Box::new(|at| Location::Frame {
                offset: at[2] + at[3] + 4096,
                len: at[1],
            }),
        ),
    ];
    for (what, found, location) in cases {
        let file = container_with(frames, |at| {
            manifest_with(location(at), size, wal_start_of(at), 0)
        });
        let e = open_err(file);
        assert!(e.is_corruption(), "{what}: {e}");
        assert!(
            matches!(&e, DatabaseError::Storage(s) if s.kind() == ErrorKind::Corrupt),
            "{what}: {e}"
        );
        let DatabaseError::Storage(persia_storage::Error::Corrupt { source, .. }) = &e else {
            panic!("{what}: {e}");
        };
        assert!(
            matches!(
                source.corruption(),
                Some((_, Corruption::WrongFrame { expected_kind: 2, found_kind, .. })) if found_kind == found
            ),
            "{what}: {e}"
        );
    }
    let object = container_with(frames, |at| {
        manifest_with(Location::Object, size, wal_start_of(at), 0)
    });
    assert!(matches!(open_err(object), DatabaseError::Layout(_)));

    // The remote layout rejects frame locations.
    let local = manifest_with(Location::Frame { offset, len }, size, 0, 0);
    assert!(local.check_layout(Layout::Container).is_ok());
    let e = local.check_layout(Layout::Objects).unwrap_err();
    assert_eq!((e.collection.as_str(), e.segment), ("a", SegmentId::new(1)));
    assert!(
        manifest_with(Location::Object, size, 0, 0)
            .check_layout(Layout::Objects)
            .is_ok()
    );
}

#[test]
fn a_poisoned_database_takes_no_more_writes() {
    use persia_storage::container::sim::SimFault;
    let clock = clock();
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [10; 16], options(&clock)).unwrap();
    db.create_collection(schema("a")).unwrap();
    file.set_fault(Some(SimFault::FailAt(file.mutations())));
    assert!(matches!(
        db.put("a", &doc("x", "y"), Durability::Fsync),
        Err(DatabaseError::Wal(_))
    ));
    file.set_fault(None);
    for result in [
        db.put("a", &doc("z", "y"), Durability::Buffered),
        db.commit().map(drop),
        db.tick().map(drop),
        db.create_collection(schema("b")).map(drop),
    ] {
        assert!(matches!(result, Err(DatabaseError::Poisoned)), "{result:?}");
    }
    // The failed `Fsync` put was never made visible.
    assert_eq!(
        db.snapshot()
            .collection("a")
            .unwrap()
            .memtable()
            .live_count(),
        0
    );
}

#[test]
fn opens_a_container_file_by_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("x", "one"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("a", &doc("y", "two"), Durability::Fsync).unwrap();
    assert!(matches!(
        Database::open(&path, DatabaseOptions::default()),
        Err(DatabaseError::Container(
            persia_storage::container::Error::Locked
        ))
    ));
    drop(db);
    let db = Database::open(&path, DatabaseOptions::default()).unwrap();
    assert_eq!(documents(&db.snapshot(), "a").len(), 2);
    let c = Container::open(&path, Mode::ReadOnly);
    assert!(c.is_ok());
}

#[test]
fn a_batch_takes_one_sync() {
    let clock = clock();
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [11; 16], options(&clock)).unwrap();
    db.create_collection(schema("a")).unwrap();
    let docs: Vec<Document> = (0..20).map(|i| doc(&format!("d{i}"), "text")).collect();
    let before = file.mutations();
    db.put_batch("a", &docs, Durability::Fsync).unwrap();
    // One chunk written, one sync (SPEC §8.2.1 group commit).
    assert_eq!(file.mutations() - before, 2);
    let before = file.mutations();
    db.put("a", &doc("x", "y"), Durability::Buffered).unwrap();
    db.delete("a", "d0", Durability::Buffered).unwrap();
    assert_eq!(file.mutations(), before);
    db.put("a", &doc("y", "y"), Durability::Async).unwrap();
    assert_eq!(file.mutations() - before, 1);
}
