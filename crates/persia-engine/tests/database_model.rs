//! Property tests of the database write path against a naive model (ROADMAP 6.3; SPEC §4.5, §8.2, §8.2.1, §8.6,
//! §10.1, ADR-0029).
//!
//! Invariants:
//! 1. Differential: any sequence of puts, upserts, deletes (over two collections), commits, policy ticks (the clock
//!    advances), creates of further collections (commits that flush nothing but move `wal_start`) and reopens shows,
//!    after every step, exactly the documents a `BTreeMap` model holds; so does a reopen that loses every unsynced
//!    write (all operations are `Fsync`), and a further commit and reopen. Deletes are restricted to ids in no
//!    segment: deleting a flushed id needs tombstones (ROADMAP 6.4.3, ADR-0029 Consequences).
//! 2. Durability levels (SPEC §8.2.1): after any prefix of a sequence of `Buffered`, `Async` and `Fsync` operations
//!    and commits, a crash that keeps the OS's pages (`CrashLoss::Nothing`, process death) reopens to the state of a
//!    prefix of the sequence that includes every `Async`/`Fsync`-acknowledged operation; a crash that loses every
//!    unsynced write (`CrashLoss::All`) reopens to the state of a prefix that includes every `Fsync`-acknowledged
//!    operation and every commit. Never a state that skips an operation and keeps a later one.
//! 3. `ContainerObjects` (ROADMAP 6.3.6, SPEC §10.1) behaves as a `MemoryStorage` holding the same objects: every
//!    listed key is `get`-able with the listed size; `get_range` gives the same bytes, or the same error (ranges past
//!    the end are `ShortRead`, kind `Corrupt`; inverted ranges are `InvalidRange`); `list` of any prefix is the
//!    filtered full list. A reopen after losing every unsynced write lists the same objects with the same bytes,
//!    and every listed WAL chunk is at or after the committed `wal_start`.
//!
//! Seeds: `PROPTEST_RNG_SEED` (default fixed below) and `PROPTEST_CASES`; a failure prints the seed it ran with.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::needless_pass_by_value,
    clippy::reversed_empty_ranges, // inverted ranges are a tested input
    clippy::string_slice, // keys are ASCII
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use persia_engine::db::{
    CommitPolicy, Database, DatabaseOptions, ManualClock, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::{FlushPolicy, IdState};
use persia_engine::schema::{CollectionSchema, FieldId, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFile};
use persia_storage::{Error as StorageError, ErrorKind, Key, KeyPrefix, MemoryStorage, Storage};
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestCaseError, TestRunner};

const COLLECTIONS: [&str; 2] = ["a", "b"];
const WORDS: [&str; 6] = ["alpha", "beta", "gamma", "delta", "common", "omega"];
const IDS: u8 = 8;

/// The fixed proptest seed; `PROPTEST_RNG_SEED` overrides it.
const PROPTEST_SEED: u64 = 0x0DB6_3000_0001;

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
        .field("n", FieldKind::I64 { stored: true })
        .build()
        .unwrap()
}

fn fields() -> (FieldId, FieldId) {
    let s = schema("x");
    (s.field("body").unwrap().id(), s.field("n").unwrap().id())
}

fn document(id: u8, words: &[u8], n: i64) -> Document {
    let (body, nf) = fields();
    let mut doc = Document::new(format!("d{id}")).with(nf, Value::I64(n));
    if !words.is_empty() {
        let text: Vec<&str> = words.iter().map(|&w| WORDS[w as usize]).collect();
        doc = doc.with(body, Value::String(text.join(" ")));
    }
    doc
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(1_700_000_000_000_000))
}

fn options(clock: &Arc<ManualClock>, wal_chunk_target: usize) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::clone(clock) as _,
        // The same seed on every open: the ids drawn collide with committed ones, which the database must skip.
        segment_ids: SegmentIds::seeded(1),
        policy: CommitPolicy {
            flush: FlushPolicy {
                memtable_bytes: 1 << 30,
                idle: Duration::from_secs(10),
            },
            interval: Duration::from_secs(30),
        },
        wal_chunk_target,
        ..DatabaseOptions::default()
    }
}

/// Documents by collection and id.
type State = BTreeMap<String, BTreeMap<String, Document>>;

/// The document with id `id` as `snapshot` shows it: the memtable's version hides the segments', newer segments
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

/// Every live document of every collection `snapshot` shows.
fn observed(snapshot: &Snapshot) -> State {
    let mut state = State::new();
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
        let docs = state.entry(name.to_owned()).or_default();
        for id in ids {
            if let Some(d) = get(snapshot, name, &id) {
                docs.insert(id, d);
            }
        }
    }
    state
}

