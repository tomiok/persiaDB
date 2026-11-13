//! What readers see of a `put_batch` while it runs and after it fails (ROADMAP 6.3.12; SPEC §8.2.1 "holds the
//! memtable", ADR-0029 item 4).
//!
//! Invariants:
//! 1. Inside a batch (observed deterministically through the injected clock, which the writer calls after each
//!    applied document and while it plans a flush), the current snapshot and one loaded before the batch each show
//!    the state before the batch plus a prefix of it, and that prefix survives a crash losing every unsynced write:
//!    only a prefix a flush commit made durable is ever seen early. Upserts of a committed and of a memtable document
//!    are not seen early either: `get` and term searches over `views` agree with that state.
//! 2. A reader thread loading snapshots while batches run (some flushing mid-batch) sees each batch as a prefix:
//!    every batch that returned before the load in full, none of one that had not started.
//! 3. A document the memtable rejects ends the batch with `BatchRejected { index }`: the documents before it are
//!    visible (to old and new snapshots) and survive a crash, the rest never appear; the memtable is not left held.
//! 4. A crash at any file call of a flushing batch, keeping none, all or a seeded subset of the unsynced writes,
//!    reopens to the state before the batch plus a prefix of it, holding everything a snapshot showed.
//!
//! A flush commit that fails while it is planned (not poisoning) is covered in `database_tombstones.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Clock, CommitPolicy, Database, DatabaseError, DatabaseOptions, DatabaseReader, ManualClock,
    ReadOnlyDatabase, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::FlushPolicy;
use persia_engine::query::{Query, TermQuery};
use persia_engine::schema::{CollectionSchema, FieldId, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_storage::container::Error as ContainerError;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};

/// Live documents by id, with their body.
type Docs = BTreeMap<String, String>;

fn schema(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn body() -> FieldId {
    schema("x").field("body").unwrap().id()
}

fn doc(id: &str, text: &str) -> Document {
    Document::new(id).with(body(), Value::String(text.into()))
}

fn text_of(d: &Document) -> String {
    match d.get(body()) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{other:?}"),
    }
}

/// A body long enough that a dozen documents pass the 4 KiB threshold of [`flushing`].
fn filler() -> String {
    "lorem ipsum dolor sit amet ".repeat(20)
}

fn plain(clock: Arc<dyn Clock>) -> DatabaseOptions {
    DatabaseOptions {
        clock,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    }
}

/// Options whose memtable flushes at 4 KiB, so a batch of a dozen [`filler`] documents commits part of itself.
fn flushing(clock: Arc<dyn Clock>) -> DatabaseOptions {
    DatabaseOptions {
        policy: CommitPolicy {
            flush: FlushPolicy {
                memtable_bytes: 4 << 10,
                idle: Duration::from_secs(10),
            },
            interval: Duration::from_secs(3600),
        },
        ..plain(clock)
    }
}

fn manual() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(1_700_000_000_000_000))
}

/// Every live document of collection `a` as `snapshot` shows it (one memtable view for the candidates; `get`
/// decides, so a memtable version or delete hides the segments', SPEC §8.3).
fn docs(snapshot: &Snapshot) -> Docs {
    let c = snapshot.collection("a").unwrap();
    let memtable = c.memtable();
    let mut ids = std::collections::BTreeSet::new();
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
        .filter_map(|id| c.get(&id).unwrap().map(|d| (id, text_of(&d))))
        .collect()
}

/// How many documents of collection `a` a term search over `snapshot`'s views (one memtable view) finds for `term`.
fn hits(snapshot: &Snapshot, term: &str) -> usize {
    let c = snapshot.collection("a").unwrap();
    let memtable = c.memtable();
    let query = Query::Term(TermQuery {
        field: body(),
        term: term.into(),
    });
    let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
    c.views(&memtable)
        .unwrap()
        .top_k(&plan, 10_000, Pruning::default())
        .unwrap()
        .len()
}

/// How many documents of `docs` have `term` as a word.
fn with_term(docs: &Docs, term: &str) -> usize {
    docs.values()
        .filter(|t| t.split_whitespace().any(|w| w == term))
        .count()
}

