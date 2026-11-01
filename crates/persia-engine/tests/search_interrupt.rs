//! A search's deadline and cancellation token (ROADMAP 7.6.2; SPEC §7.3).
//!
//! Invariants:
//! 1. A search whose token is cancelled, or whose deadline has passed, returns `Interrupted` and no hits, on every
//!    thread; a cancellation wins over a passed deadline.
//! 2. The clock is read before the search, before each view and every 256 matches: a deadline that passes mid-view
//!    stops the search.
//! 3. A deadline that does not pass and a token never cancelled change nothing: the same hits, bit for bit.

#![allow(clippy::unwrap_used, clippy::panic, missing_docs)] // Tests: inputs are fixed.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use persia_engine::db::{
    CancelToken, Clock, CollectionHit, Database, DatabaseOptions, Execution, ManualClock,
    SearchPool, SearchRequest, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{Query, TermQuery};
use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::{
    ExecError, Interrupt, Order, Pruning, RequestError, SearchPlan, Sort, SortKey,
};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

/// Each read of [`Clock::now`] moves the clock 1 ms forward: the reads a search makes are its time.
#[derive(Debug)]
struct TickingClock {
    start: Instant,
    reads: AtomicU64,
}

impl TickingClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            reads: AtomicU64::new(0),
        }
    }

    fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}

impl Clock for TickingClock {
    fn now(&self) -> Instant {
        let n = self.reads.fetch_add(1, Ordering::Relaxed);
        self.start + Duration::from_millis(n)
    }

    fn unix_micros(&self) -> i64 {
        0
    }
}

fn pool() -> SearchPool {
    static POOL: OnceLock<SearchPool> = OnceLock::new();
    POOL.get_or_init(|| SearchPool::new(NonZeroUsize::new(3).unwrap()).unwrap())
        .clone()
}

fn threads() -> [Threads; 3] {
    [Threads::Current, Threads::Global, Threads::Pool(pool())]
}

