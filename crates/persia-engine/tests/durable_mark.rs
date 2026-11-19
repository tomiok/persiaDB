//! `Database::durable_mark` never runs ahead of what a crash keeps (SPEC §8.2.1, §9.5; ADR-0051 item 7).
//!
//! `durable_mark` is what the blob pin set waits on before it lets GC sweep a blob a logged document references: a
//! mark that ran ahead of the disk would release a pin while the only record naming the blob can still be lost, and a
//! sweep then deletes a blob a recovered document references. The container lives on a path of a `SimFs`, so
//! compaction runs too; `crash` gives the file system a restart would find.
//!
//! Invariants, for random sequences of puts, deletes and group-commit batches at random durabilities, commits,
//! inline merges, compactions, and crashes (losing every unsynced write, or a seeded subset of it), with or without a
//! fault at a random mutating call (the process dies there):
//! 1. Every operation whose `wal_mark` (taken right after it returned, or failed) is at most the `durable_mark` read
//!    just before the crash survives the reopen; what survives is a prefix of the operations logged since the last
//!    open (the WAL is replayed in order), never a later operation without an earlier one.
//! 2. `durable_mark` never goes down and never passes `wal_mark`; it reaches `wal_mark` after an `Fsync` operation, a
//!    commit and a committed merge; a `Buffered` or `Async` operation leaves its own record above it (rotation and
//!    an `Async` write-out are not syncs); a reopen starts both at the same mark.
//! 3. A compaction is not a sync point for the mark, and a record written (`Async`) or buffered before a compaction
//!    and made durable by a later `Fsync` survives a crash that loses every unsynced write (the compaction moved the
//!    log to a new file: the later sync must cover the record there).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::result_large_err,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_engine::db::{
    CommitPolicy, CompactionOptions, Database, DatabaseError, DatabaseOptions, ManualClock,
    MergeOutcome, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::FlushPolicy;
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::{Durability, WalMark};
use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::container::{Container, Error as ContainerError, Mode, Repair};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

const COLLECTIONS: [&str; 2] = ["a", "b"];
const IDS: u8 = 6;
const PATH: &str = "/durable.persia";

const SEED: u64 = 0x5045_5253_4941_0942; // "PERSIA", 9.4.2

#[cfg(not(feature = "slow"))]
const CASES: u32 = 24;
#[cfg(feature = "slow")]
const CASES: u32 = 1024;

fn config() -> ProptestConfig {
    let env = ProptestConfig::default();
    ProptestConfig {
        cases: if std::env::var_os("PROPTEST_CASES").is_some() {
            env.cases
        } else {
            CASES
        },
        rng_seed: if std::env::var_os("PROPTEST_RNG_SEED").is_some() {
            env.rng_seed
        } else {
            RngSeed::Fixed(SEED)
        },
        ..env
    }
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

/// Small WAL chunks (so records rotate out between syncs), inline merges of two segments, compaction only when asked,
/// and, when `small_memtables`, a size threshold a few puts reach (so a put commits before it logs, SPEC §8.2 step 3).
/// `opens` seeds the segment ids, so a reopen never draws an id an earlier open used (SPEC §8.7.1).
fn options(opens: u64, small_memtables: bool) -> DatabaseOptions {
    let mut policy = CommitPolicy::default();
    if small_memtables {
        policy.flush = FlushPolicy {
            memtable_bytes: 600,
            ..policy.flush
        };
    }
    DatabaseOptions {
        policy,
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)),
        segment_ids: SegmentIds::seeded(opens),
        wal_chunk_target: 200,
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        compaction: CompactionOptions {
            automatic: false,
            ..CompactionOptions::default()
        },
        ..DatabaseOptions::default()
    }
}

/// Documents by collection and id: their body text.
type Docs = BTreeMap<(usize, String), String>;

