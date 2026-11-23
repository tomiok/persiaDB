//! A model test of refreshing a read-only database in place (ROADMAP 6.6.6, SPEC §8.4, §8.6, ADR-0060).
//!
//! A writer runs random sequences of puts and deletes (`Buffered`, `Async`, `Fsync`), batches, flush commits, inline
//! merges, creates, drops and schema changes over two collections; a reader opened at the start refreshes in between.
//!
//! Invariants, after every refresh:
//! 1. The reader shows what a fresh read-only open of the same file shows: `seq`, manifest, replayed records, WAL end,
//!    skipped fields, and per collection its schema, memtable counts, segment count and every document.
//! 2. It shows the writer's committed `seq`, and when the writer holds no `Buffered` record that may be unwritten, the
//!    writer's documents.
//! 3. `Unchanged` swaps nothing (the same snapshot); `Incremental { records }` adds `records` to `replayed`, and shares
//!    every segment whose manifest entry is unchanged with the snapshot before; `Reloaded` and `Incremental` report
//!    the `seq` the reader now shows.
//! 4. A snapshot taken before keeps its `seq`, and a memtable view taken before sees what it saw. An id that a
//!    snapshot's memtable still has no entry for has the answer the snapshot gave when taken (its segments are
//!    immutable). A same-`seq` refresh shares memtables, so memtable answers of an old snapshot may advance (SPEC §8.4);
//!    after a `Reloaded` or `Unchanged` refresh, every answer of the snapshot taken just before is unchanged.
//! 5. With the fixed seed, every outcome occurs: `Unchanged`, `Incremental` at the same `seq` and at a new one, and
//!    `Reloaded` (also checked by a fixed sequence).
//!
//! Seeds: `PROPTEST_RNG_SEED` (default fixed below) and `PROPTEST_CASES`; a failure prints the seed it ran with.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, ManualClock, ReadOnlyDatabase, Refreshed, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::{IdState, MemtableView};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_storage::container::ContainerFile;
use persia_storage::container::sim::{CrashLoss, SimFile};
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestCaseError, TestRunner};

const COLLECTIONS: [&str; 2] = ["a", "b"];
const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
const IDS: u8 = 6;
/// At most this many fields added by schema changes.
const MAX_EXTRAS: usize = 2;

/// The fixed proptest seed; `PROPTEST_RNG_SEED` overrides it.
const PROPTEST_SEED: u64 = 0x0066_0006_0001;

/// Runs `test` on `cases` values of `strategy` (or `PROPTEST_CASES`) with a fixed seed, persisting failures next to
/// this file, and panics with the seed and the minimal failing input. Returns the case count it ran.
fn check<S>(
    name: &'static str,
    cases: u32,
    strategy: S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) -> u32
where
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
    cases
}

fn cases(normal: u32, slow: u32) -> u32 {
    if cfg!(feature = "slow") { slow } else { normal }
}

/// The base schema with `extras` fields added by schema changes.
fn schema(name: &str, extras: usize) -> CollectionSchema {
    let mut builder =
        SchemaBuilder::new(name).field("body", FieldKind::Text(TextOptions::default()));
    for i in 0..extras {
        builder = builder.field(format!("x{i}"), FieldKind::I64 { stored: true });
    }
    builder.build().unwrap()
}

fn id(n: u8) -> String {
    format!("d{n}")
}

fn doc(n: u8, word: u8) -> Document {
    let body = schema("x", 0).field("body").unwrap().id();
    let text = format!("{} {n}", WORDS[usize::from(word) % WORDS.len()]);
    Document::new(id(n)).with(body, Value::String(text))
}

/// Inline merges, two segments per tier; small WAL chunks if `small_chunks`, so `Buffered` records are written out
/// by rotation too.
fn options(small_chunks: bool) -> DatabaseOptions {
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
        wal_chunk_target: if small_chunks {
            200
        } else {
            DatabaseOptions::default().wal_chunk_target
        },
        ..DatabaseOptions::default()
    }
}

