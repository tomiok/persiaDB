//! Replay at open, against a model (ROADMAP 6.6.1, 6.6.2, 6.6.3, 6.6.5; SPEC §4.4, §8.2.1, §8.3, §8.6, ADR-0033).
//!
//! Invariants:
//! 1. Flushes during replay (6.6.5): a synced, uncommitted backlog of puts, upserts, deletes and batches over three
//!    collections, written with any WAL chunk size (so `Buffered` operations and batches share multi-record chunks
//!    and a replay flush lands in the middle of a chunk), reopens with a small memtable threshold to exactly the
//!    documents of a `BTreeMap` model, losing every unsynced write or none. A second open with another threshold
//!    shows the same documents, and so does one after a commit that flushes nothing (replayed records still hold
//!    `wal_start` back). Every stage passes `ReadOnlyDatabase::verify`, and no id is live in two segments (§8.3).
//! 2. A flush in the middle of a multi-record chunk keeps `wal_start` at that chunk: the next open replays the rest
//!    of it (SPEC §8.6 "its `wal_start` is at most the start of the chunk being replayed").
//! 3. Read-only replay equals read-write replay (6.6.2): on the same crashed bytes (frames after `log_end` included,
//!    with mixed `Buffered`/`Async`/`Fsync` operations and commits, a torn tail, or the file cut at any offset after
//!    `log_end`), a read-only open, a read-write open that does not flush and one that flushes show the same
//!    documents, and that is the model's state after a prefix of the operations holding every acknowledged one.
//! 4. `verify` fails as corruption (6.6.3): on a segment whose damage `Segment::open` misses but `Segment::verify`
//!    finds, behind a valid frame CRC (`OpenSegment`), and on an id live in two committed segments (`LiveTwice`);
//!    the same container with the second copy gone passes.
//!
//! Seeds: `PROPTEST_RNG_SEED` (default fixed below) and `PROPTEST_CASES`; a failure prints the seed it ran with.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::needless_pass_by_value,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    CommitPolicy, Database, DatabaseError, DatabaseOptions, ManualClock, ReadOnlyDatabase,
    SegmentIds, Snapshot, Tail,
};
use persia_engine::document::{Document, Value};
use persia_engine::manifest::{CollectionDiff, Location, Manifest, ManifestDiff, SegmentEntry};
use persia_engine::memtable::FlushPolicy;
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::segment::Segment;
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_format::ByteSource;
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFile};
use persia_storage::container::{Commit, Container};
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestCaseError, TestRunner};

const COLLECTIONS: [&str; 3] = ["a", "b", "c"];
const WORDS: [&str; 6] = ["alpha", "beta", "gamma", "delta", "common", "omega"];
const IDS: u8 = 12;
const BIG: usize = 1 << 30;

/// The fixed proptest seed; `PROPTEST_RNG_SEED` overrides it.
const PROPTEST_SEED: u64 = 0x0DB6_6005_0001;

/// Documents by collection and id, with their body.
type Docs = BTreeMap<String, BTreeMap<String, String>>;

/// Runs `test` on `cases` values of `strategy` (or `PROPTEST_CASES`) with a fixed seed, persisting failures next to
/// this file, and panics with the seed and the minimal failing input.
fn check<S>(
    name: &'static str,
    cases: u32,
    strategy: S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) where
    S: Strategy,
    S::Value: Debug,
{
    let seed = std::env::var("PROPTEST_RNG_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(PROPTEST_SEED);
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(cases);
    let config = ProptestConfig {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        source_file: Some(file!()),
        test_name: Some(name),
        ..ProptestConfig::default()
    };
    let mut runner = TestRunner::new(config);
    if let Err(e) = runner.run(&strategy, test) {
        panic!("{name} failed with PROPTEST_RNG_SEED={seed}: {e}\n{runner}");
    }
}

fn cases(normal: u32, slow: u32) -> u32 {
    if cfg!(feature = "slow") { slow } else { normal }
}

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

fn options(memtable_bytes: usize, wal_chunk_target: usize) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        // The same seed on every open: the ids drawn collide with committed ones, which the database must skip.
        segment_ids: SegmentIds::seeded(3),
        policy: CommitPolicy {
            flush: FlushPolicy {
                memtable_bytes,
                ..FlushPolicy::default()
            },
            ..CommitPolicy::default()
        },
        wal_chunk_target,
        ..DatabaseOptions::default()
    }
}