fn observed(snapshot: &Snapshot) -> Docs {
    let mut docs = Docs::new();
    for (c, name) in COLLECTIONS.iter().enumerate() {
        let collection = snapshot
            .collection(name)
            .unwrap_or_else(|| panic!("{name}: missing"));
        for i in 0..IDS {
            let id = format!("d{i}");
            if let Some(d) = collection.get(&id).unwrap() {
                let Some(Value::String(text)) = d.fields().next().map(|(_, v)| v.clone()) else {
                    panic!("{id}: no body");
                };
                docs.insert((c, id), text);
            }
        }
    }
    docs
}

fn level(n: u8) -> Durability {
    match n % 3 {
        0 => Durability::Buffered,
        1 => Durability::Async,
        _ => Durability::Fsync,
    }
}

#[derive(Debug, Clone)]
enum Op {
    /// Collection, id, durability.
    Put(usize, u8, u8),
    Delete(usize, u8, u8),
    /// A group commit (SPEC §8.2.1): distinct ids, one durability for all.
    Batch(usize, Vec<u8>, u8),
    Commit,
    MergeStep,
    Compact,
    /// Crash, losing every unsynced write (`false`) or a seeded subset (`true`), check invariant 1, go on from the
    /// reopened database.
    Crash(bool),
}

fn arb_op() -> impl Strategy<Value = Op> {
    let c = 0..COLLECTIONS.len();
    prop_oneof![
        8 => (c.clone(), 0..IDS, any::<u8>()).prop_map(|(c, id, l)| Op::Put(c, id, l)),
        3 => (c.clone(), 0..IDS, any::<u8>()).prop_map(|(c, id, l)| Op::Delete(c, id, l)),
        2 => (c, proptest::sample::subsequence((0..IDS).collect::<Vec<_>>(), 2..=4), any::<u8>())
            .prop_map(|(c, ids, l)| Op::Batch(c, ids, l)),
        3 => Just(Op::Commit),
        2 => Just(Op::MergeStep),
        1 => Just(Op::Compact),
        2 => any::<bool>().prop_map(Op::Crash),
    ]
}

/// One document change logged since the last open, with the `wal_mark` that covers it: the mark taken after its
/// operation (for a batch, after the whole batch, so a document counts as required only once the batch is durable).
/// `None` for a change of a batch that failed part way: it may be in the log, but which of its records the mark
/// covers is not known from outside (a failed operation is the last one: the process died there).
#[derive(Debug, Clone)]
struct Logged {
    mark: Option<WalMark>,
    key: (usize, String),
    /// `None` for a delete.
    text: Option<String>,
}

fn apply(docs: &mut Docs, change: &Logged) {
    match &change.text {
        Some(text) => {
            docs.insert(change.key.clone(), text.clone());
        }
        None => {
            docs.remove(&change.key);
        }
    }
}

struct Run {
    fs: SimFs,
    db: Database<SimFsFile>,
    opens: u64,
    small_memtables: bool,
    /// What the database held at its open.
    base: Docs,
    /// The changes logged since, in order.
    logged: Vec<Logged>,
    /// The highest `durable_mark` seen since the open (invariant 2: it never goes down).
    durable: WalMark,
}

impl Run {
    fn open(fs: &SimFs, opens: u64, small_memtables: bool) -> Database<SimFsFile> {
        let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
        let options = options(opens, small_memtables);
        match Container::open_in(Arc::clone(&vfs), PATH, Mode::ReadWrite) {
            Ok(container) => Database::open_container(container, options).unwrap(),
            // Unsynced sectors kept out of order leave a hole before non-zero bytes (SPEC §4.4, ADR-0014): every
            // durable byte precedes it, so quarantining the tail keeps them all.
            Err(ContainerError::CorruptTail { .. }) => {
                let container =
                    Container::open_in_with(vfs, PATH, Mode::ReadWrite, Repair::Quarantine)
                        .unwrap();
                Database::open_container(container, options).unwrap()
            }
            Err(e) => panic!("reopen: {e}"),
        }
    }