/// Whether a segment of `collection` holds `id`: a delete of it would need a tombstone (ROADMAP 6.4.3).
fn in_segment(snapshot: &Snapshot, collection: &str, id: &str) -> bool {
    snapshot
        .collection(collection)
        .unwrap()
        .segments()
        .iter()
        .any(|s| s.document_by_id(id).unwrap().is_some())
}

fn empty_state() -> State {
    COLLECTIONS
        .iter()
        .map(|c| ((*c).to_owned(), BTreeMap::new()))
        .collect()
}

/// A new database on `file` with both collections committed.
fn create(file: &SimFile, clock: &Arc<ManualClock>, wal_chunk_target: usize) -> Database<SimFile> {
    let mut db =
        Database::create_on(file.clone(), [0x63; 16], options(clock, wal_chunk_target)).unwrap();
    for c in COLLECTIONS {
        db.create_collection(schema(c)).unwrap();
    }
    db
}

// ---- Invariant 1: differential against a BTreeMap ----

#[derive(Debug, Clone)]
enum Op {
    Put {
        c: usize,
        id: u8,
        words: Vec<u8>,
        n: i64,
    },
    Delete {
        c: usize,
        id: u8,
    },
    Commit,
    /// Advances the clock by this many seconds, then `tick`s.
    Tick(u64),
    /// Drops the database and opens what a crash losing every unsynced write leaves.
    Reopen,
    /// Creates the next collection `c<n>`, a commit that flushes nothing but moves `wal_start`.
    Create,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (0..2_usize, 0..IDS, prop::collection::vec(0..WORDS.len() as u8, 0..5), -3..3_i64)
            .prop_map(|(c, id, words, n)| Op::Put { c, id, words, n }),
        3 => (0..2_usize, 0..IDS).prop_map(|(c, id)| Op::Delete { c, id }),
        1 => Just(Op::Commit),
        2 => prop_oneof![Just(0_u64), Just(5), Just(10), Just(31)].prop_map(Op::Tick),
        2 => Just(Op::Reopen),
        1 => Just(Op::Create),
    ]
}

fn reopen(file: &SimFile, clock: &Arc<ManualClock>) -> (SimFile, Database<SimFile>) {
    let crashed = file.crash(CrashLoss::All);
    let db = Database::open_on(crashed.clone(), options(clock, 256)).unwrap();
    (crashed, db)
}

fn differential(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let clock = clock();
    let mut file = SimFile::new();
    let mut db = create(&file, &clock, 256);
    let mut model = empty_state();
    for (step, op) in ops.iter().enumerate() {
        match op {
            Op::Put { c, id, words, n } => {
                let doc = document(*id, words, *n);
                db.put(COLLECTIONS[*c], &doc, Durability::Fsync).unwrap();
                model
                    .get_mut(COLLECTIONS[*c])
                    .unwrap()
                    .insert(doc.id().to_owned(), doc);
            }
            Op::Delete { c, id } => {
                let id = format!("d{id}");
                if in_segment(&db.snapshot(), COLLECTIONS[*c], &id) {
                    continue;
                }
                db.delete(COLLECTIONS[*c], &id, Durability::Fsync).unwrap();
                model.get_mut(COLLECTIONS[*c]).unwrap().remove(&id);
            }
            Op::Commit => {
                let seq = db.seq();
                let dirty = db
                    .snapshot()
                    .collections()
                    .any(|(_, c)| c.memtable().version() > 0);
                let new = db.commit().unwrap();
                prop_assert_eq!(new, if dirty { seq + 1 } else { seq }, "step {}", step);
                for (_, c) in db.snapshot().collections() {
                    prop_assert_eq!(
                        c.memtable().version(),
                        0,
                        "step {}: a commit empties every memtable",
                        step
                    );
                }
            }
            Op::Tick(secs) => {
                clock.advance(Duration::from_secs(*secs));
                let seq = db.seq();
                let ticked = db.tick().unwrap();
                prop_assert_eq!(ticked.unwrap_or(seq), db.seq(), "step {}", step);
                prop_assert_eq!(db.seq(), seq + u64::from(ticked.is_some()), "step {}", step);
            }
            Op::Reopen => {
                let seq = db.seq();
                drop(db);
                (file, db) = reopen(&file, &clock);
                prop_assert_eq!(db.seq(), seq, "step {}: every commit was durable", step);
            }
            Op::Create => {
                let name = format!("c{}", model.len());
                let seq = db.seq();
                prop_assert_eq!(db.create_collection(schema(&name)).unwrap(), seq + 1);
                model.insert(name, BTreeMap::new());
            }
        }
        prop_assert_eq!(&observed(&db.snapshot()), &model, "step {}: {:?}", step, op);
        prop_assert_eq!(db.reader().seq(), db.seq());
    }

    // Every operation was `Fsync`: losing every unsynced write loses nothing.
    let seq = db.seq();
    drop(db);
    let (file, mut db) = reopen(&file, &clock);
    prop_assert_eq!(db.seq(), seq);
    prop_assert_eq!(&observed(&db.snapshot()), &model, "after the reopen");
    // Recovery is idempotent: flushing the replayed records and reopening keeps the documents.
    db.commit().unwrap();
    prop_assert_eq!(
        &observed(&db.snapshot()),
        &model,
        "after the commit of the replayed records"
    );
    for (name, c) in db.snapshot().collections() {
        prop_assert_eq!(c.memtable().version(), 0, "{}", name);
    }
    prop_assert_eq!(
        db.manifest().wal_start(),
        db.container().superblock().manifest_off
    );
    drop(db);
    let (_, db) = reopen(&file, &clock);
    prop_assert_eq!(
        &observed(&db.snapshot()),
        &model,
        "after the commit and a reopen"
    );
    Ok(())
}

