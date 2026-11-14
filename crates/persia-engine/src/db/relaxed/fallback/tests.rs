//! L5 `fallback` through a database (SPEC §7.6, ROADMAP 8.2.5): its order, what it excludes, and its pages.
//!
//! Invariants:
//! 1. L5's hits are the live documents the filters keep, the excluded clauses do not match and no earlier step
//!    matched, best first by static rank (the hit's score), then recency: memtable first, then the segments from the
//!    last to the first, each by docid descending. A deleted or replaced document is never one.
//! 2. Without a static rank (or of `weight` 0) recency alone orders them, and the response says `recency`.
//! 3. Pages by offset and by cursor are slices of that one ranking, whatever the request's sort; the order is the
//!    same on every search of a snapshot.
//! 4. The response's `fallback` names the order when L5 found the hits, and is absent when it found none.
//! 5. Facets count L5's matches; an explained hit gives its static rank and the value it came from.
//! 6. A budget that passes inside L5 drops it whole (`truncated`); the request's deadline and token are errors.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)] // Tests: inputs are fixed.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use persia_analysis::AnalyzerRegistry;
use persia_storage::container::sim::SimFile;

use super::super::{RelaxedResults, RelaxedSearchError};
use super::*;
use crate::db::{
    CancelToken, Clock, Database, DatabaseOptions, ManualClock, SearchRequest, Threads,
};
use crate::document::Document;
use crate::query::{
    Compiled, DefaultFields, Filter, FilterValue, Query, QueryCompiler, QueryLimits,
};
use crate::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use crate::schema::{
    Fallback, FieldKind, KeywordOptions, Level, Levels, RankFn, RankingConfig, RelaxationConfig,
    SchemaBuilder, StaticRank, TextOptions,
};
use crate::search::{FacetRequest, Interrupt, Order, Projection, Pruning, Sort, SortKey};
use crate::wal::Durability;