    fn create(small_memtables: bool) -> Self {
        let fs = SimFs::new();
        let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
        let container = Container::create_in(vfs, PATH).unwrap();
        let mut db = Database::create_container(container, options(1, small_memtables)).unwrap();
        for c in COLLECTIONS {
            db.create_collection(schema(c)).unwrap();
        }
        let durable = db.durable_mark();
        Self {
            fs,
            db,
            opens: 1,
            small_memtables,
            base: Docs::new(),
            logged: Vec::new(),
            durable,
        }
    }

    /// Invariant 2's bounds, after every operation.
    fn check_bounds(&mut self, ctx: &str) -> Result<(), TestCaseError> {
        let (durable, mark) = (self.db.durable_mark(), self.db.wal_mark());
        prop_assert!(durable >= self.durable, "{}: durable_mark went down", ctx);
        prop_assert!(durable <= mark, "{}: durable_mark passed wal_mark", ctx);
        self.durable = durable;
        Ok(())
    }

    /// Crashes with `loss`, reopens, and checks invariant 1: the reopened documents are the base with a prefix of the
    /// logged changes that holds every change whose mark the last `durable_mark` covers.
    fn crash(&mut self, loss: CrashLoss, ctx: &str) -> Result<(), TestCaseError> {
        let crashed = self.fs.crash(loss);
        self.opens += 1;
        let (db, found) = self.check_crashed(&crashed, loss, ctx)?;
        self.fs = crashed;
        self.db = db;
        self.base = found;
        self.logged.clear();
        self.durable = self.db.durable_mark();
        Ok(())
    }

    /// Invariant 1 for `crashed`, what a crash with `loss` left of `self.fs`, against `self.db`'s `durable_mark` now:
    /// the reopened documents are the base with a prefix of the logged changes that holds every change whose mark
    /// it covers. Returns the reopened database and its documents.
    fn check_crashed(
        &self,
        crashed: &SimFs,
        loss: CrashLoss,
        ctx: &str,
    ) -> Result<(Database<SimFsFile>, Docs), TestCaseError> {
        let durable = self.db.durable_mark();
        let required = self
            .logged
            .iter()
            .take_while(|l| l.mark.is_some_and(|m| m <= durable))
            .count();
        // Marks are taken in order, so the covered changes are a prefix.
        prop_assert!(
            self.logged[required..]
                .iter()
                .all(|l| l.mark.is_none_or(|m| m > durable)),
            "{}: marks out of order",
            ctx
        );
        let db = Self::open(crashed, self.opens, self.small_memtables);
        let found = observed(&db.snapshot());
        let mut state = self.base.clone();
        for l in &self.logged[..required] {
            apply(&mut state, l);
        }
        let mut matched = state == found;
        for l in &self.logged[required..] {
            if matched {
                break;
            }
            apply(&mut state, l);
            matched = state == found;
        }
        prop_assert!(
            matched,
            "{}, {:?}: the reopen shows {:?}, not the base with a prefix of at least the {} changes durable_mark \
             covered of {:?} (base {:?})",
            ctx,
            loss,
            found,
            required,
            self.logged,
            self.base
        );
        // Invariant 2: a new writer starts with nothing above its durable mark.
        prop_assert_eq!(
            db.durable_mark(),
            db.wal_mark(),
            "{}: after the reopen",
            ctx
        );
        Ok((db, found))
    }