#[derive(Debug, Clone)]
enum Op {
    Put {
        c: usize,
        id: u8,
        word: u8,
        durability: Durability,
    },
    Delete {
        c: usize,
        id: u8,
        durability: Durability,
    },
    PutBatch {
        c: usize,
        ids: BTreeSet<u8>,
        word: u8,
        durability: Durability,
    },
    Commit,
    /// `merge_step` until a merge commits, at most a few steps.
    Merge,
    Create(usize),
    Drop(usize),
    /// `evolve_collection` adding a field.
    Evolve(usize),
    /// The writer process dies and a new one recovers the file: what a process death leaves, or with `power_loss`
    /// only what was synced (SPEC §8.2.1).
    Restart {
        power_loss: bool,
    },
    Refresh,
}

fn durability() -> impl Strategy<Value = Durability> {
    prop_oneof![
        Just(Durability::Buffered),
        Just(Durability::Async),
        Just(Durability::Fsync),
    ]
}

/// The ops; restarts lose unsynced writes only if `power_loss`.
fn op(power_loss: bool) -> impl Strategy<Value = Op> {
    let c = 0..COLLECTIONS.len();
    prop_oneof![
        6 => (c.clone(), 0..IDS, 0..4u8, durability())
            .prop_map(|(c, id, word, durability)| Op::Put { c, id, word, durability }),
        2 => (c.clone(), 0..IDS, durability()).prop_map(|(c, id, durability)| Op::Delete { c, id, durability }),
        2 => (c.clone(), prop::collection::btree_set(0..IDS, 2..4), 0..4u8, durability())
            .prop_map(|(c, ids, word, durability)| Op::PutBatch { c, ids, word, durability }),
        2 => Just(Op::Commit),
        1 => Just(Op::Merge),
        1 => c.clone().prop_map(Op::Create),
        1 => c.clone().prop_map(Op::Drop),
        1 => c.prop_map(Op::Evolve),
        1 => any::<bool>().prop_map(move |p| Op::Restart { power_loss: p && power_loss }),
        6 => Just(Op::Refresh),
    ]
}

/// How often each outcome of a refresh occurred.
#[derive(Debug, Default, Clone, Copy)]
struct Kinds {
    unchanged: u64,
    incremental_same_seq: u64,
    incremental_new_seq: u64,
    reloaded: u64,
}

impl Kinds {
    fn count(&mut self, refreshed: Refreshed, seq_before: u64) {
        match refreshed {
            Refreshed::Unchanged => self.unchanged += 1,
            Refreshed::Incremental { seq, .. } if seq == seq_before => {
                self.incremental_same_seq += 1;
            }
            Refreshed::Incremental { .. } => self.incremental_new_seq += 1,
            Refreshed::Reloaded { .. } => self.reloaded += 1,
            other => panic!("unknown outcome {other:?}"),
        }
    }

    fn all_occurred(&self) -> bool {
        self.unchanged > 0
            && self.incremental_same_seq > 0
            && self.incremental_new_seq > 0
            && self.reloaded > 0
    }
}

/// The writer and what the model tracks of it.
struct Writer {
    file: SimFile,
    small_chunks: bool,
    db: Database<SimFile>,
    /// Fields added to each live collection.
    extras: BTreeMap<&'static str, usize>,
    /// A `Buffered` record was logged since the last write-out the model is sure of: the file may lack it.
    buffered: bool,
}

impl Writer {
    fn new(small_chunks: bool) -> Self {
        let file = SimFile::new();
        let mut db = Database::create_on(file.clone(), [9; 16], options(small_chunks)).unwrap();
        db.create_collection(schema("a", 0)).unwrap();
        Self {
            file,
            small_chunks,
            db,
            extras: BTreeMap::from([("a", 0)]),
            buffered: false,
        }
    }

    fn wrote(&mut self, durability: Durability) {
        // One ordered log: an `Async` or `Fsync` write writes out every record before it.
        self.buffered = durability == Durability::Buffered;
    }

