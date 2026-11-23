//! Facets under relaxation (ROADMAP 8.1.9, 8.1.11; SPEC §7.5, ADR-0055).
//!
//! Invariants:
//! 1. Every page of one query on one snapshot counts the facets of a page holding every hit: the counts of that page's
//!    hits, each document once, with or without `fill`, by offset or by cursor, the filter step included when it is
//!    built.
//! 2. The steps a `fill` page searches only to count them are not its own: its `applied`, `level`, hits and cursors are
//!    those of the same page without facets.
//! 3. The counts are kept once per query (8.1.11): a later page of the query reads them and searches no step only to
//!    count it, and gives the counts it would have counted.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::{by_cursor, database, exec, request, strip, term};
use crate::db::relaxed::{RelaxedQuery, RelaxedResults};
use crate::db::{CollectionViews, SearchRequest};
use crate::document::Value;
use crate::query::{BoolQuery, Filter, FilterValue, Query};
use crate::relax::{RelaxOptions, RelaxPlan};
use crate::schema::{FieldId, Level, Levels};
use crate::search::{FacetRequest, MAX_WINDOW};

const WORDS: [&str; 7] = ["red", "shoes", "hat", "blue", "shoez", "sho", "purple"];

fn all_of(body: FieldId, words: &[&str]) -> Query {
    Query::Bool(BoolQuery {
        must: words.iter().map(|w| term(body, w)).collect(),
        ..BoolQuery::default()
    })
}

fn search(
    views: &CollectionViews<'_>,
    query: &Query,
    page: &SearchRequest,
    relax: &RelaxPlan,
) -> RelaxedResults {
    views.search_relaxed(query, page, relax, &exec()).unwrap()
}

/// The counts of `out`'s one facet, as `(value, count)` pairs.
fn counts(out: &RelaxedResults) -> Vec<(Vec<u8>, u64)> {
    out.facets[0]
        .values
        .iter()
        .map(|v| (v.value.clone(), v.count))
        .collect()
}