    /// Runs `op`; `Err(())` when it failed (the process is dead: a fault fired).
    fn step(&mut self, step: usize, op: &Op, ctx: &str) -> Result<Result<(), ()>, TestCaseError> {
        let level_of = |l: &u8| level(*l);
        let ok = match op {
            Op::Put(c, id, l) | Op::Delete(c, id, l) => {
                let (name, id, durability) = (COLLECTIONS[*c], format!("d{id}"), level_of(l));
                let put = matches!(op, Op::Put(..));
                let text = format!("{step}");
                let before = self.db.wal_mark();
                let result = if put {
                    self.db.put(name, &doc(&id, &text), durability)
                } else {
                    self.db.delete(name, &id, durability)
                };
                // A failed operation that appended its record (then failed to sync it) may be in the log: required
                // once the mark covers it. One that failed before appending (in the size-triggered commit that runs
                // before it, SPEC §8.2 step 3) is not in the log at all.
                let mark = self.db.wal_mark();
                if mark > before {
                    self.logged.push(Logged {
                        mark: Some(mark),
                        key: (*c, id),
                        text: put.then_some(text),
                    });
                }
                if result.is_ok() {
                    self.check_level(durability, ctx)?;
                }
                result.is_ok()
            }
            Op::Batch(c, ids, l) => {
                let durability = level_of(l);
                let before = self.db.wal_mark();
                let docs: Vec<Document> = ids
                    .iter()
                    .map(|id| doc(&format!("d{id}"), &format!("{step} batch")))
                    .collect();
                let result = self.db.put_batch(COLLECTIONS[*c], &docs, durability);
                let mark = self.db.wal_mark();
                // A batch that failed part way: what it appended are candidates, never required (see `Logged`); the
                // documents after them never reached the log, which a prefix allows.
                let appended = mark > before;
                let mark = result.is_ok().then_some(mark);
                for id in ids.iter().filter(|_| appended) {
                    self.logged.push(Logged {
                        mark,
                        key: (*c, format!("d{id}")),
                        text: Some(format!("{step} batch")),
                    });
                }
                if result.is_ok() {
                    self.check_level(durability, ctx)?;
                }
                result.is_ok()
            }
            Op::Commit => {
                let result = self.db.commit();
                if result.is_ok() {
                    prop_assert_eq!(
                        self.db.durable_mark(),
                        self.db.wal_mark(),
                        "{}: a commit syncs the WAL first (SPEC §4.5 step 1)",
                        ctx
                    );
                }
                result.is_ok()
            }
            Op::MergeStep => match self.db.merge_step() {
                Ok(outcomes) => {
                    if outcomes
                        .iter()
                        .any(|o| matches!(o, MergeOutcome::Failed { .. }))
                    {
                        // Only a dead file system fails a merge of healthy segments.
                        return Ok(Err(()));
                    }
                    if outcomes
                        .iter()
                        .any(|o| matches!(o, MergeOutcome::Committed { .. }))
                    {
                        prop_assert_eq!(
                            self.db.durable_mark(),
                            self.db.wal_mark(),
                            "{}: a merge commit syncs the WAL first",
                            ctx
                        );
                    }
                    true
                }
                Err(_) => false,
            },
            Op::Compact => {
                let before = self.db.durable_mark();
                let result = self.db.compact();
                // Invariant 3: not a sync point for the mark.
                prop_assert_eq!(self.db.durable_mark(), before, "{}: compaction", ctx);
                result.is_ok()
            }
            Op::Crash(seeded) => {
                let loss = if *seeded {
                    CrashLoss::Seeded(u64::try_from(step).unwrap())
                } else {
                    CrashLoss::All
                };
                self.crash(loss, ctx)?;
                true
            }
        };
        self.check_bounds(ctx)?;
        Ok(if ok { Ok(()) } else { Err(()) })
    }

    /// Invariant 2 after an operation logged at `durability` returned `Ok`.
    fn check_level(&self, durability: Durability, ctx: &str) -> Result<(), TestCaseError> {
        let (durable, mark) = (self.db.durable_mark(), self.db.wal_mark());
        match durability {
            Durability::Fsync => prop_assert_eq!(durable, mark, "{}: Fsync", ctx),
            Durability::Buffered | Durability::Async => {
                prop_assert!(
                    durable < mark,
                    "{}: {:?} made its own record durable",
                    ctx,
                    durability
                );
            }
        }
        Ok(())
    }
}

