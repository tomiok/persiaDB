//! Relaxation metrics through a database (SPEC §7.6, §12.4; ROADMAP 8.3.3).
//!
//! Invariants:
//! 1. Every relaxed search without a cursor counts one query: `relaxed` when its response's `level` is not `exact`,
//!    `empty` when no step found a hit, else one at the level of its hit step. So the per-level counts add up to
//!    `queries − empty`. A cascade the budget truncated counts the same way, by the steps it ran. A cursor's page
//!    counts nowhere; a page by `offset` counts as a query; a plain search counts nowhere, unless a request whose
//!    settings run L0 alone ran it (ROADMAP 10.1.21): then it counts at level `exact`.
//! 2. A collection's snapshots share its counters (across commits); the database's are the sum over its collections,
//!    and a dropped collection's counts stay in it. A reopened database counts from 0.
//! 3. Searches on several threads each count once.
//!
//! The `tracing` event of a relaxed search is checked in `tests/relax_stats_tracing.rs`, alone in its binary: a scoped
//! subscriber does not see a callsite that tests on other threads registered first.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, missing_docs)] // Tests: inputs are fixed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Clock, CollectionSnapshot, Database, DatabaseOptions, Execution, ManualClock, RelaxationStats,
    RelaxedResults, SearchRequest, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{Fallback, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

fn schema(name: &str) -> persia_engine::schema::CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .synonyms(vec![vec!["sneakers".into(), "shoes".into()]])
        .build()
        .unwrap()
}

/// Collections `a` and `b`, each with `red shoes`, `red hat` and `blue running shoes`.
fn database() -> Database<SimFile> {
    database_on(SimFile::new())
}

fn database_on(file: SimFile) -> Database<SimFile> {
    let mut db = Database::create_on(file, [3; 16], DatabaseOptions::default()).unwrap();
    for name in ["a", "b"] {
        let schema = schema(name);
        let body = schema.field("body").unwrap().id();
        db.create_collection(schema).unwrap();
        for (id, text) in [
            ("1", "red shoes"),
            ("2", "red hat"),
            ("3", "blue running shoes"),
        ] {
            let doc = Document::new(id).with(body, Value::String(text.into()));
            db.put(name, &doc, Durability::Buffered).unwrap();
        }
    }
    db
}

/// `text` searched relaxed in collection `c` of the current snapshot, with `options` over the collection's settings. On
/// the calling thread, where a test's `tracing` subscriber is.
fn search(
    c: &CollectionSnapshot,
    text: &str,
    request: &SearchRequest,
    options: &RelaxOptions,
) -> RelaxedResults {
    search_on(c, text, request, options, &Execution::default())
}

/// [`search`] under `exec`, on the calling thread.
fn search_on(
    c: &CollectionSnapshot,
    text: &str,
    request: &SearchRequest,
    options: &RelaxOptions,
    exec: &Execution,
) -> RelaxedResults {
    let compiler = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin()).unwrap();
    let query = compiler.compile(
        text,
        &DefaultFields::all(c.schema()),
        &QueryLimits::default(),
    );
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let relax = RelaxPlan::new(c.schema().relaxation(), options).unwrap();
    views
        .search_relaxed(
            &query,
            request,
            &relax,
            &exec.clone().threads(Threads::Current),
        )
        .unwrap()
}

fn levels(stats: &RelaxationStats) -> Vec<(RelaxLevel, u64)> {
    stats.levels().filter(|&(_, n)| n > 0).collect()
}

