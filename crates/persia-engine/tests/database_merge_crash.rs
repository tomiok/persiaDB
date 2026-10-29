//! Crash safety and interleavings of background merges (ROADMAP 6.5.3; SPEC §4.5, §8.5 "Scheduling", §8.7.2,
//! ADR-0032).
//!
//! The container's file is a `SimFile`: a fault fails one mutating call and every later one (the process dies there),
//! and `crash` gives the file a restart would find (see `database_crash.rs`).
//!
//! Invariants:
//! 1. A merge commit is the commit protocol (SPEC §4.5): a fault at any of its writes or syncs (segment frame, WAL
//!    commit point, sync, manifest frame, slot write) poisons the database (every later call is
//!    `DatabaseError::Poisoned`) and publishes nothing; a reopen after any crash finds either the manifest before the
//!    merge or the one after it, never a mix, and in both cases the same live documents.
//! 2. A merge commit syncs the WAL before its manifest (`commit_point`): a `Durability::Buffered` write made before the
//!    merge step is durable once the merge commit is, and a reopen that finds the merged manifest finds every
//!    buffered write too.
//! 3. A merge result that conflicts (a commit deleted from an input meanwhile) is discarded before anything is
//!    written (ADR-0032 item 4).
//! 4. Random interleavings of writes, commits, merge steps, cancels, finishes and reopens (inline and threaded)
//!    always read the documents of a `BTreeMap` model, also after a reopen that loses every unsynced write; a
//!    committed merge never revives a deleted id; segment ids in the manifest stay unique; merges in flight never
//!    exceed the merge threads.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::result_large_err,
    missing_docs
)] // Tests: inputs are fixed or generated.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use persia_engine::db::{
    Database, DatabaseError, DatabaseOptions, ManualClock, MergeOutcome, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::manifest::{Manifest, SegmentEntry};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Container, ContainerFile, Error as ContainerError, Mode, Repair};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::SimFs;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

const COLLECTIONS: [&str; 2] = ["a", "b"];
const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

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

/// Two segments per tier merge, so a couple of commits give a merge. `ids` seeds the segment ids: each open gets its
/// own, so a reopen does not draw the ids of segments merged away before it (SPEC §8.7.1: ids are never reused).
fn options(threads: usize, ids: u64) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)),
        segment_ids: SegmentIds::seeded(ids),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads,
        },
        ..DatabaseOptions::default()
    }
}

/// Documents by collection and id: their body text.
type Docs = BTreeMap<&'static str, BTreeMap<String, String>>;

/// The documents a snapshot reads, by `CollectionSnapshot::get` of every id the memtable or a segment holds; checks
/// that no id is live in two segments (SPEC §8.3) and that `get` agrees with the segment views.
fn observed(snapshot: &Snapshot) -> Docs {
    let mut docs = Docs::new();
    for name in COLLECTIONS {
        let c = snapshot
            .collection(name)
            .unwrap_or_else(|| panic!("{name}: missing"));
        let mut ids = BTreeSet::new();
        let mut in_segments = BTreeMap::new();
        for view in c.segment_views() {
            for docid in 0..view.doc_count() {
                let id = view.segment().id_of(docid).unwrap();
                if view.is_live(docid) {
                    let d = view.document(docid).unwrap();
                    assert!(
                        in_segments.insert(id.clone(), d).is_none(),
                        "{name}: {id} is live in two segments"
                    );
                }
                ids.insert(id);
            }
        }
        let memtable = c.memtable();
        for docid in 0..memtable.doc_count() {
            if memtable.is_live(docid) {
                ids.insert(memtable.document(docid).unwrap().id().to_owned());
            }
        }
        let out = docs.entry(name).or_default();
        for id in ids {
            let Some(d) = c.get(&id).unwrap() else {
                continue;
            };
            let Some(Value::String(text)) = d.fields().next().map(|(_, v)| v.clone()) else {
                panic!("{id}: no body");
            };
            out.insert(id, text);
        }
        // An id the memtable neither wrote nor deleted reads as its live segment copy.
        for (id, d) in &in_segments {
            if memtable.lookup(id).is_none() {
                assert_eq!(c.get(id).unwrap().as_ref(), Some(d), "{name}: {id}");
            }
        }
    }
    docs
}