fn registry() -> AnalyzerRegistry {
    AnalyzerRegistry::builtin()
}

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

/// SPEC §8.3: no id is live in two segments of a collection.
fn one_live_copy(snapshot: &Snapshot) -> Result<(), TestCaseError> {
    for (name, c) in snapshot.collections() {
        let mut seen = BTreeSet::new();
        for view in c.segment_views() {
            for docid in 0..view.doc_count() {
                if view.is_live(docid) {
                    let id = view.segment().id_of(docid).unwrap();
                    prop_assert!(seen.insert(id.clone()), "{}: {} is live twice", name, id);
                }
            }
        }
    }
    Ok(())
}

fn empty_docs() -> Docs {
    COLLECTIONS
        .iter()
        .map(|c| ((*c).to_owned(), BTreeMap::new()))
        .collect()
}

/// A new database on `file` with every collection committed.
fn create(file: &SimFile, wal_chunk_target: usize) -> Database<SimFile> {
    let mut db =
        Database::create_on(file.clone(), [0x66; 16], options(BIG, wal_chunk_target)).unwrap();
    for c in COLLECTIONS {
        db.create_collection(schema(c)).unwrap();
    }
    db
}

// ---- Operations ----

#[derive(Debug, Clone)]
enum Op {
    Put {
        c: usize,
        id: u8,
        text: Text,
        durability: Durability,
    },
    Delete {
        c: usize,
        id: u8,
        durability: Durability,
    },
    /// `put_batch`: every document but the last is logged `Buffered`, so the batch shares chunks.
    Batch {
        c: usize,
        docs: Vec<(u8, Text)>,
        durability: Durability,
    },
    Commit,
}

/// A body: some words and a filler of `pad` repetitions, so memtables grow at different rates.
#[derive(Debug, Clone)]
struct Text {
    words: Vec<u8>,
    pad: u8,
}

impl Text {
    fn render(&self, version: usize) -> String {
        let mut out: Vec<String> = self
            .words
            .iter()
            .map(|&w| WORDS[w as usize].to_owned())
            .collect();
        for i in 0..self.pad {
            out.push(format!("filler{i}"));
        }
        out.push(format!("v{version}"));
        out.join(" ")
    }
}

fn text() -> impl Strategy<Value = Text> {
    (prop::collection::vec(0..WORDS.len() as u8, 1..6), 0..24u8)
        .prop_map(|(words, pad)| Text { words, pad })
}

fn durability(buffered_heavy: bool) -> BoxedStrategy<Durability> {
    if buffered_heavy {
        prop_oneof![
            3 => Just(Durability::Buffered),
            1 => Just(Durability::Async),
            2 => Just(Durability::Fsync),
        ]
        .boxed()
    } else {
        prop_oneof![
            2 => Just(Durability::Buffered),
            2 => Just(Durability::Async),
            1 => Just(Durability::Fsync),
        ]
        .boxed()
    }
}

/// Operations; `commits` allows `Commit` (rarely, so most of the sequence stays a backlog).
fn ops(len: std::ops::Range<usize>, commits: bool, buffered_heavy: bool) -> BoxedStrategy<Vec<Op>> {
    let c = 0..COLLECTIONS.len();
    let op = prop_oneof![
        6 => (c.clone(), 0..IDS, text(), durability(buffered_heavy))
            .prop_map(|(c, id, text, durability)| Op::Put { c, id, text, durability }),
        2 => (c.clone(), 0..IDS, durability(buffered_heavy))
            .prop_map(|(c, id, durability)| Op::Delete { c, id, durability }),
        2 => (c, prop::collection::vec((0..IDS, text()), 1..24), durability(buffered_heavy))
            .prop_map(|(c, docs, durability)| Op::Batch { c, docs, durability }),
        u32::from(commits) => Just(Op::Commit),
    ];
    prop::collection::vec(op, len).boxed()
}