/// `before` with the first `k` documents of `batch` applied, for each `k` in `0..=batch.len()`.
fn prefixes(before: &Docs, batch: &[Document]) -> Vec<Docs> {
    let mut state = before.clone();
    let mut out = vec![state.clone()];
    for d in batch {
        state.insert(d.id().to_owned(), text_of(d));
        out.push(state.clone());
    }
    out
}

/// The `k` for which `docs` is `prefixes[k]`, if any.
fn prefix_of(prefixes: &[Docs], docs: &Docs) -> Option<usize> {
    prefixes.iter().position(|p| p == docs)
}

/// What an [`Observer`] saw at one clock call inside a batch.
#[derive(Debug)]
struct Seen {
    /// The latest snapshot's documents.
    now: Docs,
    /// The documents of a snapshot loaded before the batch (its memtable is the one the batch started on).
    old: Docs,
    /// What a reopen after a crash losing every unsynced write shows.
    durable: Docs,
    /// Term searches over the latest snapshot: `(term, hits)`.
    hits: Vec<(String, usize)>,
}

/// What an [`Observer`] probes while armed.
struct Probe {
    reader: DatabaseReader,
    file: SimFile,
    old: Arc<Snapshot>,
    terms: Vec<String>,
    seen: Vec<Seen>,
}

/// A clock that, while armed, records what readers see each time the writer reads it: `put_batch` reads `now` after
/// each document it applies (with the memtable held) and `unix_micros` while it plans a flush. Deterministic: no
/// thread, no timing.
struct Observer {
    clock: ManualClock,
    probe: Mutex<Option<Probe>>,
}

impl fmt::Debug for Observer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Observer")
    }
}

impl Observer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            clock: ManualClock::new(1_700_000_000_000_000),
            probe: Mutex::new(None),
        })
    }

    fn observe(&self) {
        let mut probe = self.probe.lock().unwrap();
        let Some(p) = probe.as_mut() else { return };
        let now = p.reader.snapshot();
        let reopened = Database::open_on(p.file.crash(CrashLoss::All), plain(manual())).unwrap();
        let hits = p.terms.iter().map(|t| (t.clone(), hits(&now, t))).collect();
        p.seen.push(Seen {
            now: docs(&now),
            old: docs(&p.old),
            durable: docs(&reopened.snapshot()),
            hits,
        });
    }

    fn arm(&self, probe: Probe) {
        *self.probe.lock().unwrap() = Some(probe);
    }

    fn disarm(&self) -> Vec<Seen> {
        self.probe.lock().unwrap().take().unwrap().seen
    }
}

impl Clock for Observer {
    fn now(&self) -> Instant {
        self.observe();
        self.clock.now()
    }

    fn unix_micros(&self) -> i64 {
        self.observe();
        self.clock.unix_micros()
    }
}