fn run_case(
    ops: &[Op],
    small_memtables: bool,
    fault: Option<(u64, bool)>,
    loss_seed: u64,
) -> Result<(), TestCaseError> {
    let mut run = Run::create(small_memtables);
    if let Some((at, apply_first)) = fault {
        let n = run.fs.mutations() + at;
        run.fs.set_fault(Some(if apply_first {
            SimFault::ApplyThenFail(n)
        } else {
            SimFault::FailAt(n)
        }));
    }
    for (step, op) in ops.iter().enumerate() {
        let ctx = format!("step {step} {op:?}, fault {fault:?}, small {small_memtables}");
        if run.step(step, op, &ctx)?.is_err() {
            // The process died at the fault: what the disk kept must hold every change the mark covered.
            prop_assert!(
                run.fs.crashed(),
                "{}: an operation failed with no fault fired",
                ctx
            );
            run.opens += 1;
            for loss in [
                CrashLoss::All,
                CrashLoss::Nothing,
                CrashLoss::Seeded(loss_seed),
            ] {
                let crashed = run.fs.crash(loss);
                run.check_crashed(&crashed, loss, &format!("{ctx}, after the fault"))?;
            }
            return Ok(());
        }
    }
    run.crash(CrashLoss::All, "the end")
}

/// How many mutating calls `ops` make after the database is set up, run without a fault (the run is deterministic:
/// seeded segment ids, a manual clock).
fn mutations_of(ops: &[Op], small_memtables: bool) -> u64 {
    let mut run = Run::create(small_memtables);
    let start = run.fs.mutations();
    for (step, op) in ops.iter().enumerate() {
        assert!(
            matches!(run.step(step, op, "dry run"), Ok(Ok(()))),
            "dry run"
        );
    }
    run.fs.mutations() - start
}

/// The ops of a fault case: no `Crash` op, since the fault is the crash (a reopened file system has no fault).
fn without_crashes(ops: Vec<Op>) -> Vec<Op> {
    ops.into_iter()
        .filter(|op| !matches!(op, Op::Crash(_)))
        .collect()
}

proptest! {
    #![proptest_config(config())]

    /// Invariants 1 to 3, with crashes between operations.
    #[test]
    fn what_durable_mark_covers_survives_every_crash(
        ops in prop::collection::vec(arb_op(), 1..60),
        small_memtables in any::<bool>(),
    ) {
        run_case(&ops, small_memtables, None, 0)?;
    }

    /// Invariants 1 and 2 with a fault at a random mutating call of the run (the process dies there): whatever call
    /// it died at (a chunk write, a sync, a commit's slot, a compaction's rename), the mark never ran ahead of the
    /// disk.
    #[test]
    fn what_durable_mark_covers_survives_a_fault_at_any_call(
        ops in prop::collection::vec(arb_op(), 1..60).prop_map(without_crashes),
        small_memtables in any::<bool>(),
        permille in 0..1000_u64,
        apply_first in any::<bool>(),
        loss_seed in any::<u64>(),
    ) {
        let total = mutations_of(&ops, small_memtables);
        prop_assume!(total > 0);
        let at = total * permille / 1000;
        run_case(&ops, small_memtables, Some((at, apply_first)), loss_seed)?;
    }
}

/// Invariants 1 and 2 at every mutating call of one workload that mixes the three durabilities, a group commit, a
/// rotation, a size-triggered flush, a commit, a merge and a compaction, cleanly failed or applied then failed.
#[test]
fn what_durable_mark_covers_survives_a_fault_at_every_call() {
    use Op::{Batch, Commit, Compact, Delete, MergeStep, Put};
    let ops = [
        Put(0, 0, 0),
        Put(0, 1, 1),
        Put(1, 0, 2),
        Batch(0, vec![2, 3, 4], 0),
        Put(1, 1, 0),
        Commit,
        Put(0, 0, 1),
        Delete(0, 1, 0),
        Batch(1, vec![0, 2], 2),
        Put(0, 5, 0),
        Commit,
        MergeStep,
        Put(0, 3, 1),
        Compact,
        Put(1, 3, 0),
        Delete(1, 0, 1),
        Put(0, 4, 2),
        Put(1, 4, 0),
    ];
    // Both thresholds take ~6 s in a debug build: the default run keeps the default one (the random cases reach the small one).
    let thresholds: &[bool] = if cfg!(feature = "slow") {
        &[false, true]
    } else {
        &[false]
    };
    for &small_memtables in thresholds {
        let total = mutations_of(&ops, small_memtables);
        for at in 0..total {
            for apply_first in [false, true] {
                run_case(&ops, small_memtables, Some((at, apply_first)), at).unwrap_or_else(|e| {
                    panic!("call {at}/{total}, apply first {apply_first}: {e}")
                });
            }
        }
    }
}