fn chunk_target() -> impl Strategy<Value = usize> {
    // Tiny chunks rotate inside batches; the default holds a whole backlog in a few chunks.
    prop_oneof![Just(200usize), Just(2048), Just(16 << 10), Just(1 << 20)]
}

/// The model after every logged record (index `i` = after `i` records), and per record whether it was
/// acknowledged synced (`Fsync`, or a commit after it) or written out (`Async`), as the floor a crash may not go
/// below.
struct Run {
    states: Vec<Docs>,
    /// Records acknowledged on disk (`Fsync`, commit): a crash losing every unsynced write keeps at least these.
    synced: usize,
    /// Records acknowledged written (`Async` or more): a process crash keeps at least these.
    written: usize,
}

/// Applies `ops` to `db` and the model, recording the model after each record.
fn write(db: &mut Database<SimFile>, ops: &[Op]) -> Run {
    let mut model = empty_docs();
    let mut run = Run {
        states: vec![model.clone()],
        synced: 0,
        written: 0,
    };
    let mut version = 0usize;
    let ack = |run: &mut Run, d: Durability| {
        let n = run.states.len() - 1;
        match d {
            Durability::Buffered => {}
            Durability::Async => run.written = n,
            Durability::Fsync => {
                run.written = n;
                run.synced = n;
            }
        }
    };
    for op in ops {
        match op {
            Op::Put {
                c,
                id,
                text,
                durability,
            } => {
                let (name, id) = (COLLECTIONS[*c], format!("d{id}"));
                version += 1;
                let body = text.render(version);
                db.put(name, &doc(&id, &body), *durability).unwrap();
                model.get_mut(name).unwrap().insert(id, body);
                run.states.push(model.clone());
                ack(&mut run, *durability);
            }
            Op::Delete { c, id, durability } => {
                let (name, id) = (COLLECTIONS[*c], format!("d{id}"));
                db.delete(name, &id, *durability).unwrap();
                model.get_mut(name).unwrap().remove(&id);
                run.states.push(model.clone());
                ack(&mut run, *durability);
            }
            Op::Batch {
                c,
                docs,
                durability,
            } => {
                let name = COLLECTIONS[*c];
                let docs: Vec<(String, String)> = docs
                    .iter()
                    .map(|(id, text)| {
                        version += 1;
                        (format!("d{id}"), text.render(version))
                    })
                    .collect();
                let batch: Vec<Document> = docs.iter().map(|(id, t)| doc(id, t)).collect();
                db.put_batch(name, &batch, *durability).unwrap();
                for (id, body) in docs {
                    model.get_mut(name).unwrap().insert(id, body);
                    run.states.push(model.clone());
                }
                ack(&mut run, *durability);
            }
            Op::Commit => {
                db.commit().unwrap();
                ack(&mut run, Durability::Fsync);
            }
        }
    }
    run
}

// ---- Invariant 1: flushes during replay ----

#[derive(Debug, Clone)]
struct FlushCase {
    ops: Vec<Op>,
    chunk_target: usize,
    /// The memtable threshold of the first reopen: small, so replay flushes.
    small: usize,
    /// The threshold of the second reopen.
    second: usize,
}

fn flush_case() -> impl Strategy<Value = FlushCase> {
    let len = if cfg!(feature = "slow") {
        1..400
    } else {
        1..120
    };
    (
        ops(len, true, true),
        chunk_target(),
        1024usize..12 << 10,
        prop_oneof![Just(BIG), 1024usize..32 << 10],
    )
        .prop_map(|(ops, chunk_target, small, second)| FlushCase {
            ops,
            chunk_target,
            small,
            second,
        })
}