/// Invariant 1, at every durability: inside a batch that flushes part of itself and upserts a committed document
/// and a memtable one, no snapshot (new, or loaded before the batch) shows more of the batch than a crash keeps,
/// and what it shows is a prefix; `get` and searches agree. After the batch returns, all of it is visible.
#[test]
fn every_snapshot_inside_a_batch_shows_only_a_durable_prefix_of_it() {
    let observer = Observer::new();
    let file = SimFile::new();
    let mut db =
        Database::create_on(file.clone(), [21; 16], flushing(Arc::clone(&observer) as _)).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("seen", "prior committed"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    let rounds = [
        ("alpha", Durability::Buffered),
        ("bravo", Durability::Async),
        ("kilo", Durability::Fsync),
    ];
    let mut partial = 0;
    for (round, (word, durability)) in rounds.into_iter().enumerate() {
        // One document in the memtable before the batch, synced.
        let mem = format!("mem{round}");
        db.put("a", &doc(&mem, "prior memtable"), Durability::Fsync)
            .unwrap();
        // Upserts of the committed and the memtable document first, then enough to flush mid-batch.
        let mut batch = vec![
            doc("seen", &format!("{word} upserted")),
            doc(&mem, &format!("{word} upserted")),
        ];
        batch.extend(
            (0..12).map(|i| doc(&format!("{word}{i:02}"), &format!("{word} {}", filler()))),
        );
        let before = docs(&db.snapshot());
        let states = prefixes(&before, &batch);
        let terms: Vec<String> = [word, "prior", "upserted", "lorem"]
            .iter()
            .map(|t| (*t).to_owned())
            .collect();
        let segments = db.snapshot().collection("a").unwrap().segments().len();
        let old = db.snapshot();
        observer.arm(Probe {
            reader: db.reader(),
            file: file.clone(),
            old: Arc::clone(&old),
            terms,
            seen: Vec::new(),
        });
        db.put_batch("a", &batch, durability).unwrap();
        let seen = observer.disarm();
        let ctx = format!("{word} {durability:?}");
        assert!(
            seen.len() >= batch.len(),
            "{ctx}: {} observations",
            seen.len()
        );
        assert!(
            db.snapshot().collection("a").unwrap().segments().len() > segments,
            "{ctx}: the batch must flush"
        );
        for (i, s) in seen.iter().enumerate() {
            let at = |docs: &Docs, what: &str| {
                prefix_of(&states, docs).unwrap_or_else(|| {
                    panic!("{ctx} #{i}: {what} is not the state before the batch plus a prefix: {docs:?}")
                })
            };
            let (now, old, durable) = (
                at(&s.now, "now"),
                at(&s.old, "old"),
                at(&s.durable, "durable"),
            );
            assert!(
                now <= durable && old <= durable,
                "{ctx} #{i}: {now} (now) / {old} (old) documents of the batch seen, {durable} durable"
            );
            for (term, n) in &s.hits {
                assert_eq!(
                    *n,
                    with_term(&s.now, term),
                    "{ctx} #{i}: search for {term:?}"
                );
            }
            if now > 0 && now < batch.len() {
                partial += 1;
            }
        }
        // Returned: all of it is visible. A snapshot loaded before the batch sees the writes to its memtable up to
        // the flush that replaced it (SPEC §8.4): the flushed prefix, now that it is durable.
        let flushed = prefix_of(&states, &docs(&old)).unwrap();
        assert!(
            flushed > 0 && flushed < batch.len(),
            "{ctx}: {flushed} seen by the old snapshot"
        );
        let all = states.last().unwrap();
        assert_eq!(&docs(&db.snapshot()), all, "{ctx}");
        assert_eq!(hits(&db.snapshot(), word), 12 + 2, "{ctx}");
        assert_eq!(hits(&db.snapshot(), "prior"), 0, "{ctx}");
        db.put(
            "a",
            &doc(&format!("sync{round}"), "sync"),
            Durability::Fsync,
        )
        .unwrap();
    }
    assert!(
        partial > 0,
        "no observation saw a flushed prefix of a batch"
    );
}

/// Invariant 2: a reader thread, while the writer runs batches that flush mid-batch, sees each batch as a prefix
/// of itself, every batch that returned before the load in full and none of one not yet started. Bounded: the
/// writer runs a fixed number of batches, the reader loads until the writer is done (no sleeps).
#[test]
fn a_reader_thread_sees_each_batch_as_a_prefix() {
    const ROUNDS: usize = 30;
    const SIZE: usize = 10;
    let mut db = Database::create_on(SimFile::new(), [22; 16], flushing(manual())).unwrap();
    db.create_collection(schema("a")).unwrap();
    let reader = db.reader();
    let id = |round: usize, i: usize| format!("r{round:02}-{i:02}");
    // Batches whose put_batch returned, and whose put_batch started.
    let returned = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    // Loads the checker finished: the writer waits for the first, so the run never ends unchecked.
    let loaded = Arc::new(AtomicUsize::new(0));
    let checker = {
        let (returned, started, stop, loaded) = (
            Arc::clone(&returned),
            Arc::clone(&started),
            Arc::clone(&stop),
            Arc::clone(&loaded),
        );
        std::thread::spawn(move || {
            let mut loads = 0usize;
            // At least one load, and one more after the writer is done.
            loop {
                let last = stop.load(Ordering::SeqCst);
                let low = returned.load(Ordering::SeqCst);
                let snapshot = reader.snapshot();
                let shown = docs(&snapshot);
                let high = started.load(Ordering::SeqCst);
                for round in 0..ROUNDS {
                    let n = (0..SIZE)
                        .take_while(|&i| shown.contains_key(&id(round, i)))
                        .count();
                    let any = (0..SIZE)
                        .filter(|&i| shown.contains_key(&id(round, i)))
                        .count();
                    assert_eq!(
                        n,
                        any,
                        "seq {}: round {round} is not a prefix",
                        snapshot.seq()
                    );
                    if round < low {
                        assert_eq!(n, SIZE, "round {round} returned before the load");
                    }
                    if round >= high {
                        assert_eq!(n, 0, "round {round} had not started");
                    }
                }
                loads += 1;
                loaded.store(loads, Ordering::SeqCst);
                if last {
                    break;
                }
            }
            loads
        })
    };
    while loaded.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    let text = filler();
    for round in 0..ROUNDS {
        let batch: Vec<Document> = (0..SIZE).map(|i| doc(&id(round, i), &text)).collect();
        let durability = [Durability::Buffered, Durability::Async, Durability::Fsync][round % 3];
        started.fetch_add(1, Ordering::SeqCst);
        db.put_batch("a", &batch, durability).unwrap();
        returned.fetch_add(1, Ordering::SeqCst);
    }
    stop.store(true, Ordering::SeqCst);
    let loads = checker.join().unwrap();
    assert!(loads > 0);
    let snapshot = db.snapshot();
    assert_eq!(docs(&snapshot).len(), ROUNDS * SIZE);
    assert!(
        snapshot.collection("a").unwrap().segments().len() > 1,
        "the batches must flush"
    );
}

/// A document the memtable rejects: a `text` field holding an integer.
fn bad(id: &str) -> Document {
    Document::new(id).with(body(), Value::I64(1))
}

/// Invariant 3: a rejected document ends the batch; the ones before it are visible at once, to a snapshot loaded
/// before the batch too, and survive a crash losing every unsynced write; the rest never appear. The memtable is
/// not left held: a single put and another batch are visible after. Also with a flush before the rejection, and
/// with the rejected document first or last.
#[test]
fn a_rejected_document_shows_the_ones_before_it_durable_and_nothing_after() {
    let text = filler();
    let cases: Vec<(&str, Vec<Document>, usize, bool)> = vec![
        (
            "first",
            vec![bad("x"), doc("b1", "t"), doc("b2", "t")],
            0,
            false,
        ),
        (
            "middle",
            vec![doc("b0", "t"), doc("b1", "t"), bad("x"), doc("b3", "t")],
            2,
            false,
        ),
        (
            "last",
            vec![doc("b0", "t"), doc("b1", "t"), bad("x")],
            2,
            false,
        ),
        (
            "after a flush",
            (0..12)
                .map(|i| doc(&format!("b{i:02}"), &text))
                .chain([bad("x"), doc("b99", "t")])
                .collect(),
            12,
            true,
        ),
    ];
    for durability in [Durability::Buffered, Durability::Async, Durability::Fsync] {
        for (name, batch, index, flushes) in &cases {
            let ctx = format!("{name} {durability:?}");
            let file = SimFile::new();
            let mut db = Database::create_on(file.clone(), [23; 16], flushing(manual())).unwrap();
            db.create_collection(schema("a")).unwrap();
            db.put("a", &doc("seen", "prior"), Durability::Fsync)
                .unwrap();
            let old = db.snapshot();
            let segments = old.collection("a").unwrap().segments().len();
            let result = db.put_batch("a", batch, durability);
            assert!(
                matches!(result, Err(DatabaseError::BatchRejected { index: i, .. }) if i == *index),
                "{ctx}: {result:?}"
            );
            let before = BTreeMap::from([("seen".to_owned(), "prior".to_owned())]);
            let want = &prefixes(&before, &batch[..*index])[*index];
            assert_eq!(&docs(&db.snapshot()), want, "{ctx}");
            if !flushes {
                assert_eq!(
                    &docs(&old),
                    want,
                    "{ctx}: a snapshot loaded before the batch"
                );
            }
            assert_eq!(
                db.snapshot().collection("a").unwrap().segments().len() > segments,
                *flushes,
                "{ctx}"
            );
            // Made durable at `durability` before BatchRejected is returned: at `Fsync`, a crash keeps all of them;
            // at any level, it keeps a prefix of them.
            let reopened = Database::open_on(file.crash(CrashLoss::All), plain(manual())).unwrap();
            let kept = docs(&reopened.snapshot());
            if durability == Durability::Fsync {
                assert_eq!(&kept, want, "{ctx}: after a crash");
            } else {
                let all = prefixes(&before, &batch[..*index]);
                assert!(
                    prefix_of(&all, &kept).is_some(),
                    "{ctx}: after a crash: {kept:?}"
                );
            }
            drop(reopened);
            // Not left held.
            db.put("a", &doc("single", "t"), durability).unwrap();
            assert!(docs(&db.snapshot()).contains_key("single"), "{ctx}");
            db.put_batch("a", &[doc("n0", "t"), doc("n1", "t")], durability)
                .unwrap();
            let shown = docs(&db.snapshot());
            assert!(
                shown.contains_key("n0") && shown.contains_key("n1"),
                "{ctx}"
            );
            assert!(
                !shown.contains_key("x") && !shown.contains_key("b99") && !shown.contains_key("b3"),
                "{ctx}"
            );
        }
    }
}

/// Invariant 3, with a failed sync (ROADMAP 6.3.12): when a document is rejected and reaching the
/// batch's durability for the ones before it then fails too, the rejection stays the error the caller sees, with the
/// sync failure added (`ThenFailed`), not replaced by it. The failed sync poisons the database, and a reopen decides
/// what survived. Each fault is the first file call after the rejection: the write of the buffered records, without
/// effect or with, and their sync.
#[test]
fn a_rejection_then_a_failed_sync_reports_the_rejection_first() {
    let batch = [doc("b0", "t"), doc("b1", "t"), bad("x"), doc("b3", "t")];
    for k in 0..3 {
        for fault in [SimFault::FailAt, SimFault::ApplyThenFail] {
            let file = SimFile::new();
            let mut db = Database::create_on(file.clone(), [29; 16], plain(manual())).unwrap();
            db.create_collection(schema("a")).unwrap();
            let fault = fault(file.mutations() + k);
            let ctx = format!("{fault:?}");
            file.set_fault(Some(fault));
            let err = db
                .put_batch("a", &batch, Durability::Fsync)
                .expect_err(&ctx);
            if !file.crashed() {
                // The sync made fewer calls than `k`: nothing failed but the rejection.
                assert!(
                    matches!(err, DatabaseError::BatchRejected { index: 2, .. }),
                    "{ctx}: {err:?}"
                );
                continue;
            }
            let DatabaseError::ThenFailed {
                ref error,
                step,
                ref later,
            } = err
            else {
                panic!("{ctx}: {err:?}");
            };
            assert!(
                matches!(**error, DatabaseError::BatchRejected { index: 2, .. }),
                "{ctx}: {err:?}"
            );
            assert!(
                matches!(err.primary(), DatabaseError::BatchRejected { index: 2, .. }),
                "{ctx}"
            );
            assert!(matches!(**later, DatabaseError::Wal(_)), "{ctx}: {later:?}");
            // The source is the rejection, and the message holds both.
            let source = std::error::Error::source(&err).unwrap().to_string();
            assert_eq!(source, error.to_string(), "{ctx}");
            let message = err.to_string();
            assert!(
                message.starts_with(&error.to_string())
                    && message.contains(step)
                    && message.ends_with(&later.to_string()),
                "{ctx}: {message}"
            );
            // The sync failed: the database is poisoned, and the batch is not shown.
            assert!(
                matches!(
                    db.put("a", &doc("after", "t"), Durability::Fsync),
                    Err(DatabaseError::Poisoned)
                ),
                "{ctx}"
            );
            assert!(docs(&db.snapshot()).is_empty(), "{ctx}");
        }
    }
    // A single rejected `put` reports `Memtable`, inside `ThenFailed` as well.
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [29; 16], plain(manual())).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("b0", "t"), Durability::Buffered).unwrap();
    file.set_fault(Some(SimFault::FailAt(file.mutations())));
    let err = db.put("a", &bad("x"), Durability::Fsync).unwrap_err();
    assert!(
        matches!(&err, DatabaseError::ThenFailed { error, .. } if matches!(**error, DatabaseError::Memtable(_))),
        "{err:?}"
    );
    assert!(matches!(err.primary(), DatabaseError::Memtable(_)));
}