/// Each collection's segment entries: what decides "before or after the merge".
type Segments = BTreeMap<String, Vec<SegmentEntry>>;

fn segments(manifest: &Manifest) -> Segments {
    manifest
        .collections()
        .map(|c| (c.name().to_owned(), c.segments().to_vec()))
        .collect()
}

/// Segment ids are unique across the whole manifest (SPEC §8.7.1).
fn assert_unique_ids(manifest: &Manifest, ctx: &str) {
    let mut ids = BTreeSet::new();
    for c in manifest.collections() {
        for s in c.segments() {
            assert!(ids.insert(s.id), "{ctx}: segment {:?} listed twice", s.id);
        }
    }
}

// --- 1, 2: a fault at every write of a merge commit ------------------------------------------------------------

/// The base: `a` has two committed segments, the first with a sidecar past the deletion threshold, and `b` one
/// segment. With `segments_per_merge` 2 the policy plans exactly one merge, of `a`'s two segments.
fn base() -> (Vec<u8>, Docs) {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [7; 16], options(0, 1)).unwrap();
    for c in COLLECTIONS {
        db.create_collection(schema(c)).unwrap();
    }
    let mut docs = Docs::new();
    let mut put = |db: &mut Database<SimFile>, c: &'static str, id: &str, text: &str| {
        db.put(c, &doc(id, text), Durability::Buffered).unwrap();
        docs.entry(c).or_default().insert(id.into(), text.into());
    };
    for (id, text) in [("a0", "alpha zero"), ("a1", "alpha one"), ("a2", "beta")] {
        put(&mut db, "a", id, text);
    }
    put(&mut db, "b", "b0", "gamma");
    db.commit().unwrap();
    put(&mut db, "a", "a3", "delta three");
    put(&mut db, "a", "a4", "alpha four");
    db.commit().unwrap();
    db.delete("a", "a0", Durability::Buffered).unwrap();
    docs.get_mut("a").unwrap().remove("a0");
    db.commit().unwrap();
    assert_eq!(observed(&db.snapshot()), docs);
    let entries = &segments(db.manifest())["a"];
    assert_eq!(entries.len(), 2);
    assert!(entries[0].tombstones.is_some());
    assert!(file.durable_bytes() == file.bytes(), "the base is synced");
    (file.durable_bytes(), docs)
}