/// Cases in which the first reopen committed during replay, over the whole run.
static FLUSHED: AtomicU64 = AtomicU64::new(0);
static RAN: AtomicU64 = AtomicU64::new(0);

fn flush_during_replay(case: FlushCase) -> Result<(), TestCaseError> {
    let file = SimFile::new();
    let mut db = create(&file, case.chunk_target);
    let run = write(&mut db, &case.ops);
    // Make everything durable without a commit: an `Fsync` record that changes nothing.
    db.delete("a", "never-written", Durability::Fsync).unwrap();
    let base = db.seq();
    drop(db);
    let model = run.states.last().unwrap().clone();
    RAN.fetch_add(1, Ordering::Relaxed);

    for loss in [CrashLoss::All, CrashLoss::Nothing] {
        let crashed = file.crash(loss);
        let reader = ReadOnlyDatabase::open_on(SimFile::from_bytes(crashed.bytes()), &registry())
            .map_err(|e| TestCaseError::fail(format!("{loss:?}: read-only: {e}")))?;
        prop_assert_eq!(
            &observed(&reader.snapshot()),
            &model,
            "{:?}: read-only",
            loss
        );
        reader.verify().unwrap();
        drop(reader);

        let db = Database::open_on(crashed.clone(), options(case.small, case.chunk_target))
            .map_err(|e| TestCaseError::fail(format!("{loss:?}: first reopen: {e}")))?;
        prop_assert_eq!(
            &observed(&db.snapshot()),
            &model,
            "{:?}: first reopen",
            loss
        );
        one_live_copy(&db.snapshot())?;
        if loss == CrashLoss::All && db.seq() > base {
            FLUSHED.fetch_add(1, Ordering::Relaxed);
        }
        drop(db);

        // The commits of the first reopen are synced; its last memtables are replayed again.
        let again = crashed.crash(CrashLoss::All);
        let reader = ReadOnlyDatabase::open_on(SimFile::from_bytes(again.bytes()), &registry())
            .map_err(|e| TestCaseError::fail(format!("{loss:?}: read-only after flushes: {e}")))?;
        prop_assert_eq!(
            &observed(&reader.snapshot()),
            &model,
            "{:?}: read-only after flushes",
            loss
        );
        reader
            .verify()
            .map_err(|e| TestCaseError::fail(format!("{loss:?}: verify after flushes: {e}")))?;
        drop(reader);
        let mut db = Database::open_on(again.clone(), options(case.second, case.chunk_target))
            .map_err(|e| TestCaseError::fail(format!("{loss:?}: second reopen: {e}")))?;
        prop_assert_eq!(
            &observed(&db.snapshot()),
            &model,
            "{:?}: second reopen",
            loss
        );
        one_live_copy(&db.snapshot())?;
        // A commit that flushes nothing: the replayed records must keep holding `wal_start` back.
        db.create_collection(schema("z")).unwrap();
        drop(db);

        let last = again.crash(CrashLoss::All);
        let mut expected = model.clone();
        expected.insert("z".to_owned(), BTreeMap::new());
        let db = Database::open_on(last.clone(), options(BIG, case.chunk_target))
            .map_err(|e| TestCaseError::fail(format!("{loss:?}: third reopen: {e}")))?;
        prop_assert_eq!(
            &observed(&db.snapshot()),
            &expected,
            "{:?}: after a create",
            loss
        );
        drop(db);
        let reader = ReadOnlyDatabase::open_on(last, &registry()).unwrap();
        prop_assert_eq!(
            &observed(&reader.snapshot()),
            &expected,
            "{:?}: read-only, last",
            loss
        );
        reader
            .verify()
            .map_err(|e| TestCaseError::fail(format!("{loss:?}: verify, last: {e}")))?;
    }
    Ok(())
}