#[test]
fn matches_a_btreemap_across_commits_ticks_and_reopens() {
    check(
        "matches_a_btreemap_across_commits_ticks_and_reopens",
        cases(96, 1024),
        prop::collection::vec(op(), 0..48),
        differential,
    );
}

// ---- Invariant 2: durability levels and crashes ----

#[derive(Debug, Clone)]
enum DurableOp {
    Put {
        c: usize,
        id: u8,
        words: Vec<u8>,
        durability: Durability,
    },
    Delete {
        c: usize,
        id: u8,
        durability: Durability,
    },
    Commit,
}

fn durability() -> impl Strategy<Value = Durability> {
    prop_oneof![
        Just(Durability::Buffered),
        Just(Durability::Async),
        Just(Durability::Fsync)
    ]
}

fn durable_op() -> impl Strategy<Value = DurableOp> {
    prop_oneof![
        8 => (0..2_usize, 0..IDS, prop::collection::vec(0..WORDS.len() as u8, 0..4), durability())
            .prop_map(|(c, id, words, durability)| DurableOp::Put { c, id, words, durability }),
        3 => (0..2_usize, 0..IDS, durability())
            .prop_map(|(c, id, durability)| DurableOp::Delete { c, id, durability }),
        1 => Just(DurableOp::Commit),
    ]
}

/// The state after a crash with `loss`, which must be `prefixes[k]` for some `k >= at_least`.
fn check_prefix(
    file: &SimFile,
    clock: &Arc<ManualClock>,
    loss: CrashLoss,
    prefixes: &[State],
    at_least: usize,
    ctx: &str,
) -> Result<(), TestCaseError> {
    let db = Database::open_on(file.crash(loss), options(clock, 128))
        .map_err(|e| TestCaseError::fail(format!("{ctx}, {loss:?}: reopen failed: {e}")))?;
    let state = observed(&db.snapshot());
    let k = prefixes[at_least..].iter().position(|p| *p == state);
    prop_assert!(
        k.is_some(),
        "{}, {:?}: the reopened state is no prefix of at least {} operations\n  observed {:?}\n  required prefix {:?}",
        ctx,
        loss,
        at_least,
        state,
        prefixes[at_least]
    );
    Ok(())
}