/// The buffered (unsynced) writes made before the merge step: an upsert into `a`, a delete of an id of the merge's
/// input, a put into `b`.
#[derive(Debug, Clone, Copy)]
enum Buffered {
    Put(&'static str, &'static str, &'static str),
    Delete(&'static str, &'static str),
}

const BUFFERED: [Buffered; 3] = [
    Buffered::Put("a", "a1", "alpha one replaced"),
    Buffered::Delete("a", "a3"),
    Buffered::Put("b", "b1", "gamma one"),
];

fn apply_buffered(docs: &mut Docs, w: Buffered) {
    match w {
        Buffered::Put(c, id, text) => {
            docs.entry(c).or_default().insert(id.into(), text.into());
        }
        Buffered::Delete(c, id) => {
            docs.entry(c).or_default().remove(id);
        }
    }
}

/// Opens the base, makes the buffered writes, and returns the database and its file.
fn prepared(base: &[u8], threads: usize) -> (SimFile, Database<SimFile>) {
    let file = SimFile::from_bytes(base.to_vec());
    let mut db = Database::open_on(file.clone(), options(threads, 2)).unwrap();
    for w in BUFFERED {
        match w {
            Buffered::Put(c, id, text) => db.put(c, &doc(id, text), Durability::Buffered),
            Buffered::Delete(c, id) => db.delete(c, id, Durability::Buffered),
        }
        .unwrap();
    }
    (file, db)
}

/// Runs the merges: one inline step, or (threaded) every merge to the end.
fn run_merges(
    db: &mut Database<SimFile>,
    threads: usize,
) -> Result<Vec<MergeOutcome>, DatabaseError> {
    if threads == 0 {
        db.merge_step()
    } else {
        db.finish_merges()
    }
}

/// What a reopen found: documents, segments and `seq`.
#[derive(Debug, PartialEq)]
struct Found {
    docs: Docs,
    segments: Segments,
    seq: u64,
}

/// Opens the database in `crashed` (by path with `Repair::Quarantine` when a crash left a corrupt tail, as in
/// `database_crash.rs`), records what it shows, then checks recovery is idempotent and the merge can resume: a
/// commit, finishing the merges and another reopen after losing everything unsynced keep the documents.
fn reopen(crashed: &SimFile, ctx: &str, quarantined: &mut usize) -> Found {
    match Database::open_on(crashed.clone(), options(0, 3)) {
        Ok(db) => {
            let found = found(&db, ctx);
            drop(resume(db, ctx, &found.docs));
            // `resume` wrote through a clone of `crashed`, which shares its file.
            let db = Database::open_on(crashed.crash(CrashLoss::All), options(0, 4)).unwrap();
            assert_eq!(
                observed(&db.snapshot()),
                found.docs,
                "{ctx}: after resuming and reopening"
            );
            found
        }
        Err(DatabaseError::Container(ContainerError::CorruptTail { .. })) => {
            *quarantined += 1;
            let fs = SimFs::new();
            let path = Path::new("/db.persia");
            let file = fs.create_new(path).unwrap();
            file.write_all_at(&crashed.bytes(), 0).unwrap();
            file.sync_data().unwrap();
            fs.sync_dir(Path::new("/")).unwrap();
            let vfs: Arc<dyn Vfs<File = _>> = Arc::new(fs.clone());
            let container = Container::open_in_with(vfs, path, Mode::ReadWrite, Repair::Quarantine)
                .unwrap_or_else(|e| panic!("{ctx}: quarantining open: {e}"));
            let db = Database::open_container(container, options(0, 3))
                .unwrap_or_else(|e| panic!("{ctx}: open after quarantine: {e}"));
            let found = found(&db, ctx);
            drop(resume(db, ctx, &found.docs));
            let vfs: Arc<dyn Vfs<File = _>> = Arc::new(fs.crash(CrashLoss::All));
            let container = Container::open_in(vfs, path, Mode::ReadWrite).unwrap();
            let db = Database::open_container(container, options(0, 4)).unwrap();
            assert_eq!(
                observed(&db.snapshot()),
                found.docs,
                "{ctx}: after resuming and reopening"
            );
            found
        }
        Err(e) => panic!("{ctx}: reopen failed: {e}"),
    }
}

fn found<F: ContainerFile + Sync>(db: &Database<F>, ctx: &str) -> Found {
    assert_unique_ids(db.manifest(), ctx);
    Found {
        docs: observed(&db.snapshot()),
        segments: segments(db.manifest()),
        seq: db.seq(),
    }
}

/// Commits what the reopen replayed and finishes the merges: the documents stay.
fn resume<F: ContainerFile + Sync>(mut db: Database<F>, ctx: &str, docs: &Docs) -> Database<F> {
    db.commit()
        .unwrap_or_else(|e| panic!("{ctx}: commit after reopen: {e}"));
    assert_eq!(&observed(&db.snapshot()), docs, "{ctx}: after the commit");
    let outcomes = db
        .finish_merges()
        .unwrap_or_else(|e| panic!("{ctx}: merging after reopen: {e}"));
    for o in &outcomes {
        assert!(matches!(o, MergeOutcome::Committed { .. }), "{ctx}: {o:?}");
    }
    assert_eq!(&observed(&db.snapshot()), docs, "{ctx}: after merging");
    assert_unique_ids(db.manifest(), ctx);
    db
}

/// Invariant 2, without faults: after a merge commit, a crash that loses every unsynced write keeps the buffered
/// writes made before the merge step, and the merged manifest.
#[test]
fn a_merge_commit_makes_earlier_buffered_writes_durable() {
    let (base, committed) = base();
    let mut full = committed.clone();
    for w in BUFFERED {
        apply_buffered(&mut full, w);
    }
    for threads in [0, 1] {
        let (file, mut db) = prepared(&base, threads);
        assert_eq!(observed(&db.snapshot()), full, "threads {threads}");
        // Without the merge, the buffered writes are not durable: the test would prove nothing otherwise.
        let lost = Database::open_on(file.crash(CrashLoss::All), options(0, 9)).unwrap();
        assert_eq!(observed(&lost.snapshot()), committed, "threads {threads}");
        drop(lost);

        let seq = db.seq();
        let outcomes = run_merges(&mut db, threads).unwrap();
        assert!(
            matches!(&outcomes[..], [MergeOutcome::Committed { seq: s, output: Some(_), inputs, .. }]
                if *s == seq + 1 && inputs.len() == 2),
            "threads {threads}: {outcomes:?}"
        );
        assert_eq!(observed(&db.snapshot()), full, "threads {threads}");
        let after = segments(db.manifest());
        assert_eq!(after["a"].len(), 1, "threads {threads}");
        assert_eq!(after["a"][0].tombstones, None);
        drop(db);

        let db = Database::open_on(file.crash(CrashLoss::All), options(0, 9)).unwrap();
        assert_eq!(db.seq(), seq + 1, "threads {threads}");
        assert_eq!(segments(db.manifest()), after, "threads {threads}");
        assert_eq!(observed(&db.snapshot()), full, "threads {threads}");
    }
}

/// Invariants 1 and 2: a fault (failing without effect, or taking effect then failing) at every mutating call of a
/// merge commit, inline and threaded, then a crash keeping none, all or a seeded subset of the unsynced writes.
#[test]
fn a_crash_at_any_write_of_a_merge_commit_gives_the_old_or_the_new_manifest() {
    let (base, committed) = base();
    // The documents with the first `k` buffered writes: what a reopen may find (WAL records replay as a prefix).
    let prefixes: Vec<Docs> = (0..=BUFFERED.len())
        .map(|k| {
            let mut docs = committed.clone();
            for w in &BUFFERED[..k] {
                apply_buffered(&mut docs, *w);
            }
            docs
        })
        .collect();
    let full = prefixes[BUFFERED.len()].clone();

    let mut checked = 0;
    let mut quarantined = 0;
    let mut saw_old = 0;
    let mut saw_new = 0;
    for threads in [0, 1] {
        // The merge's mutating calls and its manifests, without faults.
        let (file, mut db) = prepared(&base, threads);
        let pre = segments(db.manifest());
        let pre_seq = db.seq();
        let start = file.mutations();
        let outcomes = run_merges(&mut db, threads).unwrap();
        assert!(
            matches!(&outcomes[..], [MergeOutcome::Committed { .. }]),
            "{outcomes:?}"
        );
        let total = file.mutations() - start;
        let post = segments(db.manifest());
        assert_ne!(pre, post);
        assert_eq!(pre["b"], post["b"], "the merge touches `a` only");
        drop(db);
        // A segment frame, the WAL chunk, a sync, the manifest frame, the slot and its sync.
        assert!(total >= 5, "{total} mutating calls");

        for point in 0..total {
            for apply_first in [false, true] {
                let (file, mut db) = prepared(&base, threads);
                let at = file.mutations() + point;
                file.set_fault(Some(if apply_first {
                    SimFault::ApplyThenFail(at)
                } else {
                    SimFault::FailAt(at)
                }));
                let result = run_merges(&mut db, threads);
                let ctx =
                    format!("threads {threads}, call {point}/{total}, apply first {apply_first}");
                let error = match result {
                    Ok(outcomes) => panic!("{ctx}: no fault: {outcomes:?}"),
                    Err(e) => e,
                };
                assert!(!matches!(error, DatabaseError::Poisoned), "{ctx}: {error}");
                assert!(file.crashed(), "{ctx}");
                // Nothing was published: the writer still shows the state before the merge.
                assert_eq!(
                    segments(db.manifest()),
                    pre,
                    "{ctx}: published a failed merge"
                );
                assert_eq!(db.seq(), pre_seq, "{ctx}");
                assert_eq!(observed(&db.snapshot()), full, "{ctx}");
                // Poisoned: every later write, commit and merge.
                file.set_fault(None);
                for later in [
                    db.put("a", &doc("z", "late"), Durability::Buffered),
                    db.delete("a", "a2", Durability::Buffered),
                    db.commit().map(drop),
                    db.merge_step().map(drop),
                    db.finish_merges().map(drop),
                ] {
                    assert!(
                        matches!(later, Err(DatabaseError::Poisoned)),
                        "{ctx}: {later:?}"
                    );
                }
                assert!(db.cancel_merges().is_empty(), "{ctx}");
                drop(db);

                for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(point)] {
                    let ctx = format!("{ctx}, {loss:?}: {error}");
                    let crashed = file.crash(loss);
                    let stray = stray_manifest(&crashed);
                    let f = reopen(&crashed, &ctx, &mut quarantined);
                    if f.segments == pre {
                        // A cut merge commit's Manifest is retired by the reopen's own commit (ROADMAP 6.6.9).
                        match stray {
                            Some(stray) => {
                                assert_eq!(
                                    f.seq,
                                    pre_seq + u64::from(stray),
                                    "{ctx}, stray {stray}"
                                );
                            }
                            None => assert!(f.seq == pre_seq || f.seq == pre_seq + 1, "{ctx}"),
                        }
                        assert!(
                            prefixes.contains(&f.docs),
                            "{ctx}: before the merge, observed {:?}",
                            f.docs
                        );
                        saw_old += 1;
                    } else if f.segments == post {
                        assert_eq!(f.seq, pre_seq + 1, "{ctx}");
                        // The merge commit synced the WAL before its manifest.
                        assert_eq!(f.docs, full, "{ctx}: after the merge");
                        saw_new += 1;
                    } else {
                        panic!(
                            "{ctx}: neither manifest\n  found {:?}\n  pre {pre:?}\n  post {post:?}",
                            f.segments
                        );
                    }
                    checked += 1;
                }
            }
        }
    }
    // Both outcomes occur: a fault at the slot's sync that applied it commits the merge.
    assert!(saw_old > 0 && saw_new > 0, "old {saw_old}, new {saw_new}");
    eprintln!(
        "{checked} crashes: {saw_old} before the merge, {saw_new} after, {quarantined} quarantined"
    );
}