/// `body` text, `sales` (i64, the static rank when `ranked`: linear, weight 1), `tag` a facet. Two segments and the
/// memtable, with an upsert (`b`, whose segment copy the memtable hides) and a delete (`h`):
///
/// | view | docid: id (sales, tag, body) |
/// |---|---|
/// | segment 0 | 0: a (5, x, red shoes), 1: b (1, y, blue hat), 2: c (5, x, green hat), 3: d (−3, y, red hat) |
/// | segment 1 | 0: e (9, x, blue shoes), 1: f (none, y, red), 2: g (5, x, x y), 3: h (0, y, z), deleted |
/// | memtable  | 0: i (2, x, q), 1: j (none, y, w), 2: k (5, x, red), 3: b (7, y, blue hat) |
fn database(ranked: bool) -> Database<SimFile> {
    let mut builder = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("sales", FieldKind::I64 { stored: true })
        .field(
            "tag",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        );
    if ranked {
        let mut rank = StaticRank::new("sales");
        rank.function = RankFn::Linear;
        builder = builder.ranking(RankingConfig {
            static_rank: Some(rank),
        });
    }
    let schema = builder.build().unwrap();
    let field = |n: &str| schema.field(n).unwrap().id();
    let (body, sales, tag) = (field("body"), field("sales"), field("tag"));
    let mut db = Database::create_on(SimFile::new(), [5; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    let docs: [(&str, Option<i64>, &str, &str); 12] = [
        ("a", Some(5), "x", "red shoes"),
        ("b", Some(1), "y", "blue hat"),
        ("c", Some(5), "x", "green hat"),
        ("d", Some(-3), "y", "red hat"),
        ("e", Some(9), "x", "blue shoes"),
        ("f", None, "y", "red"),
        ("g", Some(5), "x", "x y"),
        ("h", Some(0), "y", "z"),
        ("i", Some(2), "x", "q"),
        ("j", None, "y", "w"),
        ("k", Some(5), "x", "red"),
        ("b", Some(7), "y", "blue hat"),
    ];
    for (n, (id, s, t, text)) in docs.into_iter().enumerate() {
        let mut doc = Document::new(id)
            .with(body, Value::String(text.into()))
            .with(tag, Value::String(t.into()));
        if let Some(s) = s {
            doc = doc.with(sales, Value::I64(s));
        }
        db.put("c", &doc, Durability::Buffered).unwrap();
        if n == 3 || n == 7 {
            if n == 7 {
                db.delete("c", "h", Durability::Buffered).unwrap();
            }
            db.commit().unwrap();
        }
    }
    db
}

/// L5's order over every live document of [`database`], by static rank then recency.
const RANKED: [&str; 10] = ["e", "b", "k", "g", "c", "a", "i", "j", "f", "d"];
/// By recency alone.
const RECENT: [&str; 10] = ["b", "k", "j", "i", "g", "f", "e", "d", "c", "a"];

fn exec() -> Execution {
    Execution::new(Pruning::default())
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

/// L0, the levels given, then L5, relaxing until `min_results` (10 000: every step).
fn plan(levels: Vec<Level>) -> RelaxPlan {
    RelaxPlan::new(
        &RelaxationConfig::default(),
        &RelaxOptions {
            min_results: Some(*RelaxationConfig::MIN_RESULTS.end()),
            levels: Some(Levels::Custom(levels)),
            fallback: Some(Fallback::StaticRank),
            ..RelaxOptions::default()
        },
    )
    .unwrap()
}

fn request(limit: usize) -> SearchRequest {
    SearchRequest {
        limit,
        projection: Projection::None,
        ..SearchRequest::default()
    }
}

fn compile(db: &Database<SimFile>, text: &str) -> Compiled {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(
            text,
            &DefaultFields::all(c.schema()),
            &QueryLimits::default(),
        )
}

fn search_with(
    db: &Database<SimFile>,
    query: &Compiled,
    relax: &RelaxPlan,
    request: &SearchRequest,
    exec: &Execution,
) -> Result<RelaxedResults, RelaxedSearchError> {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    views.search_relaxed(query, request, relax, exec)
}

fn search(db: &Database<SimFile>, text: &str, request: &SearchRequest) -> RelaxedResults {
    search_with(db, &compile(db, text), &plan(Vec::new()), request, &exec()).unwrap()
}

fn ids(out: &RelaxedResults) -> Vec<&str> {
    out.hits.iter().map(|h| h.hit.document.id()).collect()
}

fn tag_is(db: &Database<SimFile>, value: &str) -> Filter {
    let snapshot = db.snapshot();
    let tag = snapshot
        .collection("c")
        .unwrap()
        .schema()
        .field("tag")
        .unwrap()
        .id();
    Filter::Term {
        field: tag,
        value: FilterValue::Keyword(value.into()),
    }
}

#[test]
fn keys_rank_by_static_rank_then_view_then_docid() {
    let key = |score, view, docid| Key { score, view, docid };
    assert!(key(1.0, 0, 0) > key(0.5, MEMTABLE, 9));
    assert!(key(1.0, MEMTABLE, 0) > key(1.0, 3, 9));
    assert!(key(1.0, 3, 2) > key(1.0, 3, 1));
    assert_eq!(key(0.0, 1, 1), key(-0.0 + 0.0, 1, 1));
    let mut best = Best::new(2);
    for k in [
        key(1.0, 0, 0),
        key(3.0, 0, 1),
        key(2.0, 0, 2),
        key(3.0, 1, 0),
    ] {
        best.offer(k);
    }
    let kept: Vec<_> = best
        .into_sorted()
        .iter()
        .map(|k| (k.view, k.docid))
        .collect();
    assert_eq!(kept, [(1, 0), (0, 1)]);
    assert!(!Best::new(0).admits(key(9.0, 0, 0)));
}

#[test]
fn the_static_rank_reads_the_column_key_as_its_field_type() {
    use persia_format::for_codec::{f64_to_key, i64_to_key};

    let mut rank = StaticRank::new("n");
    rank.function = RankFn::Linear;
    let of = |kind| Ranking::StaticRank {
        rank: &rank,
        field: FieldId::new(0),
        kind,
    };
    assert_eq!(
        of(FieldType::I64).score(Some(i64_to_key(7))),
        (7.0, Some(7.0))
    );
    assert_eq!(
        of(FieldType::I64).score(Some(i64_to_key(-7))),
        (0.0, Some(-7.0))
    );
    assert_eq!(
        of(FieldType::F64).score(Some(f64_to_key(2.5))),
        (2.5, Some(2.5))
    );
    assert_eq!(
        of(FieldType::Datetime).score(Some(i64_to_key(1_000))),
        (1_000.0, Some(1_000.0))
    );
    // No value: 0, and none to explain.
    assert_eq!(of(FieldType::I64).score(None), (0.0, None));
    assert_eq!(Ranking::Recency.score(Some(i64_to_key(7))), (0.0, None));
}

#[test]
fn l5_ranks_every_live_document_by_static_rank_then_recency() {
    let db = database(true);
    let out = search(&db, "nosuch", &request(100));
    assert_eq!(ids(&out), RANKED);
    let r = &out.relaxation;
    assert_eq!(r.level, RelaxLevel::Fallback);
    assert_eq!(r.applied, ["fallback"]);
    assert_eq!(r.fallback, Some(FallbackOrder::StaticRank));
    // No word was searched, and none is excluded.
    assert_eq!(r.effective.as_deref(), Some(""));
    assert_eq!(r.corrected, []);
    // Each hit's score is its static rank; the rank score never increases down the page.
    let scores: Vec<f64> = out.hits.iter().map(|h| h.hit.score).collect();
    assert_eq!(scores, [9.0, 7.0, 5.0, 5.0, 5.0, 5.0, 2.0, 0.0, 0.0, 0.0]);
    assert!(
        out.hits
            .iter()
            .all(|h| h.level == RelaxLevel::Fallback && h.step == 1)
    );
    assert!(
        out.hits
            .windows(2)
            .all(|w| w[0].rank_score >= w[1].rank_score)
    );
    // The same snapshot gives the same order every time.
    assert_eq!(ids(&search(&db, "nosuch", &request(100))), RANKED);
}

#[test]
fn without_a_static_rank_recency_alone_orders_the_hits() {
    let db = database(false);
    let out = search(&db, "nosuch", &request(100));
    assert_eq!(ids(&out), RECENT);
    assert_eq!(out.relaxation.fallback, Some(FallbackOrder::Recency));
    assert!(out.hits.iter().all(|h| h.hit.score == 0.0));
    // A short page reads the newest views only, and is the top of the same ranking.
    for k in 1..RECENT.len() {
        assert_eq!(ids(&search(&db, "nosuch", &request(k))), RECENT[..k]);
    }
}

#[test]
fn filters_and_excluded_clauses_still_apply_and_earlier_hits_are_not_repeated() {
    let db = database(true);
    // Filters are never relaxed: only `x`.
    let filtered = compile(&db, "nosuch").with_filter([tag_is(&db, "x")]);
    let out = search_with(&db, &filtered, &plan(Vec::new()), &request(100), &exec()).unwrap();
    assert_eq!(ids(&out), ["e", "k", "g", "c", "a", "i"]);
    // Excluded clauses stay excluded, and are the step's text.
    let out = search(&db, "-red nosuch", &request(100));
    assert_eq!(ids(&out), ["e", "b", "g", "c", "i", "j"]);
    assert_eq!(out.relaxation.effective.as_deref(), Some("-red"));
    // L2 finds `a` and `e` (`shoes`); L5 ranks the rest, below them.
    let shoes = compile(&db, "shoes nosuch");
    let out = search_with(
        &db,
        &shoes,
        &plan(vec![Level::DropTerms]),
        &request(100),
        &exec(),
    )
    .unwrap();
    let steps: Vec<(usize, &str)> = out
        .hits
        .iter()
        .map(|h| (h.step, h.hit.document.id()))
        .collect();
    let mut l2: Vec<&str> = steps.iter().filter(|s| s.0 == 1).map(|s| s.1).collect();
    l2.sort_unstable();
    assert_eq!(l2, ["a", "e"]);
    let l5: Vec<&str> = steps.iter().filter(|s| s.0 == 2).map(|s| s.1).collect();
    assert_eq!(l5, ["b", "k", "g", "c", "i", "j", "f", "d"]);
    assert_eq!(out.relaxation.applied, ["drop_terms:75%", "fallback"]);
}

#[test]
fn the_marker_is_set_only_when_l5_found_the_hits() {
    let db = database(true);
    // A filter no document passes: L5 is built but finds nothing, so it is not the hit step.
    let none = compile(&db, "nosuch").with_filter([tag_is(&db, "nope")]);
    let out = search_with(&db, &none, &plan(Vec::new()), &request(10), &exec()).unwrap();
    assert_eq!(out.relaxation.level, RelaxLevel::Fallback);
    assert_eq!(out.relaxation.applied, ["fallback"]);
    assert_eq!(ids(&out), Vec::<&str>::new());
    assert_eq!(out.relaxation.fallback, None);
    assert_eq!(out.relaxation.effective.as_deref(), Some("nosuch"));
    // Enough hits at L0 (`min_results` 1): L5 is not built.
    let strict = RelaxPlan::new(
        &RelaxationConfig::default(),
        &RelaxOptions {
            levels: Some(Levels::Custom(Vec::new())),
            ..RelaxOptions::default()
        },
    )
    .unwrap();
    let red = compile(&db, "red");
    let out = search_with(&db, &red, &strict, &request(10), &exec()).unwrap();
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    assert_eq!(out.relaxation.fallback, None);
    // `fallback: "none"` builds no L5.
    let off = RelaxPlan::new(
        &RelaxationConfig::default(),
        &RelaxOptions {
            levels: Some(Levels::Custom(Vec::new())),
            fallback: Some(Fallback::None),
            ..RelaxOptions::default()
        },
    )
    .unwrap();
    let out = search_with(&db, &compile(&db, "nosuch"), &off, &request(10), &exec()).unwrap();
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    assert_eq!(ids(&out), Vec::<&str>::new());
}

#[test]
fn pages_by_offset_and_by_cursor_are_slices_of_one_ranking_whatever_the_sort() {
    for ranked in [true, false] {
        let db = database(ranked);
        let whole = if ranked { RANKED } else { RECENT };
        let snapshot = db.snapshot();
        let schema = snapshot.collection("c").unwrap().schema().clone();
        let sales = schema.field("sales").unwrap().id();
        let tag = schema.field("tag").unwrap().id();
        // L5 ranks by its own order under any sort; the hits still state the sort's values, their cursors.
        for sort in [
            Sort::score(),
            Sort::new([SortKey::field(sales, Order::Asc)], &schema).unwrap(),
            Sort::new(
                [SortKey::field(tag, Order::Desc), SortKey::score()],
                &schema,
            )
            .unwrap(),
        ] {
            for k in 1..=4 {
                let page = |offset, search_after| SearchRequest {
                    sort: sort.clone(),
                    offset,
                    search_after,
                    ..request(k)
                };
                let mut by_cursor = Vec::new();
                let mut cursor = None;
                loop {
                    let out = search(&db, "nosuch", &page(0, cursor.clone()));
                    let Some(last) = out.hits.last() else {
                        break;
                    };
                    assert!(out.relaxation.fallback.is_some());
                    cursor = Some(last.cursor());
                    by_cursor.extend(ids(&out).into_iter().map(str::to_owned));
                    assert!(
                        by_cursor.len() <= whole.len(),
                        "a page repeats hits: {by_cursor:?}"
                    );
                }
                assert_eq!(by_cursor, whole, "sort {sort:?}, pages of {k}");
                for offset in (0..whole.len()).step_by(k) {
                    let out = search(&db, "nosuch", &page(offset, None));
                    let end = (offset + k).min(whole.len());
                    assert_eq!(ids(&out), whole[offset..end]);
                }
            }
        }
    }
}

#[test]
fn a_hit_states_the_sorts_values_and_explains_its_static_rank() {
    let db = database(true);
    let snapshot = db.snapshot();
    let schema = snapshot.collection("c").unwrap().schema().clone();
    let sales = schema.field("sales").unwrap().id();
    let request = SearchRequest {
        sort: Sort::new(
            [SortKey::field(sales, Order::Asc), SortKey::score()],
            &schema,
        )
        .unwrap(),
        explain: true,
        ..request(100)
    };
    let out = search(&db, "nosuch", &request);
    let e = &out.hits[0].hit;
    assert_eq!(e.document.id(), "e");
    assert_eq!(
        e.sort,
        [
            crate::search::SortValue::Key(persia_format::for_codec::i64_to_key(9)),
            crate::search::SortValue::Score(9.0)
        ]
    );
    assert_eq!(
        e.explanation,
        Some(Explanation::StaticRank {
            score: 9.0,
            value: Some(9.0)
        })
    );
    let j = out
        .hits
        .iter()
        .find(|h| h.hit.document.id() == "j")
        .unwrap();
    assert_eq!(j.hit.sort[0], crate::search::SortValue::Missing);
    assert_eq!(
        j.hit.explanation,
        Some(Explanation::StaticRank {
            score: 0.0,
            value: None
        })
    );
    assert!(
        out.hits
            .iter()
            .all(|h| h.hit.explanation.as_ref().map(Explanation::score) == Some(h.hit.score))
    );
}

#[test]
fn facets_count_l5s_matches() {
    let db = database(true);
    let snapshot = db.snapshot();
    let schema = snapshot.collection("c").unwrap().schema().clone();
    let tag = schema.field("tag").unwrap().id();
    let faceted = SearchRequest {
        facets: vec![FacetRequest::new(tag, 10, &schema).unwrap()],
        ..request(2)
    };
    for ranked in [true, false] {
        let db = database(ranked);
        let out = search(&db, "-red nosuch", &faceted);
        // `e b g c i j`: four `x`, two `y`; the page is still the ranking's top.
        let counts: Vec<(Vec<u8>, u64)> = out.facets[0]
            .values
            .iter()
            .map(|v| (v.value.clone(), v.count))
            .collect();
        assert_eq!(counts, [(b"x".to_vec(), 4), (b"y".to_vec(), 2)]);
        let whole = search(&db, "-red nosuch", &request(100));
        assert_eq!(ids(&out), ids(&whole)[..2]);
    }
}

#[test]
fn fill_lets_l5_complete_a_page_below_the_earlier_hits() {
    let db = database(true);
    let fill = RelaxPlan::new(
        &RelaxationConfig::default(),
        &RelaxOptions {
            levels: Some(Levels::Custom(Vec::new())),
            fill: Some(true),
            ..RelaxOptions::default()
        },
    )
    .unwrap();
    // `shoes` has two hits at L0; with `fill`, L5 completes the page of five with the best others.
    let shoes = compile(&db, "shoes");
    let out = search_with(&db, &shoes, &fill, &request(5), &exec()).unwrap();
    let mut l0: Vec<&str> = out.hits[..2].iter().map(|h| h.hit.document.id()).collect();
    l0.sort_unstable();
    assert_eq!(l0, ["a", "e"]);
    assert_eq!(ids(&out)[2..], ["b", "k", "g"]);
    // The hit step is L5: the response is flagged.
    assert_eq!(out.relaxation.fallback, Some(FallbackOrder::StaticRank));
    // Without `fill`, L0's hits are enough (`min_results` 1).
    let no_fill = RelaxPlan::new(
        &RelaxationConfig::default(),
        &RelaxOptions {
            levels: Some(Levels::Custom(Vec::new())),
            ..RelaxOptions::default()
        },
    )
    .unwrap();
    let out = search_with(&db, &shoes, &no_fill, &request(5), &exec()).unwrap();
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    assert_eq!(out.relaxation.fallback, None);
}

/// Moves `by` on every read of [`Clock::now`].
#[derive(Debug)]
struct Ticking {
    now: Mutex<Instant>,
    by: Duration,
}

impl Clock for Ticking {
    fn now(&self) -> Instant {
        let mut now = self.now.lock().unwrap();
        *now += self.by;
        *now
    }

    fn unix_micros(&self) -> i64 {
        0
    }
}

#[test]
fn a_budget_passing_inside_l5_drops_it_whole() {
    let db = database(true);
    let budget = |ms| {
        RelaxPlan::new(
            &RelaxationConfig::default(),
            &RelaxOptions {
                levels: Some(Levels::Custom(Vec::new())),
                budget_ms: Some(ms),
                ..RelaxOptions::default()
            },
        )
        .unwrap()
    };
    let ticking = || {
        exec().budget_clock(Arc::new(Ticking {
            now: Mutex::new(Instant::now()),
            by: Duration::from_millis(4),
        }))
    };
    // Reads after L0: the budget's start (t), before the level (t + 4), before the step (t + 8, within 10), then
    // inside L5 (t + 12, past it): the step is dropped, and nothing else was found.
    let query = compile(&db, "nosuch");
    let out = search_with(&db, &query, &budget(10), &request(10), &ticking()).unwrap();
    assert!(out.relaxation.truncated);
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    assert_eq!(out.relaxation.applied, Vec::<String>::new());
    assert_eq!(ids(&out), Vec::<&str>::new());
    assert_eq!(out.relaxation.fallback, None);
    // With time to spare, L5 runs whole.
    let out = search_with(&db, &query, &budget(60_000), &request(10), &ticking()).unwrap();
    assert!(!out.relaxation.truncated);
    assert_eq!(ids(&out), RANKED);
}

#[test]
fn the_requests_deadline_and_token_stop_l5_with_an_error() {
    let db = database(true);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let stats = c.search_statistics(&memtable);
    let every = SearchPlan::new(&Query::MatchAll, c.schema(), &stats).unwrap();
    let sort = Sort::score();
    let part = Part {
        sort: &sort,
        after: None,
        facets: &[],
        explain: false,
        window: 10,
    };
    let clock = Arc::new(ManualClock::new(0));
    let passed = exec().deadline(Arc::clone(&clock) as _, clock.now());
    clock.advance(Duration::from_millis(1));
    assert!(matches!(
        views.collect_fallback(&every, &part, &passed),
        Err(RequestError::Interrupted(Interrupt::DeadlineExceeded))
    ));
    let token = CancelToken::new();
    token.cancel();
    assert!(matches!(
        views.collect_fallback(&every, &part, &exec().cancel(token)),
        Err(RequestError::Interrupted(Interrupt::Cancelled))
    ));
    // Through the cascade, a deadline is an error, not a truncation.
    let query = compile(&db, "nosuch");
    let late = exec().deadline(Arc::clone(&clock) as _, clock.now());
    clock.advance(Duration::from_millis(1));
    assert!(matches!(
        search_with(&db, &query, &plan(Vec::new()), &request(10), &late),
        Err(RelaxedSearchError::Request(RequestError::Interrupted(
            Interrupt::DeadlineExceeded
        )))
    ));
}

/// Ties (equal static ranks, and every hit by recency alone) are broken by index location, the same way on every
/// search of one snapshot, whatever the threads, and across the pages of a cursor (SPEC §7.6, ADR-0044).
#[test]
fn ties_are_deterministic_within_a_snapshot_and_across_cursor_pages() {
    for ranked in [true, false] {
        let db = database(ranked);
        let query = compile(&db, "nosuch");
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let pool = crate::db::SearchPool::new(std::num::NonZeroUsize::new(4).unwrap()).unwrap();
        let runs: Vec<Vec<String>> = [exec(), exec().threads(Threads::Pool(pool))]
            .iter()
            .flat_map(|exec| [exec.clone(), exec.clone()])
            .map(|exec| {
                // Fresh views of the same snapshot each time.
                let memtable = c.memtable();
                let views = c.views(&memtable).unwrap();
                let out = views
                    .search_relaxed(&query, &request(100), &plan(Vec::new()), &exec)
                    .unwrap();
                out.hits
                    .iter()
                    .map(|h| h.hit.document.id().to_owned())
                    .collect()
            })
            .collect();
        let whole = if ranked { RANKED } else { RECENT };
        assert!(runs.iter().all(|r| *r == whole), "{runs:?}");
        // The ties at static rank 5 (`k g c a`) and 0 (`j f d`) split across pages of two by cursor, in the same order.
        let mut paged = Vec::new();
        let mut page = request(2);
        loop {
            let memtable = c.memtable();
            let views = c.views(&memtable).unwrap();
            let out = views
                .search_relaxed(&query, &page, &plan(Vec::new()), &exec())
                .unwrap();
            let Some(last) = out.hits.last() else {
                break;
            };
            page.search_after = Some(last.cursor());
            paged.extend(out.hits.iter().map(|h| h.hit.document.id().to_owned()));
            assert!(paged.len() <= whole.len(), "a page repeats hits: {paged:?}");
        }
        assert_eq!(paged, whole);
    }
}

/// Many FOR blocks (1 024 keys each) per segment: visiting them best first, stopping at the first that cannot beat the
/// worst kept hit, ranks as the order itself (static rank, then view and docid descending), for every window, also
/// with a cursor in the middle of a run of ties.
#[test]
fn blocks_visited_best_first_rank_as_the_whole_order() {
    let mut rank = StaticRank::new("sales");
    rank.function = RankFn::Linear;
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("sales", FieldKind::I64 { stored: false })
        .ranking(RankingConfig {
            static_rank: Some(rank),
        })
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let sales = schema.field("sales").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [3; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    // 5 000 documents, two segments of 2 000 (two blocks each, the second partial) and 1 000 in the memtable. Sales
    // repeat (ties across blocks and views), some are negative (static rank 0) and some are missing.
    let mut order: Vec<(f64, usize, u32, String)> = Vec::new();
    for i in 0..5_000u32 {
        let id = format!("d{i}");
        let mut doc = Document::new(id.as_str()).with(body, Value::String("w".into()));
        let v = i64::from((i * 7_919) % 97) - 10;
        if i % 13 != 0 {
            doc = doc.with(sales, Value::I64(v));
        }
        db.put("c", &doc, Durability::Buffered).unwrap();
        let (view, docid) = match i {
            0..2_000 => (0, i),
            2_000..4_000 => (1, i - 2_000),
            _ => (MEMTABLE, i - 4_000),
        };
        let score = if i % 13 == 0 {
            0.0
        } else {
            f64::from(i32::try_from(v.max(0)).unwrap())
        };
        order.push((score, view, docid, id));
        if i == 1_999 || i == 3_999 {
            db.commit().unwrap();
        }
    }
    order.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
    let whole: Vec<&str> = order.iter().map(|o| o.3.as_str()).collect();
    let query = compile(&db, "nosuch");
    for k in [1, 10, 100, 1_500] {
        let out = search_with(&db, &query, &plan(Vec::new()), &request(k), &exec()).unwrap();
        assert_eq!(ids(&out), whole[..k], "window {k}");
    }
    // A cursor among ties: the next page continues the order.
    let first = search_with(&db, &query, &plan(Vec::new()), &request(37), &exec()).unwrap();
    let next = SearchRequest {
        search_after: Some(first.hits.last().unwrap().cursor()),
        ..request(500)
    };
    let out = search_with(&db, &query, &plan(Vec::new()), &next, &exec()).unwrap();
    assert_eq!(ids(&out), whole[37..537]);
}

/// An `f64` block's bound may pass `f64::MAX`'s key into `+∞` and NaN (SPEC §5.2.1: the minimum plus `2^width − 1`),
/// whose static rank reads as 0: the bound used stops at `f64::MAX`, so such a block still ranks first and its
/// threshold is a finite key.
#[test]
fn an_f64_bound_past_the_largest_float_stops_there() {
    use persia_format::for_codec::f64_to_key;

    let mut rank = StaticRank::new("n");
    rank.function = RankFn::Linear;
    let ranking = Ranking::StaticRank {
        rank: &rank,
        field: FieldId::new(0),
        kind: FieldType::F64,
    };
    let block = crate::segment::KeyBlock {
        start: 0,
        end: 2,
        min: f64_to_key(1.0),
        max: f64_to_key(f64::INFINITY) + 7,
    };
    assert_eq!(ranking.largest(block), f64_to_key(f64::MAX));
    assert_eq!(ranking.score(Some(ranking.largest(block))).0, f64::MAX);
    // The smallest key reaching 2.0 is 2.0's own.
    assert_eq!(ranking.threshold(block, 2.0), Some(f64_to_key(2.0)));
    assert_eq!(ranking.threshold(block, 0.5), Some(f64_to_key(1.0)));
    // Above every key of the block: none.
    let low = crate::segment::KeyBlock {
        max: f64_to_key(3.0),
        ..block
    };
    assert_eq!(ranking.threshold(low, 3.5), None);
}