fn durability_levels(ops: Vec<DurableOp>) -> Result<(), TestCaseError> {
    let clock = clock();
    let file = SimFile::new();
    // Small chunks: buffered records are written out (unsynced) by rotation, as well as by `Async`.
    let mut db = create(&file, &clock, 128);
    let mut model = empty_state();
    // `prefixes[k]`: the model after the first `k` operations that ran.
    let mut prefixes = vec![model.clone()];
    // The length of the longest prefix acknowledged at `Async` or better (survives process death), and at `Fsync`
    // (survives losing every unsynced write). A commit syncs everything before it.
    let (mut written, mut synced) = (0, 0);
    for (step, op) in ops.iter().enumerate() {
        let level = match op {
            DurableOp::Put {
                c,
                id,
                words,
                durability,
            } => {
                let doc = document(*id, words, i64::try_from(step).unwrap());
                db.put(COLLECTIONS[*c], &doc, *durability).unwrap();
                model
                    .get_mut(COLLECTIONS[*c])
                    .unwrap()
                    .insert(doc.id().to_owned(), doc);
                *durability
            }
            DurableOp::Delete { c, id, durability } => {
                let id = format!("d{id}");
                if in_segment(&db.snapshot(), COLLECTIONS[*c], &id) {
                    continue;
                }
                db.delete(COLLECTIONS[*c], &id, *durability).unwrap();
                model.get_mut(COLLECTIONS[*c]).unwrap().remove(&id);
                *durability
            }
            DurableOp::Commit => {
                db.commit().unwrap();
                Durability::Fsync
            }
        };
        prefixes.push(model.clone());
        let ran = prefixes.len() - 1;
        match level {
            Durability::Buffered => {}
            Durability::Async => written = ran,
            Durability::Fsync => (written, synced) = (ran, ran),
        }
        prop_assert_eq!(&observed(&db.snapshot()), &model, "step {}", step);
        let ctx = format!("after step {step} ({op:?})");
        check_prefix(&file, &clock, CrashLoss::Nothing, &prefixes, written, &ctx)?;
        check_prefix(&file, &clock, CrashLoss::All, &prefixes, synced, &ctx)?;
    }
    // A clean close loses nothing a commit made durable: commit, then crash.
    db.commit().unwrap();
    check_prefix(
        &file,
        &clock,
        CrashLoss::All,
        &prefixes,
        prefixes.len() - 1,
        "after the final commit",
    )?;
    Ok(())
}

#[test]
fn crashes_keep_a_prefix_with_every_acknowledged_operation() {
    check(
        "crashes_keep_a_prefix_with_every_acknowledged_operation",
        cases(48, 512),
        prop::collection::vec(durable_op(), 0..32),
        durability_levels,
    );
}

// ---- Invariant 3: the container's object view ----

#[derive(Debug, Clone)]
struct ObjectsCase {
    /// (collection, id, words); `None` commits.
    ops: Vec<Option<(usize, u8, Vec<u8>)>>,
    wal_chunk_target: usize,
    /// Ranges as fractions (in 1/16ths) of an object's length plus a few bytes past it.
    ranges: Vec<(u8, u8)>,
}

fn objects_case() -> impl Strategy<Value = ObjectsCase> {
    let op = prop_oneof![
        6 => (0..2_usize, 0..IDS, prop::collection::vec(0..WORDS.len() as u8, 0..6)).prop_map(Some),
        1 => Just(None),
    ];
    (
        prop::collection::vec(op, 0..40),
        prop_oneof![Just(64_usize), Just(200), Just(1 << 20)],
        prop::collection::vec((0..=18_u8, 0..=18_u8), 1..12),
    )
        .prop_map(|(ops, wal_chunk_target, ranges)| ObjectsCase {
            ops,
            wal_chunk_target,
            ranges,
        })
}