/// Two segments of `per_segment` documents and a memtable of 10, every one holding "common".
fn database(per_segment: usize) -> (Database<SimFile>, Query) {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("n", FieldKind::I64 { stored: false })
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let n_field = schema.field("n").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [5; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    for segment in 0..3 {
        let n = if segment < 2 { per_segment } else { 10 };
        for i in 0..n {
            // Lengths vary, so scores do too.
            let text = format!("common{}", " filler".repeat(i % 7));
            let doc = Document::new(format!("{segment}-{i}"))
                .with(body, Value::String(text))
                .with(n_field, Value::I64(i64::try_from(i).unwrap()));
            db.put("c", &doc, Durability::Buffered).unwrap();
        }
        if segment < 2 {
            db.commit().unwrap();
        }
    }
    let query = Query::Term(TermQuery {
        field: body,
        term: "common".into(),
    });
    (db, query)
}

fn bits(hits: &[CollectionHit]) -> Vec<(persia_engine::db::Located, u64)> {
    hits.iter()
        .map(|h| (h.location, h.score.to_bits()))
        .collect()
}

#[test]
fn a_cancelled_or_expired_search_returns_interrupted() {
    let (db, query) = database(20);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
    let views = c.views(&memtable).unwrap();
    let clock = Arc::new(ManualClock::new(0));
    for threads in threads() {
        let cancelled = CancelToken::new();
        cancelled.cancel();
        let expired = Execution::default()
            .threads(threads.clone())
            .deadline(clock.clone(), clock.now());
        let cases = [
            (
                Execution::default()
                    .threads(threads.clone())
                    .cancel(cancelled.clone()),
                Interrupt::Cancelled,
            ),
            (expired.clone(), Interrupt::DeadlineExceeded),
            // A cancellation wins over a passed deadline.
            (expired.cancel(cancelled.clone()), Interrupt::Cancelled),
        ];
        for (exec, interrupt) in cases {
            match views.top_k_with(&plan, 10, &exec) {
                Err(ExecError::Interrupted(i)) => assert_eq!(i, interrupt, "{threads:?}"),
                other => panic!("{threads:?}: {other:?}"),
            }
            match views.search_with(&plan, &SearchRequest::default(), &exec) {
                Err(RequestError::Interrupted(i)) => assert_eq!(i, interrupt, "{threads:?}"),
                other => panic!("{threads:?}: {other:?}"),
            }
        }
    }
}

#[test]
fn a_deadline_passing_mid_view_stops_the_search() {
    // 2 000 matches in a segment: the clock is read about 8 times there, beside once per view.
    let (db, query) = database(2_000);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
    let views = c.views(&memtable).unwrap();
    // Exhaustive, so every match is yielded and counted: pruning would yield fewer.
    let run = |deadline_ms: u64| {
        let clock = Arc::new(TickingClock::new());
        let at = clock.start + Duration::from_millis(deadline_ms);
        let exec = Execution::new(Pruning::Exhaustive)
            .threads(Threads::Current)
            .deadline(clock.clone(), at);
        (views.top_k_with(&plan, 10, &exec), clock.reads())
    };
    // The first view's first reads pass a deadline 3 ms away: it stops inside that view, not after it.
    let (stopped, reads) = run(3);
    assert!(matches!(
        stopped,
        Err(ExecError::Interrupted(Interrupt::DeadlineExceeded))
    ));
    assert_eq!(reads, 4, "the read that saw the deadline is the 4th");
    // A deadline far enough away changes nothing.
    let (finished, reads) = run(1_000);
    let expected = views
        .top_k_with(
            &plan,
            10,
            &Execution::new(Pruning::Exhaustive).threads(Threads::Current),
        )
        .unwrap();
    assert_eq!(bits(&finished.unwrap()), bits(&expected));
    // 3 views, 2 000 + 2 000 + 10 matches: one read before the search, 3 before the views, 7 + 7 + 0 within them.
    assert_eq!(reads, 1 + 3 + 7 + 7);
}

#[test]
fn a_live_token_and_a_far_deadline_change_no_hit() {
    let (db, query) = database(500);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
    let views = c.views(&memtable).unwrap();
    let clock = Arc::new(ManualClock::new(0));
    let request = SearchRequest {
        limit: 25,
        ..SearchRequest::default()
    };
    for pruning in [
        Pruning::Exhaustive,
        Pruning::MaxScore,
        Pruning::BlockMaxWand,
    ] {
        let sequential = Execution::new(pruning).threads(Threads::Current);
        let expected = views.top_k_with(&plan, 25, &sequential).unwrap();
        let page = views.search_with(&plan, &request, &sequential).unwrap();
        for threads in threads() {
            let exec = Execution::new(pruning)
                .threads(threads.clone())
                .deadline(clock.clone(), clock.now() + Duration::from_secs(60))
                .cancel(CancelToken::new());
            let hits = views.top_k_with(&plan, 25, &exec).unwrap();
            assert_eq!(bits(&hits), bits(&expected), "{pruning:?} on {threads:?}");
            assert_eq!(views.search_with(&plan, &request, &exec).unwrap(), page);
        }
    }
}

#[test]
fn a_deadline_passing_mid_view_stops_a_column_sort() {
    let (db, query) = database(2_000);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
    let views = c.views(&memtable).unwrap();
    let n = c.schema().field("n").unwrap().id();
    let request = SearchRequest {
        sort: Sort::new([SortKey::field(n, Order::Desc)], c.schema()).unwrap(),
        ..SearchRequest::default()
    };
    let run = |deadline_ms: u64| {
        let clock = Arc::new(TickingClock::new());
        let at = clock.start + Duration::from_millis(deadline_ms);
        let exec = Execution::default()
            .threads(Threads::Current)
            .deadline(clock.clone(), at);
        (views.search_with(&plan, &request, &exec), clock.reads())
    };
    let (stopped, reads) = run(3);
    assert!(matches!(
        stopped,
        Err(RequestError::Interrupted(Interrupt::DeadlineExceeded))
    ));
    assert_eq!(reads, 4, "stopped inside the first view");
    // One read before the search, 3 before the views, 7 + 7 within them, and one before the page's documents are
    // read.
    let (finished, reads) = run(1_000);
    assert_eq!(finished.unwrap().hits.len(), 20);
    assert_eq!(reads, 1 + 3 + 7 + 7 + 1);
}

#[test]
fn a_search_with_no_document_is_interrupted_too() {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [6; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    let query = Query::Term(TermQuery {
        field: body,
        term: "common".into(),
    });
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    assert!(c.segments().is_empty());
    let memtable = c.memtable();
    let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
    let views = c.views(&memtable).unwrap();
    let token = CancelToken::new();
    token.cancel();
    let exec = Execution::default().cancel(token);
    assert!(matches!(
        views.top_k_with(&plan, 10, &exec),
        Err(ExecError::Interrupted(Interrupt::Cancelled))
    ));
    assert!(matches!(
        views.search_with(&plan, &SearchRequest::default(), &exec),
        Err(RequestError::Interrupted(Interrupt::Cancelled))
    ));
}