/// Invariant 3, spelled out: a record logged `Async` (written, not synced) or `Buffered` before a compaction, then
/// made durable by an `Fsync` put after it, survives losing every unsynced write. The compaction moved the log to a
/// new file; if it dropped the unsynced tail, the later sync would claim records the disk no longer has.
#[test]
fn a_record_before_a_compaction_is_durable_after_a_later_fsync() {
    for first in [Durability::Buffered, Durability::Async] {
        let mut run = Run::create(false);
        run.db
            .put("a", &doc("d0", "committed"), Durability::Fsync)
            .unwrap();
        run.db.commit().unwrap();
        run.db
            .put("a", &doc("d0", "replaced"), Durability::Fsync)
            .unwrap();
        run.db.commit().unwrap();
        run.db.put("a", &doc("d1", "before"), first).unwrap();
        let before = run.db.wal_mark();
        assert!(run.db.durable_mark() < before, "{first:?}");
        let done = run.db.compact().unwrap();
        assert!(
            done.after.total < done.before.total,
            "{first:?}: nothing compacted"
        );
        assert!(
            run.db.durable_mark() < before,
            "{first:?}: compaction moved the mark"
        );
        run.db
            .put("b", &doc("d2", "after"), Durability::Fsync)
            .unwrap();
        assert!(run.db.durable_mark() >= before, "{first:?}");
        run.fs = run.fs.crash(CrashLoss::All);
        let db = Run::open(&run.fs, 2, false);
        let docs = observed(&db.snapshot());
        assert_eq!(
            docs,
            Docs::from([
                ((0, "d0".into()), "replaced".into()),
                ((0, "d1".into()), "before".into()),
                ((1, "d2".into()), "after".into()),
            ]),
            "{first:?}"
        );
    }
}

/// Invariant 2 for every call a sync can fail at: an `Fsync` put whose write-out or sync fails (cleanly, or after
/// it took effect) leaves `durable_mark` where it was, and a crash that loses every unsynced write keeps exactly the
/// records it covered.
#[test]
fn a_failed_fsync_leaves_the_mark_and_the_disk_agrees() {
    // How many mutating calls the Fsync put makes after two buffered ones.
    let calls = {
        let mut run = Run::create(false);
        run.db
            .put("a", &doc("d0", "x"), Durability::Buffered)
            .unwrap();
        run.db.put("a", &doc("d1", "x"), Durability::Async).unwrap();
        let start = run.fs.mutations();
        run.db.put("a", &doc("d2", "x"), Durability::Fsync).unwrap();
        run.fs.mutations() - start
    };
    assert!(calls >= 2, "a write-out and a sync: {calls}");
    for point in 0..calls {
        for apply_first in [false, true] {
            let ctx = format!("call {point}, apply first {apply_first}");
            let mut run = Run::create(false);
            run.db.put("a", &doc("d0", "x"), Durability::Fsync).unwrap();
            let durable = run.db.durable_mark();
            run.db
                .put("a", &doc("d1", "x"), Durability::Buffered)
                .unwrap();
            run.db.put("b", &doc("d2", "x"), Durability::Async).unwrap();
            let n = run.fs.mutations() + point;
            run.fs.set_fault(Some(if apply_first {
                SimFault::ApplyThenFail(n)
            } else {
                SimFault::FailAt(n)
            }));
            let error = run
                .db
                .put("a", &doc("d3", "x"), Durability::Fsync)
                .unwrap_err();
            assert!(matches!(error, DatabaseError::Wal(_)), "{ctx}: {error}");
            assert_eq!(
                run.db.durable_mark(),
                durable,
                "{ctx}: a failed sync moved the mark"
            );
            assert!(matches!(
                run.db.put("a", &doc("d4", "x"), Durability::Fsync),
                Err(DatabaseError::Poisoned)
            ));
            assert_eq!(run.db.durable_mark(), durable, "{ctx}");
            let db = Run::open(&run.fs.crash(CrashLoss::All), 2, false);
            let docs = observed(&db.snapshot());
            assert!(docs.contains_key(&(0, "d0".into())), "{ctx}: {docs:?}");
        }
    }
}