/// Every listed object and its bytes.
fn objects_of<S: Storage>(objects: &S) -> BTreeMap<String, Vec<u8>> {
    let listed = objects.list(&KeyPrefix::new("").unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for meta in listed {
        let bytes = objects.get(&meta.key).unwrap();
        assert_eq!(
            bytes.len() as u64,
            meta.size,
            "{}: listed size",
            meta.key.as_str()
        );
        assert!(
            out.insert(meta.key.as_str().to_owned(), bytes.to_vec())
                .is_none(),
            "listed twice"
        );
    }
    out
}

/// What `get_range` gave, comparable across backends.
fn read_range<S: Storage>(
    s: &S,
    key: &Key,
    range: std::ops::Range<u64>,
) -> Result<(u64, Vec<u8>), String> {
    s.get_range(key, range)
        .map(|b| (b.base_offset(), b.to_vec()))
        .map_err(|e| format!("{e:?}"))
}

fn object_view(case: ObjectsCase) -> Result<(), TestCaseError> {
    let clock = clock();
    let file = SimFile::new();
    let mut db = create(&file, &clock, case.wal_chunk_target);
    for op in &case.ops {
        match op {
            Some((c, id, words)) => db
                .put(COLLECTIONS[*c], &document(*id, words, 0), Durability::Fsync)
                .unwrap(),
            None => drop(db.commit().unwrap()),
        }
    }
    // A commit that flushes nothing still advances `seq` when it creates a collection: WAL chunks written since the
    // last flush become committed objects.
    db.create_collection(schema("c")).unwrap();

    let objects = db.objects();
    let all = objects_of(&objects);
    let mirror = MemoryStorage::new();
    for (key, bytes) in &all {
        mirror
            .put(&Key::new(key.as_str()).unwrap(), bytes.clone().into())
            .unwrap();
    }

    // What is listed: the committed manifest, every live segment, the WAL chunks from `wal_start` to `log_end`.
    let manifest = db.manifest();
    let mut expected: BTreeSet<String> = BTreeSet::new();
    expected.insert(format!("manifests/{:020}.mf", manifest.seq()));
    for c in manifest.collections() {
        for s in c.segments() {
            expected.insert(format!("segments/{}/{}.seg", c.name(), s.id));
        }
    }
    let sb = db.container().superblock();
    for f in db.container().frames() {
        if f.kind == FrameKind::WalChunk && f.end() <= sb.log_end {
            prop_assert!(
                f.offset >= manifest.wal_start(),
                "a chunk before wal_start is listed"
            );
            expected.insert(format!("wal/{:020}.wal", f.frame_seq));
        }
    }
    prop_assert_eq!(all.keys().cloned().collect::<BTreeSet<_>>(), expected);

    for (key, bytes) in &all {
        let k = Key::new(key.as_str()).unwrap();
        let len = bytes.len() as u64;
        let at = |sixteenths: u8| {
            (len * u64::from(sixteenths.min(16)) / 16) + u64::from(sixteenths.saturating_sub(16))
        };
        let mut ranges: Vec<std::ops::Range<u64>> =
            case.ranges.iter().map(|&(a, b)| at(a)..at(b)).collect();
        ranges.extend([
            0..0,
            0..len,
            len..len,
            len..len + 1,
            0..len + 1,
            len + 1..len + 1,
        ]);
        if len > 0 {
            ranges.extend([len - 1..len, 1..0, len..len - 1]);
        }
        for range in ranges {
            let got = read_range(&objects, &k, range.clone());
            prop_assert_eq!(
                &got,
                &read_range(&mirror, &k, range.clone()),
                "{} {:?}",
                key,
                range
            );
            if range.start > range.end {
                let e = objects.get_range(&k, range.clone()).unwrap_err();
                prop_assert!(
                    matches!(e, StorageError::InvalidRange { .. }),
                    "{} {:?}: {:?}",
                    key,
                    range,
                    e
                );
                prop_assert_eq!(e.kind(), ErrorKind::InvalidInput);
            } else if range.end > len {
                let e = objects.get_range(&k, range.clone()).unwrap_err();
                prop_assert!(
                    matches!(e, StorageError::ShortRead { .. }),
                    "{} {:?}: {:?}",
                    key,
                    range,
                    e
                );
                prop_assert_eq!(e.kind(), ErrorKind::Corrupt);
            } else {
                let (base, got) = got.unwrap();
                prop_assert_eq!(base, range.start);
                prop_assert_eq!(&got[..], &bytes[range.start as usize..range.end as usize]);
            }
        }
    }
    let mut prefixes: BTreeSet<String> = [
        "",
        "m",
        "manifests/",
        "segments/",
        "segments/a/",
        "segments/b",
        "wal/",
        "zzz",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for key in all.keys() {
        // Every proper prefix of a few keys, at its `/` boundaries and mid-component.
        for (i, _) in key.char_indices().step_by(7) {
            prefixes.insert(key[..i].to_owned());
        }
    }
    for prefix in prefixes {
        let p = KeyPrefix::new(prefix.as_str()).unwrap();
        let listed: Vec<(String, u64)> = objects
            .list(&p)
            .unwrap()
            .into_iter()
            .map(|m| (m.key.as_str().to_owned(), m.size))
            .collect();
        let expected: Vec<(String, u64)> = all
            .iter()
            .filter(|(k, _)| k.starts_with(prefix.as_str()))
            .map(|(k, b)| (k.clone(), b.len() as u64))
            .collect();
        prop_assert_eq!(listed, expected, "prefix {:?}", prefix);
    }
    let missing = Key::new("segments/a/missing.seg").unwrap();
    prop_assert_eq!(
        objects.get(&missing).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    prop_assert_eq!(
        objects.get_range(&missing, 0..1).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    drop(objects);

    // Committed objects are durable: a reopen after losing every unsynced write serves the same ones.
    let reopened = Database::open_on(
        file.crash(CrashLoss::All),
        options(&clock, case.wal_chunk_target),
    )
    .unwrap();
    prop_assert_eq!(objects_of(&reopened.objects()), all);
    Ok(())
}

#[test]
fn container_objects_behave_as_a_memory_storage_of_the_same_objects() {
    check(
        "container_objects_behave_as_a_memory_storage_of_the_same_objects",
        cases(32, 512),
        objects_case(),
        object_view,
    );
}