#[test]
fn each_query_counts_once_at_its_hit_level_or_as_empty() {
    let db = database();
    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    let request = SearchRequest::default();
    let defaults = RelaxOptions::default();
    assert_eq!(a.relaxation_stats(), RelaxationStats::default());
    // L0 has hits.
    let out = search(a, "red", &request, &defaults);
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    // L1: `sneakers` is in no document, its synonym `shoes` is.
    let out = search(a, "sneakers", &request, &defaults);
    assert_eq!(out.relaxation.level, RelaxLevel::Synonyms);
    // L2: `red purple` drops `purple`.
    search(a, "red purple", &request, &defaults);
    // L4: `shoez` is one edit from `shoes`; L3 first finds nothing (`shoez` starts no term).
    let out = search(a, "shoez", &request, &defaults);
    assert_eq!(out.relaxation.level, RelaxLevel::Fuzzy);
    // L5: `zebra` matches nothing, the fallback ranks the collection.
    search(a, "zebra", &request, &defaults);
    // Still empty: no fallback, and nothing near `zebra`. The cascade built L3 and L4, so it relaxed.
    let no_fallback = RelaxOptions {
        fallback: Some(Fallback::None),
        ..RelaxOptions::default()
    };
    let out = search(a, "zebra", &request, &no_fallback);
    assert_eq!(out.hits.len(), 0);
    assert_ne!(out.relaxation.level, RelaxLevel::Exact);
    // Empty without relaxing: `relax: false`.
    let out = search(a, "zebra", &request, &RelaxOptions::disabled());
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);

    let stats = a.relaxation_stats();
    assert_eq!((stats.queries, stats.relaxed, stats.empty), (7, 5, 2));
    assert_eq!(
        levels(&stats),
        [
            (RelaxLevel::Exact, 1),
            (RelaxLevel::Synonyms, 1),
            (RelaxLevel::DropTerms, 1),
            (RelaxLevel::Fuzzy, 1),
            (RelaxLevel::Fallback, 1),
        ]
    );
    let found: u64 = stats.levels().map(|(_, n)| n).sum();
    assert_eq!(found, stats.queries - stats.empty);
    assert!((stats.relaxed_ratio() - 5.0 / 7.0).abs() < 1e-12);
    assert!((stats.empty_ratio() - 2.0 / 7.0).abs() < 1e-12);
    // The other collection counted nothing; the database counts both.
    let b = snapshot.collection("b").unwrap();
    assert_eq!(b.relaxation_stats(), RelaxationStats::default());
    assert_eq!(db.relaxation_stats(), stats);
}

#[test]
fn the_hit_step_is_the_last_step_that_found_a_hit_whatever_the_page_shows() {
    let db = database();
    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    // `red` has 2 hits at L0; at `min_results` 3 the cascade goes on to L5, which adds the third document. A page of
    // one holds an L0 hit only, but the query's hit step is L5's.
    let options = RelaxOptions {
        min_results: Some(3),
        ..RelaxOptions::default()
    };
    let page = SearchRequest {
        limit: 1,
        ..SearchRequest::default()
    };
    let out = search(a, "red", &page, &options);
    assert_eq!(out.hits.len(), 1);
    assert_eq!(out.hits[0].level, RelaxLevel::Exact);
    assert_eq!(levels(&a.relaxation_stats()), [(RelaxLevel::Fallback, 1)]);
}

/// A [`ManualClock`] that moves `by` at every read: the relaxation budget passes after as many reads as the cascade
/// makes, the same at every run, whatever the machine.
#[derive(Debug)]
struct Advancing {
    clock: ManualClock,
    by: Duration,
}

impl Advancing {
    fn new(by_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            clock: ManualClock::new(0),
            by: Duration::from_millis(by_ms),
        })
    }
}

impl Clock for Advancing {
    fn now(&self) -> Instant {
        self.clock.advance(self.by);
        self.clock.now()
    }

    fn unix_micros(&self) -> i64 {
        self.clock.unix_micros()
    }
}