/// What SPEC §7.5 counts for `query` under `relax`: the tags of the hits of a page holding every hit, as a facet of
/// `size` 10 orders them (highest count, then bytes).
fn of_every_hit<'q>(
    views: &CollectionViews<'_>,
    query: impl Into<RelaxedQuery<'q>>,
    relax: &RelaxPlan,
    tag: FieldId,
) -> Vec<(Vec<u8>, u64)> {
    // Every stored field: the tag of each hit.
    let every = SearchRequest {
        limit: MAX_WINDOW,
        ..SearchRequest::default()
    };
    let all = views.search_relaxed(query, &every, relax, &exec()).unwrap();
    let mut by: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    for hit in &all.hits {
        if let Some(Value::String(t)) = hit.hit.document.get(tag) {
            *by.entry(t.as_bytes().to_vec()).or_default() += 1;
        }
    }
    let mut out: Vec<(Vec<u8>, u64)> = by.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

fn faceted(page: SearchRequest, tag: FieldId, views: &CollectionViews<'_>) -> SearchRequest {
    SearchRequest {
        facets: vec![FacetRequest::new(tag, 10, views.collection().schema()).unwrap()],
        ..page
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Invariants 1 and 2 over the engine's own levels: every page, by offset and by cursor, at every page size,
    /// `min_results`, set of levels and `fill`, counts what a page of every hit holds, and states the same cascade and
    /// hits as without facets.
    #[test]
    fn every_page_counts_the_facets_of_a_page_holding_every_hit(
        words in prop::collection::vec(0..WORDS.len(), 1..=3),
        limit in 1usize..=3,
        min_results in 1u32..=4,
        fill in any::<bool>(),
        levels in prop::option::of(prop::collection::vec(any::<bool>(), 4)),
        fallback in any::<bool>(),
    ) {
        let (db, body, tag) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let query = all_of(body, &words.iter().map(|&w| WORDS[w]).collect::<Vec<_>>());
        let options = RelaxOptions {
            fill: Some(fill),
            min_results: Some(min_results),
            levels: levels.map(|keep| {
                Levels::Custom(Level::ALL.into_iter().zip(keep).filter_map(|(l, k)| k.then_some(l)).collect())
            }),
            fallback: Some(if fallback { crate::schema::Fallback::StaticRank } else { crate::schema::Fallback::None }),
            ..RelaxOptions::default()
        };
        let relax = RelaxPlan::new(c.schema().relaxation(), &options).unwrap();
        let want = of_every_hit(&views, &query, &relax, tag);
        for offset in 0..5 {
            let plain = SearchRequest { offset, ..request(limit) };
            let out = search(&views, &query, &faceted(plain.clone(), tag, &views), &relax);
            prop_assert_eq!(counts(&out), want.clone(), "offset {}", offset);
            let without = search(&views, &query, &plain, &relax);
            prop_assert_eq!(strip(&out.hits), strip(&without.hits));
            prop_assert_eq!(&out.relaxation, &without.relaxation);
            let cursors: Vec<_> = out.hits.iter().map(super::super::RelaxedHit::cursor).collect();
            let plain_cursors: Vec<_> = without.hits.iter().map(super::super::RelaxedHit::cursor).collect();
            prop_assert_eq!(cursors, plain_cursors);
        }
        // By cursor: every page counts the same.
        let mut page = faceted(request(limit), tag, &views);
        let mut walked = Vec::new();
        loop {
            let out = search(&views, &query, &page, &relax);
            prop_assert_eq!(counts(&out), want.clone());
            let Some(last) = out.hits.last() else { break };
            page.search_after = Some(last.cursor());
            walked.extend(out.hits);
            prop_assert!(walked.len() <= 10, "paging does not end");
        }
        prop_assert_eq!(strip(&walked), strip(&by_cursor(&views, &query, &request(limit), &relax)));
    }
}

/// Invariant 1 by hand under `fill` (it replaces the interim rule of ROADMAP 8.1.9, where a deeper page counted more):
/// every page counts L0, L2's rung and L5, which reaches every document, though the first page holds L0's hit alone.
#[test]
fn under_fill_every_page_counts_the_facets_of_a_page_holding_every_hit() {
    let (db, body, tag) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let relax = RelaxPlan::new(
        c.schema().relaxation(),
        &RelaxOptions::from_json(r#"{"fill": true}"#).unwrap(),
    )
    .unwrap();
    let red_shoes = all_of(body, &["red", "shoes"]);
    let whole = [(b"x".to_vec(), 3), (b"y".to_vec(), 2)];
    let first = search(
        &views,
        &red_shoes,
        &faceted(request(1), tag, &views),
        &relax,
    );
    assert_eq!(counts(&first), whole);
    // The first page builds and states L0 alone: the steps it counted past its own are not listed.
    assert_eq!(first.relaxation.applied.len(), 0);
    let second = search(
        &views,
        &red_shoes,
        &faceted(
            SearchRequest {
                search_after: Some(first.hits[0].cursor()),
                ..request(2)
            },
            tag,
            &views,
        ),
        &relax,
    );
    assert_eq!(counts(&second), whole);
    let last = search(
        &views,
        &red_shoes,
        &faceted(
            SearchRequest {
                offset: 4,
                ..request(1)
            },
            tag,
            &views,
        ),
        &relax,
    );
    assert_eq!(counts(&last), whole);
}

/// Invariant 1 with the filter step: it counts when built (fewer than `min_results` hits before it from the top), on
/// every page, and not when the steps before it found enough.
#[test]
fn the_filter_step_counts_on_every_page_when_it_is_built() {
    let (db, body, tag) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    // `tag = y` keeps `b` and `d`: `red shoes` under it finds `b` at L2, and without it `a` at L6 (`min_results` 4, no
    // L5).
    let y = Filter::Term {
        field: tag,
        value: FilterValue::Keyword("y".into()),
    };
    let query = Query::Bool(BoolQuery {
        must: vec![term(body, "red"), term(body, "shoes")],
        filter: vec![y.clone()],
        ..BoolQuery::default()
    });
    let soft = [y];
    let relax = RelaxPlan::new(
        c.schema().relaxation(),
        &RelaxOptions::from_json(
            r#"{"min_results": 4, "fallback": "none", "filters": "last_resort"}"#,
        )
        .unwrap(),
    )
    .unwrap();
    let marked = RelaxedQuery::from(&query).with_soft_filter(&soft);
    let all = views
        .search_relaxed(marked, &request(MAX_WINDOW), &relax, &exec())
        .unwrap();
    assert!(all.relaxation.filters_dropped);
    let want = of_every_hit(&views, marked, &relax, tag);
    assert_eq!(want, [(b"x".to_vec(), 1), (b"y".to_vec(), 1)]);
    let mut page = faceted(request(1), tag, &views);
    let mut pages = 0;
    loop {
        let out = views
            .search_relaxed(marked, &page, &relax, &exec())
            .unwrap();
        assert_eq!(counts(&out), want, "page {pages}");
        let Some(last) = out.hits.last() else { break };
        page.search_after = Some(last.cursor());
        pages += 1;
        assert!(pages <= 10);
    }
    assert_eq!(pages, 2);
    // With `min_results` 1, L2's `b` is enough: the filter step is not built, and no page counts `a`.
    let one = RelaxPlan::new(
        c.schema().relaxation(),
        &RelaxOptions::from_json(r#"{"fallback": "none", "filters": "last_resort"}"#).unwrap(),
    )
    .unwrap();
    let out = views
        .search_relaxed(marked, &faceted(request(1), tag, &views), &one, &exec())
        .unwrap();
    assert_eq!(counts(&out), [(b"y".to_vec(), 1)]);
}

/// The cache's counts of `views`' snapshot.
fn kept(views: &CollectionViews<'_>) -> usize {
    views
        .collection()
        .facets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

/// Steps searched by `f`, on this thread.
fn searches<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = crate::db::relaxed::searched();
    let out = f();
    (out, crate::db::relaxed::searched() - before)
}

/// Invariant 3 (ROADMAP 8.1.11): the first page that counts every step keeps its counts; a later cursor page of the
/// query reads them, searching only the steps its page needs (as many as without facets), and states the same counts.
/// Another facet request, or another memtable view, is another key.
#[test]
fn a_later_page_reads_the_kept_counts_and_searches_no_step_only_to_count_them() {
    let (mut db, body, tag) = database();
    let relax = RelaxPlan::new(
        db.snapshot().collection("c").unwrap().schema().relaxation(),
        &RelaxOptions::from_json(r#"{"min_results": 5}"#).unwrap(),
    )
    .unwrap();
    let red_shoes = all_of(body, &["red", "shoes"]);
    {
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        assert_eq!(kept(&views), 0);
        let first = search(
            &views,
            &red_shoes,
            &faceted(request(1), tag, &views),
            &relax,
        );
        assert_eq!(kept(&views), 1);
        let want = of_every_hit(&views, &red_shoes, &relax, tag);
        assert_eq!(counts(&first), want);
        // Three pages on, in a later step than L0's.
        let mut deep = request(1);
        for _ in 0..3 {
            let page = search(&views, &red_shoes, &deep, &relax);
            deep.search_after = Some(page.hits[0].cursor());
        }
        assert!(deep.search_after.as_ref().unwrap().relaxed.unwrap().step > 0);
        let (without, plain) = searches(|| search(&views, &red_shoes, &deep, &relax));
        let (with, cached) = searches(|| {
            search(
                &views,
                &red_shoes,
                &faceted(deep.clone(), tag, &views),
                &relax,
            )
        });
        assert_eq!(cached, plain);
        assert_eq!(counts(&with), want);
        assert_eq!(strip(&with.hits), strip(&without.hits));
        // Another facet request (`size` 9) is not kept yet: that page searches the steps before its cursor's too, and
        // keeps its own counts.
        let nine = SearchRequest {
            facets: vec![FacetRequest::new(tag, 9, c.schema()).unwrap()],
            ..deep.clone()
        };
        let (other, counted) = searches(|| search(&views, &red_shoes, &nine, &relax));
        assert!(counted > cached, "{counted} steps against {cached}");
        assert_eq!(counts(&other), want);
        assert_eq!(kept(&views), 2);
    }
    // A write: a new memtable view, whose counts are its own.
    let doc = crate::document::Document::new("z")
        .with(body, Value::String("red shoes".into()))
        .with(tag, Value::String("y".into()));
    db.put("c", &doc, crate::wal::Durability::Buffered).unwrap();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let out = search(
        &views,
        &red_shoes,
        &faceted(request(1), tag, &views),
        &relax,
    );
    assert_eq!(counts(&out), of_every_hit(&views, &red_shoes, &relax, tag));
}

/// A clock that moves a millisecond each time it is read.
#[derive(Debug, Default)]
struct Ticking(std::sync::atomic::AtomicU64);

impl crate::db::Clock for Ticking {
    fn now(&self) -> std::time::Instant {
        let ms = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        BASE.with(|b| *b) + std::time::Duration::from_millis(ms)
    }
    fn unix_micros(&self) -> i64 {
        0
    }
}

thread_local! {
    static BASE: std::time::Instant = std::time::Instant::now();
}

/// A page the budget truncated keeps no counts: the next page counts again.
#[test]
fn a_truncated_page_keeps_no_counts() {
    let (db, body, tag) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    // `budget_ms` 1: L0 is never cut, and the clock has moved past the budget at the next check.
    let relax = RelaxPlan::new(
        c.schema().relaxation(),
        &RelaxOptions::from_json(r#"{"min_results": 5, "budget_ms": 1}"#).unwrap(),
    )
    .unwrap();
    let exec = exec().budget_clock(std::sync::Arc::new(Ticking::default()));
    let red_shoes = all_of(body, &["red", "shoes"]);
    let out = views
        .search_relaxed(&red_shoes, &faceted(request(1), tag, &views), &relax, &exec)
        .unwrap();
    assert!(out.relaxation.truncated);
    assert_eq!(kept(&views), 0);
}