    /// Runs `op` (never `Refresh`); ops on an absent collection, and schema changes past [`MAX_EXTRAS`], are skipped.
    fn run(&mut self, op: &Op) {
        let name = |c: &usize| COLLECTIONS[*c];
        match op {
            Op::Put {
                c,
                id,
                word,
                durability,
            } => {
                if self.extras.contains_key(name(c)) {
                    self.db.put(name(c), &doc(*id, *word), *durability).unwrap();
                    self.wrote(*durability);
                }
            }
            Op::Delete { c, id, durability } => {
                if self.extras.contains_key(name(c)) {
                    self.db
                        .delete(name(c), &self::id(*id), *durability)
                        .unwrap();
                    self.wrote(*durability);
                }
            }
            Op::PutBatch {
                c,
                ids,
                word,
                durability,
            } => {
                if self.extras.contains_key(name(c)) {
                    let docs: Vec<_> = ids.iter().map(|&n| doc(n, *word)).collect();
                    self.db.put_batch(name(c), &docs, *durability).unwrap();
                    self.wrote(*durability);
                }
            }
            Op::Commit => {
                self.db.commit().unwrap();
                // Every memtable with records is flushed.
                self.buffered = false;
            }
            Op::Merge => {
                let seq = self.db.seq();
                for _ in 0..4 {
                    self.db.merge_step().unwrap();
                    if self.db.seq() != seq {
                        break;
                    }
                }
            }
            Op::Create(c) => {
                if !self.extras.contains_key(name(c)) {
                    self.db.create_collection(schema(name(c), 0)).unwrap();
                    self.extras.insert(name(c), 0);
                }
            }
            Op::Drop(c) => {
                if self.extras.remove(name(c)).is_some() {
                    self.db.drop_collection(name(c)).unwrap();
                    // Every other memtable is flushed; the dropped one's records are gone.
                    self.buffered = false;
                }
            }
            Op::Evolve(c) => {
                if let Some(extras) = self.extras.get_mut(name(c))
                    && *extras < MAX_EXTRAS
                {
                    *extras += 1;
                    let proposed = schema(name(c), *extras);
                    self.db.evolve_collection(&proposed).unwrap();
                    // Every memtable is flushed first.
                    self.buffered = false;
                }
            }
            Op::Restart { power_loss } => {
                // The image first: a process that dies runs no destructor.
                let loss = if *power_loss {
                    CrashLoss::All
                } else {
                    CrashLoss::Nothing
                };
                let file = self.file.crash(loss);
                self.db = Database::open_on(file.clone(), options(self.small_chunks)).unwrap();
                self.file = file;
                self.buffered = false;
            }
            Op::Refresh => panic!("the reader's op"),
        }
    }
}

fn ids() -> impl Iterator<Item = String> {
    (0..IDS).map(id)
}

/// Invariant 1.
fn assert_fresh(
    reader: &ReadOnlyDatabase<SimFile>,
    file: &SimFile,
    ctx: &str,
) -> Result<(), TestCaseError> {
    let fresh = ReadOnlyDatabase::open_on(file.clone(), &AnalyzerRegistry::builtin()).unwrap();
    prop_assert_eq!(reader.seq(), fresh.seq(), "seq: {}", ctx);
    prop_assert_eq!(&*reader.manifest(), &*fresh.manifest(), "manifest: {}", ctx);
    prop_assert_eq!(reader.replayed(), fresh.replayed(), "replayed: {}", ctx);
    prop_assert_eq!(reader.wal_end(), fresh.wal_end(), "WAL end: {}", ctx);
    prop_assert_eq!(reader.skipped_analysis(), fresh.skipped_analysis());
    let (got, want) = (reader.snapshot(), fresh.snapshot());
    let names = |s: &Snapshot| {
        s.collections()
            .map(|(n, _)| n.to_owned())
            .collect::<Vec<_>>()
    };
    prop_assert_eq!(names(&got), names(&want), "collections: {}", ctx);
    for (name, collection) in want.collections() {
        let mine = got.collection(name).unwrap();
        prop_assert_eq!(mine.schema(), collection.schema(), "{}: {}", name, ctx);
        let (m, w) = (mine.memtable(), collection.memtable());
        prop_assert_eq!(m.live_count(), w.live_count(), "{} live: {}", name, ctx);
        prop_assert_eq!(m.doc_count(), w.doc_count(), "{} docs: {}", name, ctx);
        prop_assert_eq!(
            mine.segments().len(),
            collection.segments().len(),
            "{} segments: {}",
            name,
            ctx
        );
        for id in ids() {
            prop_assert_eq!(
                m.lookup(&id),
                w.lookup(&id),
                "{}/{} memtable: {}",
                name,
                id,
                ctx
            );
            prop_assert_eq!(
                mine.get(&id).unwrap(),
                collection.get(&id).unwrap(),
                "{}/{}: {}",
                name,
                id,
                ctx
            );
        }
    }
    Ok(())
}