// --- 3: a conflicting merge writes nothing -----------------------------------------------------------------------

/// ADR-0032 item 4: a merge whose input gained deletions after the plan read it is discarded before any write.
#[test]
fn a_conflicting_merge_result_is_discarded_without_writing() {
    let (base, mut docs) = base();
    let file = SimFile::from_bytes(base);
    let mut db = Database::open_on(file.clone(), options(1, 2)).unwrap();
    assert!(db.merge_step().unwrap().is_empty());
    assert_eq!(db.merges_running(), 1);
    // The plan read `a`'s second segment without deletions; this commit gives it one.
    db.delete("a", "a4", Durability::Buffered).unwrap();
    docs.get_mut("a").unwrap().remove("a4");
    db.commit().unwrap();
    let seq = db.seq();
    let committed = segments(db.manifest());
    let mutations = file.mutations();
    let mut outcomes = Vec::new();
    // No sleeps: the worker finishes when it does; each step either sees it or not.
    while outcomes.is_empty() {
        outcomes = db.merge_step().unwrap();
        std::thread::yield_now();
    }
    assert!(
        matches!(&outcomes[..], [MergeOutcome::Conflict { inputs, .. }] if inputs.len() == 2),
        "{outcomes:?}"
    );
    assert_eq!(file.mutations(), mutations, "a discarded merge wrote");
    assert_eq!(db.seq(), seq);
    assert_eq!(segments(db.manifest()), committed);
    assert_eq!(observed(&db.snapshot()), docs);
    // Planned again from the current segments; the deletion stays.
    let rest = db.finish_merges().unwrap();
    assert!(
        rest.iter()
            .any(|o| matches!(o, MergeOutcome::Committed { .. })),
        "{rest:?}"
    );
    assert_eq!(observed(&db.snapshot()), docs);
    drop(db);
    let db = Database::open_on(file.crash(CrashLoss::All), options(0, 3)).unwrap();
    assert_eq!(observed(&db.snapshot()), docs);
}