#[test]
fn flushes_during_replay_match_the_model() {
    check(
        "flushes_during_replay_match_the_model",
        cases(48, 512),
        flush_case(),
        flush_during_replay,
    );
    // The generator must reach the code under test: most cases flush during the first reopen.
    let (flushed, ran) = (FLUSHED.load(Ordering::Relaxed), RAN.load(Ordering::Relaxed));
    assert!(
        flushed * 3 >= ran,
        "only {flushed} of {ran} cases flushed during replay"
    );
}

// ---- Invariant 2: a flush in the middle of a chunk ----

#[test]
fn a_flush_in_the_middle_of_a_chunk_keeps_wal_start_at_it() {
    let file = SimFile::new();
    let mut db = create(&file, 1 << 20);
    // One batch, one chunk: 200 records of `b` and none of `a` or `c`.
    let docs: Vec<Document> = (0..200)
        .map(|i| {
            doc(
                &format!("d{}", i % 50),
                &format!("batch {i} {}", WORDS[i % 6]),
            )
        })
        .collect();
    db.put_batch("b", &docs, Durability::Fsync).unwrap();
    let base = db.seq();
    drop(db);
    let crashed = file.crash(CrashLoss::All);
    let chunks: Vec<u64> =
        Container::open_on(crashed.clone(), persia_storage::container::Mode::ReadOnly)
            .unwrap()
            .uncommitted_frames()
            .iter()
            .filter(|f| f.kind == FrameKind::WalChunk)
            .map(|f| f.offset)
            .collect();
    assert_eq!(chunks.len(), 1, "the batch is one chunk");
    let model: BTreeMap<String, String> = (150..200)
        .map(|i| {
            (
                format!("d{}", i % 50),
                format!("batch {i} {}", WORDS[i % 6]),
            )
        })
        .collect();

    let db = Database::open_on(crashed.clone(), options(2048, 1 << 20)).unwrap();
    assert!(db.seq() > base, "no flush during replay");
    // Every flush happened while replaying the one chunk: `wal_start` stays at it.
    assert_eq!(db.manifest().wal_start(), chunks[0]);
    let shown = observed(&db.snapshot());
    assert_eq!(shown["b"], model);
    drop(db);

    // The rest of the chunk (after the last flush) comes back from the WAL, the part before it again too.
    let again = crashed.crash(CrashLoss::All);
    let reader =
        ReadOnlyDatabase::open_on(SimFile::from_bytes(again.bytes()), &registry()).unwrap();
    assert_eq!(reader.replayed(), 200);
    assert_eq!(observed(&reader.snapshot())["b"], model);
    reader.verify().unwrap();
    for memtable_bytes in [BIG, 2048, 700] {
        let db = Database::open_on(
            again.crash(CrashLoss::All),
            options(memtable_bytes, 1 << 20),
        )
        .unwrap();
        assert_eq!(observed(&db.snapshot())["b"], model, "{memtable_bytes}");
        one_live_copy(&db.snapshot()).unwrap();
    }
}

// ---- Invariant 3: read-only replay equals read-write replay ----

#[derive(Debug, Clone)]
struct SameCase {
    ops: Vec<Op>,
    chunk_target: usize,
    small: usize,
    /// Where to cut the file after `log_end`, as fractions of the bytes there (per mille).
    cuts: Vec<u16>,
}

fn same_case() -> impl Strategy<Value = SameCase> {
    let len = if cfg!(feature = "slow") {
        1..300
    } else {
        1..80
    };
    (
        ops(len, true, false),
        chunk_target(),
        1024usize..12 << 10,
        prop::collection::vec(0..=1000u16, 1..4),
    )
        .prop_map(|(ops, chunk_target, small, cuts)| SameCase {
            ops,
            chunk_target,
            small,
            cuts,
        })
}