/// Invariant 2.
fn assert_writer(
    reader: &ReadOnlyDatabase<SimFile>,
    writer: &Writer,
    ctx: &str,
) -> Result<(), TestCaseError> {
    prop_assert_eq!(reader.seq(), writer.db.seq(), "seq: {}", ctx);
    if writer.buffered {
        return Ok(());
    }
    let (got, want) = (reader.snapshot(), writer.db.snapshot());
    for (name, collection) in want.collections() {
        let mine = got.collection(name).unwrap();
        for id in ids() {
            prop_assert_eq!(
                mine.get(&id).unwrap(),
                collection.get(&id).unwrap(),
                "{}/{} against the writer: {}",
                name,
                id,
                ctx
            );
        }
    }
    prop_assert_eq!(got.collections().len(), want.collections().len(), "{}", ctx);
    Ok(())
}

/// A snapshot taken before a refresh, with what it answered then.
struct Taken {
    snapshot: Arc<Snapshot>,
    seq: u64,
    /// Per collection, a memtable view taken then and its answers, and the snapshot's `get` answers.
    collections: Vec<TakenCollection>,
}

struct TakenCollection {
    name: String,
    view: MemtableView,
    live_count: u32,
    lookups: Vec<Option<IdState>>,
    answers: Vec<Option<Document>>,
}

fn take(reader: &ReadOnlyDatabase<SimFile>) -> Taken {
    let snapshot = reader.snapshot();
    let collections = snapshot
        .collections()
        .map(|(name, c)| {
            let view = c.memtable();
            TakenCollection {
                name: name.to_owned(),
                live_count: view.live_count(),
                lookups: ids().map(|id| view.lookup(&id)).collect(),
                answers: ids().map(|id| c.get(&id).unwrap()).collect(),
                view,
            }
        })
        .collect();
    Taken {
        seq: snapshot.seq(),
        snapshot,
        collections,
    }
}

/// Invariant 4: `all_answers` after a refresh that kept nothing of the memtables shared with the new state.
fn assert_kept(taken: &Taken, all_answers: bool, ctx: &str) -> Result<(), TestCaseError> {
    prop_assert_eq!(taken.snapshot.seq(), taken.seq, "{}", ctx);
    for t in &taken.collections {
        let c = taken.snapshot.collection(&t.name).unwrap();
        prop_assert_eq!(
            t.view.live_count(),
            t.live_count,
            "{} view: {}",
            &t.name,
            ctx
        );
        let now = c.memtable();
        for (i, id) in ids().enumerate() {
            prop_assert_eq!(
                &t.view.lookup(&id),
                &t.lookups[i],
                "{}/{} view: {}",
                &t.name,
                id,
                ctx
            );
            if all_answers || now.lookup(&id).is_none() {
                prop_assert_eq!(
                    &c.get(&id).unwrap(),
                    &t.answers[i],
                    "{}/{} old snapshot: {}",
                    &t.name,
                    id,
                    ctx
                );
            }
        }
    }
    Ok(())
}