/// A cascade the budget truncates counts as any other (SPEC §7.6 "Metrics"): `empty` when no step it ran found a hit,
/// else at the level of its hit step, and `relaxed` when its `level` is not `exact`.
#[test]
fn a_cascade_the_budget_truncates_counts_by_the_steps_it_ran() {
    let db = database();
    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    let request = SearchRequest::default();
    // `min_results` above the collection's 3 documents: only the budget ends a cascade before its last level.
    let budget = |ms| RelaxOptions {
        budget_ms: Some(ms),
        min_results: Some(10),
        ..RelaxOptions::default()
    };
    // 10 ms a read, 1 ms of budget: past it at the first read after L0, so no later step is built.
    let exec = Execution::default().budget_clock(Advancing::new(10));
    let red = search_on(a, "red", &request, &budget(1), &exec);
    assert!(red.relaxation.truncated);
    assert_eq!(
        (red.relaxation.level, red.hits.len()),
        (RelaxLevel::Exact, 2)
    );
    // L0 finds nothing for `zebra`: L5 runs past the budget (ROADMAP 8.1.33), so the page is not empty.
    let zebra = search_on(a, "zebra", &request, &budget(1), &exec);
    assert!(zebra.relaxation.truncated);
    assert_eq!(
        (zebra.relaxation.level, zebra.hits.len()),
        (RelaxLevel::Fallback, 3)
    );
    let stats = a.relaxation_stats();
    assert_eq!((stats.queries, stats.relaxed, stats.empty), (2, 1, 0));
    assert_eq!(
        levels(&stats),
        [(RelaxLevel::Exact, 1), (RelaxLevel::Fallback, 1)]
    );

    // 1 ms a read, every budget up to 60 ms: each query counts as its response says, truncated or not.
    let mut want = (stats.queries, stats.relaxed, stats.empty);
    let mut want_levels = levels(&stats);
    let mut seen = (false, false);
    for ms in 1..=60 {
        for text in ["sneakers", "red purple", "shoez", "zebra"] {
            let exec = Execution::default().budget_clock(Advancing::new(1));
            let out = search_on(a, text, &request, &budget(ms), &exec);
            let r = &out.relaxation;
            let relaxed = r.level != RelaxLevel::Exact;
            // The page holds every hit, so its last is the hit step's.
            let hit = out.hits.last().map(|h| h.level);
            want.0 += 1;
            want.1 += u64::from(relaxed);
            match hit {
                None => want.2 += 1,
                Some(level) => match want_levels.iter_mut().find(|(l, _)| *l == level) {
                    Some((_, n)) => *n += 1,
                    None => want_levels.push((level, 1)),
                },
            }
            want_levels.sort_by_key(|&(l, _)| l);
            let stats = a.relaxation_stats();
            let context = format!("{text:?} at {ms} ms: {r:?}");
            assert_eq!(
                (stats.queries, stats.relaxed, stats.empty),
                want,
                "{context}"
            );
            assert_eq!(levels(&stats), want_levels, "{context}");
            // The cases the budget makes: relaxed with a hit step after L0, and never relaxed and still empty: the plan
            // has L5, which runs past the budget when the page would be empty (ROADMAP 8.1.33).
            if r.truncated && relaxed {
                match hit {
                    None => seen.0 = true,
                    Some(level) => seen.1 |= level != RelaxLevel::Exact,
                }
            }
        }
    }
    assert_eq!(
        seen,
        (false, true),
        "a truncated relaxed cascade is never empty with L5, and the budgets truncate one with a hit step after L0"
    );
}

#[test]
fn a_cursor_page_counts_nowhere_and_an_offset_page_counts_as_a_query() {
    let db = database();
    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    let defaults = RelaxOptions::default();
    let page = SearchRequest {
        limit: 1,
        ..SearchRequest::default()
    };
    let first = search(a, "zebra", &page, &defaults);
    let next = SearchRequest {
        search_after: Some(first.hits[0].cursor()),
        ..page.clone()
    };
    let second = search(a, "zebra", &next, &defaults);
    assert_eq!(second.hits.len(), 1);
    // Past the end, by cursor: still nothing counted.
    let mut after = second;
    for _ in 0..3 {
        let Some(last) = after.hits.last() else { break };
        let next = SearchRequest {
            search_after: Some(last.cursor()),
            ..page.clone()
        };
        after = search(a, "zebra", &next, &defaults);
    }
    assert_eq!(after.hits.len(), 0);
    assert_eq!(a.relaxation_stats().queries, 1);
    // By offset, a page runs the cascade from the top: a query, at L5 (past the end too: its hits are before it).
    let deep = SearchRequest {
        offset: 10,
        ..page.clone()
    };
    assert_eq!(search(a, "zebra", &deep, &defaults).hits.len(), 0);
    let stats = a.relaxation_stats();
    assert_eq!((stats.queries, stats.empty), (2, 0));
    assert_eq!(stats.hits_at(RelaxLevel::Fallback), 2);
    // A plain search is not a relaxed one.
    let compiler = QueryCompiler::new(a.schema(), &AnalyzerRegistry::builtin()).unwrap();
    let query = compiler
        .compile(
            "red",
            &DefaultFields::all(a.schema()),
            &QueryLimits::default(),
        )
        .into_query();
    let memtable = a.memtable();
    let views = a.views(&memtable).unwrap();
    let plan = SearchPlan::new(&query, a.schema(), &a.search_statistics(&memtable)).unwrap();
    assert_eq!(views.top_k(&plan, 10, Pruning::default()).unwrap().len(), 2);
    assert_eq!(a.relaxation_stats().queries, 2);
}