// --- 4: random interleavings against a model ---------------------------------------------------------------------

const SEED: u64 = 0x5045_5253_4941_0653; // "PERSIA", 6.5.3

#[cfg(not(feature = "slow"))]
const CASES: u32 = 64;
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

#[derive(Debug, Clone)]
enum Op {
    /// Collection index, id, word, `Fsync` (else `Buffered`).
    Put(usize, u8, usize, bool),
    Delete(usize, u8, bool),
    Commit,
    MergeStep,
    Cancel,
    Finish,
    /// Drops the database and reopens it after a crash that loses every unsynced write.
    Reopen,
}

fn arb_op() -> impl Strategy<Value = Op> {
    let c = 0..COLLECTIONS.len();
    prop_oneof![
        6 => (c.clone(), 0..8_u8, 0..WORDS.len(), any::<bool>())
            .prop_map(|(c, id, w, f)| Op::Put(c, id, w, f)),
        4 => (c, 0..8_u8, any::<bool>()).prop_map(|(c, id, f)| Op::Delete(c, id, f)),
        4 => Just(Op::Commit),
        5 => Just(Op::MergeStep),
        1 => Just(Op::Cancel),
        1 => Just(Op::Finish),
        1 => Just(Op::Reopen),
    ]
}

/// The model: what the database shows (`acked`), and what survives losing every unsynced write (`durable`). A sync
/// point (an `Fsync` write, a flush commit, a merge commit) makes `acked` durable.
struct Model {
    acked: Docs,
    durable: Docs,
}