/// Invariant 3 for an `Incremental` refresh: the segments of an unchanged manifest entry are shared.
fn assert_shared(before: &Snapshot, after: &Snapshot, ctx: &str) -> Result<(), TestCaseError> {
    for (name, new) in after.collections() {
        let Some(old) = before.collection(name) else {
            continue;
        };
        let (old_entry, new_entry) = (
            before.manifest().collection(name).unwrap(),
            after.manifest().collection(name).unwrap(),
        );
        let unchanged = new_entry
            .segments()
            .iter()
            .filter(|e| old_entry.segments().contains(e))
            .count();
        let shared = new
            .segments()
            .iter()
            .filter(|s| old.segments().iter().any(|o| Arc::ptr_eq(o, s)))
            .count();
        prop_assert_eq!(shared, unchanged, "{} shared segments: {}", name, ctx);
    }
    Ok(())
}

/// A reader with a handle of its own on the writer's file: a `SimFile` it is opened on, into which each refresh
/// first copies what the writer's file holds now (a restarted writer's file is another `SimFile`).
struct Reader {
    file: SimFile,
    db: ReadOnlyDatabase<SimFile>,
}

impl Reader {
    fn new(writer: &Writer) -> Self {
        let file = SimFile::from_bytes(writer.file.bytes());
        let db = ReadOnlyDatabase::open_on(file.clone(), &AnalyzerRegistry::builtin()).unwrap();
        Self { file, db }
    }

    /// What its handle reads now: the writer's bytes.
    fn follow(&self, writer: &Writer) {
        let bytes = writer.file.bytes();
        self.file.write_all_at(&bytes, 0).unwrap();
        self.file.set_len(bytes.len() as u64).unwrap();
    }
}

/// One refresh, with every invariant checked; `older` are the snapshots taken before earlier refreshes.
fn refresh(
    handle: &Reader,
    writer: &Writer,
    older: &mut Vec<Taken>,
    kinds: &mut Kinds,
    ctx: &str,
) -> Result<(), TestCaseError> {
    handle.follow(writer);
    let reader = &handle.db;
    let taken = take(reader);
    let replayed = reader.replayed();
    let refreshed = reader.refresh().unwrap();
    kinds.count(refreshed, taken.seq);
    let now = reader.snapshot();
    let ctx = format!("{ctx}, {refreshed:?}");
    match refreshed {
        Refreshed::Unchanged => {
            prop_assert!(Arc::ptr_eq(&now, &taken.snapshot), "swapped: {}", ctx);
            prop_assert_eq!(reader.replayed(), replayed);
        }
        Refreshed::Incremental { seq, records } => {
            prop_assert_eq!(seq, reader.seq());
            prop_assert_eq!(reader.replayed(), replayed + records, "{}", ctx);
            prop_assert!(seq != taken.seq || records > 0, "nothing new: {}", ctx);
            assert_shared(&taken.snapshot, &now, &ctx)?;
        }
        Refreshed::Reloaded { seq } => prop_assert_eq!(seq, reader.seq()),
        other => panic!("unknown outcome {other:?}"),
    }
    assert_fresh(reader, &handle.file, &ctx)?;
    assert_writer(reader, writer, &ctx)?;
    let shares = matches!(refreshed, Refreshed::Incremental { .. });
    assert_kept(&taken, !shares, &ctx)?;
    for (i, old) in older.iter().enumerate() {
        assert_kept(old, false, &format!("{ctx}, snapshot {i}"))?;
    }
    older.push(taken);
    Ok(())
}

/// Runs `ops` on a writer with a reader opened at the start, refreshing at each `Refresh` and at the end.
fn run(small_chunks: bool, ops: &[Op], kinds: &mut Kinds) -> Result<(), TestCaseError> {
    let mut writer = Writer::new(small_chunks);
    let reader = Reader::new(&writer);
    let mut older = Vec::new();
    for (step, op) in ops.iter().enumerate() {
        match op {
            Op::Refresh => refresh(&reader, &writer, &mut older, kinds, &format!("step {step}"))?,
            other => writer.run(other),
        }
    }
    refresh(&reader, &writer, &mut older, kinds, "the end")
}