#[test]
fn snapshots_share_their_collections_counters_and_a_drop_keeps_the_total() {
    let mut db = database();
    let before = db.snapshot();
    search(
        before.collection("a").unwrap(),
        "red",
        &SearchRequest::default(),
        &RelaxOptions::default(),
    );
    db.commit().unwrap();
    let after = db.snapshot();
    let a = after.collection("a").unwrap();
    search(
        a,
        "zebra",
        &SearchRequest::default(),
        &RelaxOptions::default(),
    );
    search(
        after.collection("b").unwrap(),
        "sneakers",
        &SearchRequest::default(),
        &RelaxOptions::default(),
    );
    // The snapshot before the commit reads the same counters as the one after.
    assert_eq!(
        before.collection("a").unwrap().relaxation_stats(),
        a.relaxation_stats()
    );
    assert_eq!(a.relaxation_stats().queries, 2);
    let total = db.relaxation_stats();
    assert_eq!((total.queries, total.relaxed), (3, 2));
    assert_eq!(
        total,
        a.relaxation_stats()
            .plus(&after.collection("b").unwrap().relaxation_stats())
    );
    // Dropped, `a`'s counts stay in the total, which never goes down.
    db.drop_collection("a").unwrap();
    assert_eq!(db.relaxation_stats(), total);
    // A search on a snapshot from before the drop still counts into `a`'s counters, which the database no longer reads.
    search(
        a,
        "red",
        &SearchRequest::default(),
        &RelaxOptions::default(),
    );
    assert_eq!(db.relaxation_stats(), total);
}

#[test]
fn a_reopened_database_counts_from_zero() {
    let file = SimFile::new();
    let mut db = database_on(file.clone());
    db.commit().unwrap();
    let snapshot = db.snapshot();
    search(
        snapshot.collection("a").unwrap(),
        "red",
        &SearchRequest::default(),
        &RelaxOptions::default(),
    );
    assert_eq!(db.relaxation_stats().queries, 1);
    drop(snapshot);
    drop(db);
    let db = Database::open_on(file, DatabaseOptions::default()).unwrap();
    assert_eq!(db.relaxation_stats(), RelaxationStats::default());
    let snapshot = db.snapshot();
    assert_eq!(
        snapshot.collection("a").unwrap().relaxation_stats(),
        RelaxationStats::default()
    );
}

#[test]
fn searches_on_several_threads_each_count_once() {
    let db = database();
    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    std::thread::scope(|s| {
        for t in 0..4 {
            s.spawn(move || {
                for i in 0..25 {
                    let text = if (t + i) % 2 == 0 { "red" } else { "zebra" };
                    search(a, text, &SearchRequest::default(), &RelaxOptions::default());
                }
            });
        }
    });
    let stats = a.relaxation_stats();
    assert_eq!((stats.queries, stats.relaxed, stats.empty), (100, 50, 0));
    assert_eq!(stats.hits_at(RelaxLevel::Exact), 50);
    assert_eq!(stats.hits_at(RelaxLevel::Fallback), 50);
}