/// A commit or a compaction failed at any of its mutating calls (cleanly, or after the call took effect) never
/// moves `durable_mark` ahead of the disk: a compaction never moves it at all, and a commit moves it only once its WAL
/// sync succeeded (SPEC §4.5 step 1), so the record pending before it survives losing every unsynced write. Both
/// outcomes occur: a fault at the WAL sync, or before, leaves the mark; one after it finds it moved.
#[test]
fn a_failed_commit_or_compaction_never_moves_the_mark_ahead_of_the_disk() {
    #[derive(Debug, Clone, Copy)]
    enum Step {
        Commit,
        Compact,
    }
    // A database with dead frames (a replaced, committed document) and one pending record logged at `pending`.
    let setup = |pending: Durability| {
        let mut run = Run::create(false);
        for text in ["one", "two"] {
            run.db
                .put("a", &doc("d0", text), Durability::Fsync)
                .unwrap();
            run.db.commit().unwrap();
        }
        run.db.put("b", &doc("d1", "pending"), pending).unwrap();
        assert!(run.db.durable_mark() < run.db.wal_mark());
        run
    };
    let attempt = |run: &mut Run, step: Step| match step {
        Step::Commit => run.db.commit().map(drop),
        Step::Compact => run.db.compact().map(drop),
    };
    for pending in [Durability::Buffered, Durability::Async] {
        for step in [Step::Commit, Step::Compact] {
            let calls = {
                let mut run = setup(pending);
                let start = run.fs.mutations();
                attempt(&mut run, step).unwrap();
                run.fs.mutations() - start
            };
            let (mut kept, mut moved) = (0, 0);
            for point in 0..calls {
                for apply_first in [false, true] {
                    let ctx = format!(
                        "{pending:?} {step:?}, call {point}/{calls}, apply first {apply_first}"
                    );
                    let mut run = setup(pending);
                    let before = run.db.durable_mark();
                    let n = run.fs.mutations() + point;
                    run.fs.set_fault(Some(if apply_first {
                        SimFault::ApplyThenFail(n)
                    } else {
                        SimFault::FailAt(n)
                    }));
                    assert!(attempt(&mut run, step).is_err(), "{ctx}: no failure");
                    let after = run.db.durable_mark();
                    assert!(after <= run.db.wal_mark(), "{ctx}");
                    let survived =
                        observed(&Run::open(&run.fs.crash(CrashLoss::All), 2, false).snapshot())
                            .contains_key(&(1, "d1".into()));
                    match step {
                        Step::Compact => {
                            assert_eq!(after, before, "{ctx}: a compaction moved the mark");
                        }
                        Step::Commit if after == before => kept += 1,
                        Step::Commit => {
                            assert_eq!(after, run.db.wal_mark(), "{ctx}");
                            assert!(survived, "{ctx}: the mark covers a record the crash lost");
                            moved += 1;
                        }
                    }
                }
            }
            if matches!(step, Step::Commit) {
                assert!(
                    kept > 0 && moved > 0,
                    "{pending:?}: kept {kept}, moved {moved}"
                );
            }
        }
    }
}