impl Model {
    fn synced(&mut self) {
        self.durable = self.acked.clone();
    }
}

fn empty_docs() -> Docs {
    COLLECTIONS.iter().map(|c| (*c, BTreeMap::new())).collect()
}

/// Checks a merge outcome against the manifest after it, and returns whether it committed.
fn check_outcome(
    o: &MergeOutcome,
    manifest: &Manifest,
    last_seq: &mut u64,
) -> Result<bool, TestCaseError> {
    match o {
        MergeOutcome::Committed {
            collection,
            inputs,
            output,
            seq,
            ..
        } => {
            prop_assert!(*seq > *last_seq, "seq {} after {}", seq, last_seq);
            prop_assert!(*seq <= manifest.seq());
            *last_seq = *seq;
            let c = manifest.collection(collection).unwrap();
            for id in inputs {
                prop_assert!(c.segment(*id).is_none(), "input {:?} still listed", id);
            }
            if let Some(out) = output {
                prop_assert!(!inputs.contains(out));
            }
            Ok(true)
        }
        MergeOutcome::Conflict { .. } | MergeOutcome::Cancelled { .. } => Ok(false),
        MergeOutcome::Failed { error, .. } => Err(TestCaseError::fail(format!(
            "a merge of healthy segments failed: {error}"
        ))),
    }
}