/// A request whose settings run L0 alone (`relax: false`) runs the plain search and counts as a query at level `exact`
/// (SPEC §7.6, ROADMAP 10.1.21): `empty` when L0 found no hit, its hit level `exact` otherwise, never `relaxed`. A page
/// from a cursor counts nowhere; a page by `offset`, or of `limit` 0, counts by what L0 found from the top. The plain
/// search keeps its proximity bonus and its explanations without a `relaxed` node, and the engine's own plain search
/// (run without a request) counts nowhere.
#[test]
fn a_request_of_l0_alone_counts_at_level_exact_without_relaxing() {
    use persia_engine::search::Explanation;
    use persia_engine::search::request::{QueryRequest, QueryResults, RunOptions};

    let db = database();
    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    let run = |json: &str| {
        let request = QueryRequest::from_json(json, a.schema(), &QueryLimits::default()).unwrap();
        let response = request
            .run(a, &RunOptions::new().threads(Threads::Current))
            .unwrap();
        let QueryResults::Plain(results) = response.results else {
            panic!("{json}: not the plain search")
        };
        results
    };
    let counted = |queries, empty, exact| {
        let stats = a.relaxation_stats();
        assert_eq!(
            (stats.queries, stats.relaxed, stats.empty),
            (queries, 0, empty)
        );
        assert_eq!(stats.hits_at(RelaxLevel::Exact), exact);
        assert_eq!(levels(&stats).len(), usize::from(exact > 0));
    };
    // A zero-result query is in `empty`, the zero-result rate.
    assert_eq!(run(r#"{"q": "zebra", "relax": false}"#).hits.len(), 0);
    counted(1, 1, 0);
    // One with hits counts at level `exact`.
    let red = run(r#"{"q": "red", "relax": false, "limit": 1}"#);
    assert_eq!(red.hits.len(), 1);
    counted(2, 1, 1);
    // Its cursor's page continues the query: it counts nowhere, even empty.
    let token = red.hits[0].cursor().to_token();
    let next = format!(r#"{{"q": "red", "relax": false, "limit": 1, "search_after": "{token}"}}"#);
    assert_eq!(run(&next).hits.len(), 1);
    counted(2, 1, 1);
    // A page by `offset` past every hit, and a page of `limit` 0, are empty, but L0 found hits from the top.
    assert_eq!(
        run(r#"{"q": "red", "relax": false, "offset": 5}"#)
            .hits
            .len(),
        0
    );
    assert_eq!(
        run(r#"{"q": "red", "relax": false, "limit": 0}"#)
            .hits
            .len(),
        0
    );
    counted(4, 1, 3);
    // And for a query no document matches, both are empty queries.
    assert_eq!(
        run(r#"{"q": "zebra", "relax": false, "limit": 0}"#)
            .hits
            .len(),
        0
    );
    assert_eq!(
        run(r#"{"q": "zebra", "relax": false, "offset": 3}"#)
            .hits
            .len(),
        0
    );
    counted(6, 3, 3);
    // Still the plain search: `proximity` is taken (a relaxed search refuses it), an explanation has no `relaxed` node.
    let near = run(r#"{"q": "red shoes", "relax": false, "proximity": true, "explain": true}"#);
    let root = near.hits[0].explanation.as_ref().unwrap();
    assert!(matches!(root, Explanation::Bool { .. }), "{root:?}");
    counted(7, 3, 4);
    // The engine's plain search without a request counts nowhere.
    let memtable = a.memtable();
    let views = a.views(&memtable).unwrap();
    let compiled = QueryCompiler::new(a.schema(), &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(
            "zebra",
            &DefaultFields::all(a.schema()),
            &QueryLimits::default(),
        );
    let exec = Execution::new(Pruning::default()).threads(Threads::Current);
    let stats = a.search_statistics(&memtable);
    let plan =
        SearchPlan::with_terms(compiled.query(), a.schema(), &stats, &exec.terms(&views)).unwrap();
    let engine = views
        .search_with(&plan, &SearchRequest::default(), &exec)
        .unwrap();
    assert_eq!(engine.hits.len(), 0);
    counted(7, 3, 4);
}