/// Runs the model with restarts that lose unsynced writes if `power_loss`; returns the outcomes and the case count.
fn model(name: &'static str, normal: u32, power_loss: bool) -> (Kinds, u32) {
    let kinds = Mutex::new(Kinds::default());
    let ran = check(
        name,
        normal,
        (any::<bool>(), prop::collection::vec(op(power_loss), 1..40)),
        |(small_chunks, ops)| {
            let mut mine = Kinds::default();
            let result = run(small_chunks, &ops, &mut mine);
            let mut all = kinds.lock().unwrap();
            all.unchanged += mine.unchanged;
            all.incremental_same_seq += mine.incremental_same_seq;
            all.incremental_new_seq += mine.incremental_new_seq;
            all.reloaded += mine.reloaded;
            result
        },
    );
    let kinds = *kinds.lock().unwrap();
    (kinds, ran)
}

#[test]
fn a_refreshed_reader_equals_a_fresh_open_and_keeps_old_snapshots() {
    let normal = cases(256, 5_000);
    let (kinds, ran) = model(
        "a_refreshed_reader_equals_a_fresh_open_and_keeps_old_snapshots",
        normal,
        false,
    );
    // Invariant 5; a run with fewer cases (a `PROPTEST_CASES` override) may miss one.
    if ran >= normal {
        assert!(kinds.all_occurred(), "an outcome never occurred: {kinds:?}");
    }
}

/// Invariant 5, without the generator: a fixed sequence takes every outcome, in a known order.
#[test]
fn every_refresh_outcome_occurs_in_a_fixed_sequence() {
    let p = |id, durability| Op::Put {
        c: 0,
        id,
        word: id,
        durability,
    };
    let ops = [
        Op::Refresh, // Unchanged
        p(0, Durability::Fsync),
        Op::Refresh, // Incremental, same seq
        p(1, Durability::Buffered),
        Op::Commit,
        Op::Refresh, // Reloaded: the flush passed the chunk replayed
        p(2, Durability::Buffered),
        Op::Commit,
        Op::Refresh, // Incremental, new seq: the flush passed only a chunk never replayed
        Op::Merge,
        Op::Refresh, // Incremental, new seq: a merge
        Op::Create(1),
        Op::Refresh, // Incremental, new seq: a created collection
        Op::Drop(1),
        Op::Refresh, // Reloaded: a dropped collection
        p(3, Durability::Async),
        Op::Evolve(0),
        Op::Refresh, // Reloaded: a schema change
        Op::Refresh, // Unchanged
    ];
    let mut writer = Writer::new(false);
    let reader = Reader::new(&writer);
    let mut older = Vec::new();
    let mut outcomes = Vec::new();
    for (step, op) in ops.iter().enumerate() {
        if let Op::Refresh = op {
            let seq = reader.db.seq();
            let mut kinds = Kinds::default();
            refresh(
                &reader,
                &writer,
                &mut older,
                &mut kinds,
                &format!("step {step}"),
            )
            .unwrap();
            let kind = match kinds {
                Kinds { unchanged: 1, .. } => "unchanged",
                Kinds {
                    incremental_same_seq: 1,
                    ..
                } => "incremental same seq",
                Kinds {
                    incremental_new_seq: 1,
                    ..
                } => "incremental new seq",
                _ => "reloaded",
            };
            outcomes.push((kind, reader.db.seq() > seq));
        } else {
            writer.run(op);
        }
    }
    let kinds: Vec<_> = outcomes.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        kinds,
        [
            "unchanged",
            "incremental same seq",
            "reloaded",
            "incremental new seq",
            "incremental new seq",
            "incremental new seq",
            "reloaded",
            "reloaded",
            "unchanged",
        ]
    );
}

/// Invariants 1 to 4 when a writer restart may lose unsynced writes the reader replayed (a power loss the reader
/// survives, as a reader on another host of a shared disk does): the next history may put other chunks where the
/// lost ones were.
#[test]
fn a_refreshed_reader_equals_a_fresh_open_across_power_losses() {
    model(
        "a_refreshed_reader_equals_a_fresh_open_across_power_losses",
        cases(256, 5_000),
        true,
    );
}