fn run_case(threads: usize, ops: &[Op]) -> Result<(), TestCaseError> {
    let mut file = SimFile::new();
    let mut opens = 10_u64;
    let mut db = Database::create_on(file.clone(), [11; 16], options(threads, opens)).unwrap();
    for c in COLLECTIONS {
        db.create_collection(schema(c)).unwrap();
    }
    let mut model = Model {
        acked: empty_docs(),
        durable: empty_docs(),
    };
    let mut last_seq = db.seq();
    for (step, op) in ops.iter().enumerate() {
        let ctx = format!("step {step} {op:?}, threads {threads}");
        match *op {
            Op::Put(c, id, w, fsync) => {
                let (name, id) = (COLLECTIONS[c], format!("d{id}"));
                let text = format!("{} {step}", WORDS[w]);
                let durability = if fsync {
                    Durability::Fsync
                } else {
                    Durability::Buffered
                };
                db.put(name, &doc(&id, &text), durability).unwrap();
                model.acked.get_mut(name).unwrap().insert(id, text);
                if fsync {
                    model.synced();
                }
            }
            Op::Delete(c, id, fsync) => {
                let name = COLLECTIONS[c];
                let durability = if fsync {
                    Durability::Fsync
                } else {
                    Durability::Buffered
                };
                db.delete(name, &format!("d{id}"), durability).unwrap();
                model.acked.get_mut(name).unwrap().remove(&format!("d{id}"));
                if fsync {
                    model.synced();
                }
            }
            Op::Commit => {
                let before = db.seq();
                let seq = db.commit().unwrap();
                if seq != before {
                    last_seq = seq;
                    model.synced();
                }
            }
            Op::MergeStep | Op::Finish => {
                let outcomes = if matches!(op, Op::Finish) {
                    db.finish_merges().unwrap()
                } else {
                    db.merge_step().unwrap()
                };
                let mut any = false;
                for o in &outcomes {
                    any |= check_outcome(o, db.manifest(), &mut last_seq)?;
                }
                if any {
                    model.synced();
                }
                if matches!(op, Op::Finish) {
                    prop_assert_eq!(db.merges_running(), 0, "{}", ctx);
                    // Finished: the policy plans nothing more.
                    prop_assert!(db.merge_step().unwrap().is_empty(), "{}", ctx);
                    prop_assert_eq!(db.merges_running(), 0, "{}", ctx);
                }
            }
            Op::Cancel => {
                let seq = db.seq();
                for o in db.cancel_merges() {
                    prop_assert!(
                        matches!(o, MergeOutcome::Cancelled { .. }),
                        "{}: {:?}",
                        ctx,
                        o
                    );
                }
                prop_assert_eq!(db.merges_running(), 0, "{}", ctx);
                prop_assert_eq!(db.seq(), seq, "{}", ctx);
            }
            Op::Reopen => {
                drop(db);
                file = file.crash(CrashLoss::All);
                opens += 1;
                db = Database::open_on(file.clone(), options(threads, opens)).unwrap();
                model.acked = model.durable.clone();
                last_seq = db.seq();
            }
        }
        prop_assert!(db.merges_running() <= threads.max(1), "{}", ctx);
        assert_unique_ids(db.manifest(), &ctx);
        prop_assert_eq!(&observed(&db.snapshot()), &model.acked, "{}", ctx);
    }
    // The end: everything merged, then a crash keeps what was durable.
    let mut any = false;
    for o in &db.finish_merges().unwrap() {
        any |= check_outcome(o, db.manifest(), &mut last_seq)?;
    }
    if any {
        model.synced();
    }
    prop_assert_eq!(&observed(&db.snapshot()), &model.acked);
    drop(db);
    let db = Database::open_on(file.crash(CrashLoss::All), options(threads, opens + 1)).unwrap();
    assert_unique_ids(db.manifest(), "final reopen");
    prop_assert_eq!(&observed(&db.snapshot()), &model.durable);
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    /// Invariant 4. Threaded runs depend on when the worker finishes, but every timing must satisfy the model.
    #[test]
    fn merge_interleavings_match_the_model(
        threads in 0..=1_usize,
        ops in prop::collection::vec(arb_op(), 1..64),
    ) {
        run_case(threads, &ops)?;
    }
}

/// Whether `file` holds a valid Manifest frame after the committed `log_end` (a cut commit's, ROADMAP 6.6.9); `None`
/// if a read-only open cannot tell (a corrupt tail, which the reopen quarantines).
fn stray_manifest(file: &SimFile) -> Option<bool> {
    let container = Container::open_on(file.clone(), Mode::ReadOnly).ok()?;
    let log_end = container.superblock().log_end;
    Some(
        container
            .frames()
            .iter()
            .any(|f| f.kind == FrameKind::Manifest && f.offset >= log_end),
    )
}