/// Invariant 4: whichever file call of a flushing batch fails, with or without effect, a crash keeping none, all
/// or a seeded subset of the unsynced writes reopens to the state before the batch plus a prefix of it (replay
/// stops at the first record it cannot read, SPEC §8.6), holding everything a snapshot showed after the failure.
#[test]
fn a_crash_at_any_call_of_a_batch_recovers_a_prefix_holding_what_was_seen() {
    let text = filler();
    let batch: Vec<Document> = std::iter::once(doc("seen", "batch upserted"))
        .chain((0..12).map(|i| doc(&format!("b{i:02}"), &text)))
        .collect();
    let setup = |file: &SimFile| {
        let mut db = Database::create_on(file.clone(), [24; 16], flushing(manual())).unwrap();
        db.create_collection(schema("a")).unwrap();
        db.put("a", &doc("seen", "prior"), Durability::Fsync)
            .unwrap();
        db.commit().unwrap();
        db.put("a", &doc("mem", "prior"), Durability::Fsync)
            .unwrap();
        db
    };
    let twin = SimFile::new();
    let mut db = setup(&twin);
    let before = docs(&db.snapshot());
    let states = prefixes(&before, &batch);
    let start = twin.mutations();
    db.put_batch("a", &batch, Durability::Fsync).unwrap();
    let calls = twin.mutations() - start;
    assert!(calls > 2, "{calls} calls");
    let mut recovered = std::collections::BTreeSet::new();
    // Seeded crashes that tore the tail.
    let mut torn = 0;
    for k in 0..calls {
        for fault in [
            SimFault::FailAt(start + k),
            SimFault::ApplyThenFail(start + k),
        ] {
            let file = SimFile::new();
            let mut db = setup(&file);
            assert_eq!(file.mutations(), start, "{fault:?}");
            file.set_fault(Some(fault));
            assert!(
                db.put_batch("a", &batch, Durability::Fsync).is_err(),
                "{fault:?}"
            );
            file.set_fault(None);
            let seen = prefix_of(&states, &docs(&db.snapshot()))
                .unwrap_or_else(|| panic!("{fault:?}: shown is not a prefix"));
            drop(db);
            // `crash` leaves `file` as it is: each loss crashes the same state.
            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(k)] {
                let ctx = format!("{fault:?} {loss:?}");
                let crashed = file.crash(loss);
                let shown = match Database::open_on(crashed.clone(), plain(manual())) {
                    Ok(db) => docs(&db.snapshot()),
                    // A torn tail: the writer refuses it; a read-only open replays up to it.
                    Err(DatabaseError::Container(ContainerError::CorruptTail { .. })) => {
                        let ro = ReadOnlyDatabase::open_on(crashed, &AnalyzerRegistry::builtin())
                            .unwrap_or_else(|e| panic!("{ctx}: read-only open: {e}"));
                        torn += 1;
                        docs(&ro.snapshot())
                    }
                    Err(e) => panic!("{ctx}: reopen: {e}"),
                };
                let durable = prefix_of(&states, &shown)
                    .unwrap_or_else(|| panic!("{ctx}: recovered is not a prefix: {shown:?}"));
                assert!(seen <= durable, "{ctx}: {seen} seen, {durable} recovered");
                recovered.insert(durable);
            }
        }
    }
    assert!(
        recovered.len() > 2,
        "recovered prefixes: {recovered:?} ({torn} torn)"
    );
}