/// What a read-only open, a read-write open that does not flush, and a read-write open that flushes show of
/// `bytes`; all three must agree. Returns that state.
fn every_open_agrees(bytes: &[u8], case: &SameCase, what: &str) -> Result<Docs, TestCaseError> {
    let reader = ReadOnlyDatabase::open_on(SimFile::from_bytes(bytes.to_vec()), &registry())
        .map_err(|e| TestCaseError::fail(format!("{what}: read-only: {e}")))?;
    let ro = observed(&reader.snapshot());
    drop(reader);
    for memtable_bytes in [BIG, case.small] {
        let file = SimFile::from_bytes(bytes.to_vec());
        let db = Database::open_on(file.clone(), options(memtable_bytes, case.chunk_target))
            .map_err(|e| {
                TestCaseError::fail(format!("{what}: read-write {memtable_bytes}: {e}"))
            })?;
        prop_assert_eq!(
            &observed(&db.snapshot()),
            &ro,
            "{}: read-write {}",
            what,
            memtable_bytes
        );
        drop(db);
        // And a read-only open after the writer (truncated tail, replay flushes) still agrees.
        let reader =
            ReadOnlyDatabase::open_on(file.crash(CrashLoss::All), &registry()).map_err(|e| {
                TestCaseError::fail(format!("{what}: read-only after {memtable_bytes}: {e}"))
            })?;
        prop_assert_eq!(
            &observed(&reader.snapshot()),
            &ro,
            "{}: read-only after {}",
            what,
            memtable_bytes
        );
        reader.verify().map_err(|e| {
            TestCaseError::fail(format!("{what}: verify after {memtable_bytes}: {e}"))
        })?;
    }
    Ok(ro)
}

/// The index of the first state at or after `floor` equal to `docs`.
fn prefix_at(run: &Run, docs: &Docs, floor: usize) -> Option<usize> {
    (floor..run.states.len()).find(|&i| run.states[i] == *docs)
}

fn read_only_equals_read_write(case: SameCase) -> Result<(), TestCaseError> {
    let file = SimFile::new();
    let mut db = create(&file, case.chunk_target);
    let run = write(&mut db, &case.ops);
    drop(db);

    for (loss, floor) in [
        (CrashLoss::All, run.synced),
        (CrashLoss::Nothing, run.written),
    ] {
        let crashed = file.crash(loss);
        let bytes = crashed.bytes();
        let shown = every_open_agrees(&bytes, &case, &format!("{loss:?}"))?;
        prop_assert!(
            prefix_at(&run, &shown, floor).is_some(),
            "{:?}: not the state of a prefix holding the {} acknowledged records: {:?}",
            loss,
            floor,
            shown
        );

        // Zeros after the last frame: a torn tail, which a read-write open truncates and a read-only one reads up to.
        let mut torn = bytes.clone();
        torn.resize(torn.len() + 1000, 0);
        prop_assert_eq!(
            &every_open_agrees(&torn, &case, &format!("{loss:?}, torn"))?,
            &shown
        );

        // Cut anywhere after `log_end`: a prefix of the uncommitted frames survives.
        let log_end = Container::open_on(
            SimFile::from_bytes(bytes.clone()),
            persia_storage::container::Mode::ReadOnly,
        )
        .unwrap()
        .superblock()
        .log_end;
        let after = bytes.len() as u64 - log_end;
        for &cut in &case.cuts {
            let at = log_end + after * u64::from(cut) / 1000;
            let what = format!("{loss:?}, cut at {at} of {}", bytes.len());
            let cut_shown = every_open_agrees(&bytes[..at as usize], &case, &what)?;
            // Never a state that skips a record and keeps a later one; and the committed records stay.
            let at_state = (0..run.states.len()).find(|&i| run.states[i] == cut_shown);
            prop_assert!(
                at_state.is_some(),
                "{}: not a prefix: {:?}",
                what,
                cut_shown
            );
        }
    }
    Ok(())
}

#[test]
fn read_only_replay_equals_read_write_replay() {
    check(
        "read_only_replay_equals_read_write_replay",
        cases(32, 384),
        same_case(),
        read_only_equals_read_write,
    );
}

/// Every cut after `log_end`, on one fixed backlog of mixed chunk sizes (slow: every byte; default: a stride).
#[test]
fn every_cut_after_log_end_opens_alike() {
    let file = SimFile::new();
    let mut db = create(&file, 600);
    db.put("a", &doc("d0", "committed alpha"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    for i in 0..30 {
        let c = COLLECTIONS[i % 3];
        if i % 4 == 3 {
            let batch: Vec<Document> = (0..5)
                .map(|j| doc(&format!("d{}", (i + j) % 7), &format!("batch {i} {j} beta")))
                .collect();
            db.put_batch(c, &batch, Durability::Async).unwrap();
        } else if i % 5 == 2 {
            db.delete(c, &format!("d{}", i % 7), Durability::Buffered)
                .unwrap();
        } else {
            db.put(
                c,
                &doc(&format!("d{}", i % 7), &format!("put {i} gamma")),
                Durability::Fsync,
            )
            .unwrap();
        }
    }
    drop(db);
    let bytes = file.crash(CrashLoss::Nothing).bytes();
    let log_end = Container::open_on(
        SimFile::from_bytes(bytes.clone()),
        persia_storage::container::Mode::ReadOnly,
    )
    .unwrap()
    .superblock()
    .log_end as usize;
    assert!(bytes.len() > log_end + 1000, "a backlog after log_end");
    let case = SameCase {
        ops: Vec::new(),
        chunk_target: 600,
        small: 1024,
        cuts: Vec::new(),
    };
    let stride = if cfg!(feature = "slow") { 1 } else { 37 };
    let mut previous: Option<Docs> = None;
    let mut distinct = 0;
    for at in (log_end..=bytes.len()).step_by(stride) {
        let shown = every_open_agrees(&bytes[..at], &case, &format!("cut at {at}"))
            .unwrap_or_else(|e| panic!("{e}"));
        if previous.as_ref() != Some(&shown) {
            distinct += 1;
        }
        previous = Some(shown);
    }
    assert!(distinct > 5, "{distinct} distinct prefixes");
}

// ---- Invariant 4: verify failures ----

/// A database committing `docs` into one segment of `a`, with segment ids from `seed`: the segment's entry and bytes.
fn one_segment(seed: u64, docs: &[(&str, &str)]) -> (SegmentEntry, Vec<u8>) {
    let file = SimFile::new();
    let mut options = options(BIG, 1 << 20);
    options.segment_ids = SegmentIds::seeded(seed);
    let mut db = Database::create_on(file, [1; 16], options).unwrap();
    db.create_collection(schema("a")).unwrap();
    for (id, text) in docs {
        db.put("a", &doc(id, text), Durability::Buffered).unwrap();
    }
    db.commit().unwrap();
    let entry = db.manifest().collection("a").unwrap().segments()[0];
    let Location::Frame { offset, .. } = entry.location else {
        panic!("a local segment is a frame")
    };
    let bytes = db
        .container()
        .read_frame(offset, usize::MAX)
        .unwrap()
        .payload;
    (entry, bytes)
}

/// A container whose committed manifest has collection `a` with `segments` (entry, bytes), each appended as a
/// Segment frame with a valid CRC, in order.
fn crafted(segments: &[(SegmentEntry, Vec<u8>)]) -> SimFile {
    let file = SimFile::new();
    let mut c = Container::create_on(file.clone(), [8; 16]).unwrap();
    let mut entries = Vec::new();
    for (entry, bytes) in segments {
        let f = c
            .append(FrameKind::Segment, bytes, Compression::None)
            .unwrap();
        entries.push(SegmentEntry {
            size: bytes.len() as u64,
            location: Location::Frame {
                offset: f.offset,
                len: f.len,
            },
            tombstones: None,
            ..*entry
        });
    }
    c.sync().unwrap();
    let wal_start = c.end();
    let mut diff = ManifestDiff {
        wal_start: Some(wal_start),
        ..ManifestDiff::default()
    };
    diff.collections.insert(
        "a".into(),
        CollectionDiff::Create {
            schema: schema("a"),
            segments: entries,
        },
    );
    let next = Manifest::empty().apply(&diff).unwrap();
    let frame = c
        .append(
            FrameKind::Manifest,
            &next.encode().unwrap(),
            Compression::None,
        )
        .unwrap();
    c.commit(Commit {
        manifest: frame,
        wal_start,
        fence_token: next.fence_token(),
    })
    .unwrap();
    file
}

#[test]
fn verify_fails_on_damage_that_open_misses() {
    let docs: Vec<(String, String)> = (0..40)
        .map(|i| {
            (
                format!("x{i:02}"),
                format!("{} {} number {i}", WORDS[i % 6], WORDS[(i * 5 + 1) % 6]),
            )
        })
        .collect();
    let docs: Vec<(&str, &str)> = docs.iter().map(|(i, t)| (i.as_str(), t.as_str())).collect();
    let (entry, bytes) = one_segment(11, &docs);
    let sound = crafted(&[(entry, bytes.clone())]);
    let report = ReadOnlyDatabase::open_on(sound, &registry())
        .unwrap()
        .verify()
        .unwrap();
    assert_eq!((report.segments, report.documents), (1, 40));

    // The first single-bit flip that `Segment::open` accepts (same id and count) and `Segment::verify` rejects.
    let s = schema("a");
    let damaged = (0..bytes.len())
        .find_map(|at| {
            let mut edited = bytes.clone();
            edited[at] ^= 0x01;
            let seg = Segment::open(ByteSource::new(edited.clone().into()), &s).ok()?;
            let same = seg.segment_id() == entry.id.get() && seg.doc_count() == entry.doc_count;
            (same && seg.verify().is_err()).then_some(edited)
        })
        .expect("damage that only verify finds");
    let file = crafted(&[(entry, damaged)]);
    // Behind a valid frame CRC, every open succeeds; only verify reads the whole segment.
    let before = file.mutations();
    let reader = ReadOnlyDatabase::open_on(file.clone(), &registry()).unwrap();
    let error = reader.verify().unwrap_err();
    assert!(
        matches!(&error, DatabaseError::OpenSegment { collection, id, .. } if collection == "a" && *id == entry.id),
        "{error}"
    );
    assert!(error.is_corruption(), "{error}");
    assert_eq!(
        file.mutations(),
        before,
        "the read-only open and verify wrote nothing"
    );
    drop(reader);
    // A read-write open does not check whole segments either.
    Database::open_on(file, options(BIG, 1 << 20)).unwrap();
}

#[test]
fn verify_fails_on_an_id_live_in_two_segments() {
    let (first, first_bytes) = one_segment(21, &[("x", "one alpha"), ("y", "two beta")]);
    let (second, second_bytes) = one_segment(22, &[("z", "three gamma"), ("x", "four delta")]);
    assert_ne!(first.id, second.id);
    let file = crafted(&[(first, first_bytes.clone()), (second, second_bytes)]);
    // Open checks no overlap: a lookup shows one copy, verify finds both.
    let before = file.mutations();
    let reader = ReadOnlyDatabase::open_on(file.clone(), &registry()).unwrap();
    let error = reader.verify().unwrap_err();
    assert!(
        matches!(&error, DatabaseError::LiveTwice { collection, id } if collection == "a" && id == "x"),
        "{error}"
    );
    assert!(error.is_corruption(), "{error}");
    assert_eq!(file.mutations(), before, "verify wrote nothing");

    // Control: the same layout without the duplicate passes.
    let (other, other_bytes) = one_segment(22, &[("z", "three gamma"), ("w", "four delta")]);
    let file = crafted(&[(first, first_bytes), (other, other_bytes)]);
    let report = ReadOnlyDatabase::open_on(file, &registry())
        .unwrap()
        .verify()
        .unwrap();
    assert_eq!(
        (report.segments, report.documents, report.deleted),
        (2, 4, 0)
    );
    assert_eq!(report.tail, Tail::Clean);
}
