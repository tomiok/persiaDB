//! The filter step, end to end (ROADMAP 8.2.6, SPEC §7.6, ADR-0047): `filters: "last_resort"` plans one last step,
//! L0's query without its root filters, built only when every earlier step found fewer than `min_results` hits.
//!
//! Invariants:
//! 1. `"never"` never returns a hit its filters reject, at any level, under any plan, whatever query a caller put in
//!    place of a compiled one (`Compiled::with_query`, ROADMAP 8.2.14).
//! 2. `"last_resort"` returns the hits `"never"` does, in the same order, and after them, only when those are fewer than
//!    `min_results`, the documents L0's words match that the filters reject: excluded clauses stay excluded.
//! 3. Its hits rank by the request's sort; pages by cursor and by offset are slices of one ranking; a cursor from
//!    `"never"` is `CursorRelax` under `"last_resort"`, and the reverse.
//! 4. `fill` never builds it; the budget stops before it as before any step.
//! 5. Facets count its matches; `explain` and the response state it (`level`, `applied`, `filters_dropped`).
//! 6. A query's `filters` overrides the collection's, both ways; `relax: false` never builds it.

use std::sync::Arc;
use std::time::Duration;

use persia_analysis::AnalyzerRegistry;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;

use super::exec;
use crate::db::relaxed::{RelaxedHit, RelaxedQuery, RelaxedResults, RelaxedSearchError};
use crate::db::{CollectionViews, Database, DatabaseOptions, ManualClock, SearchRequest};
use crate::document::{Document, Value};
use crate::query::{
    BoolQuery, BoostQuery, Compiled, ConstantScoreQuery, DefaultFields, Filter, FilterValue, Query,
    QueryCompiler, QueryLimits, TermQuery,
};
use crate::relax::{LevelInput, LevelTransform, RelaxLevel, RelaxOptions, RelaxPlan, Step};
use crate::schema::{
    FieldId, FieldKind, FilterRelaxation, KeywordOptions, Level, Levels, RelaxationConfig,
    SchemaBuilder, TextOptions,
};
use crate::search::{Explanation, FacetRequest, Projection, RequestError};
use crate::segment::ReadError;
use crate::wal::Durability;

/// `(id, body, tag)`: the filter is `tag = x`.
const DOCS: [(&str, &str, &str); 7] = [
    ("a", "red shoes", "x"),
    ("b", "red hat", "y"),
    ("c", "blue shoes", "y"),
    ("d", "green hat", "x"),
    ("e", "red shoes blue", "y"),
    ("f", "purple socks", "y"),
    ("g", "red shoes again", "y"),
];

/// [`DOCS`] over two segments and the memtable, in a collection whose relaxation defaults are `relaxation`.
fn database(relaxation: RelaxationConfig) -> Database<SimFile> {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field(
            "tag",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .relaxation(relaxation)
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let tag = schema.field("tag").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    for (i, (id, text, t)) in DOCS.into_iter().enumerate() {
        let doc = Document::new(id)
            .with(body, Value::String(text.into()))
            .with(tag, Value::String(t.into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i == 2 || i == 4 {
            db.commit().unwrap();
        }
    }
    db
}

fn field(views: &CollectionViews<'_>, name: &str) -> FieldId {
    views.collection().schema().field(name).unwrap().id()
}

fn tag_is(tag: FieldId, value: &str) -> Filter {
    Filter::Term {
        field: tag,
        value: FilterValue::Keyword(value.into()),
    }
}

/// `text` compiled over every field, with the request filter `tag = x` unless `unfiltered`.
fn compile(views: &CollectionViews<'_>, text: &str, filtered: bool) -> Compiled {
    let schema = views.collection().schema();
    let compiled = QueryCompiler::new(schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(text, &DefaultFields::all(schema), &QueryLimits::default());
    if filtered {
        compiled.with_filter([tag_is(field(views, "tag"), "x")])
    } else {
        compiled
    }
}

fn plan(views: &CollectionViews<'_>, json: &str) -> RelaxPlan {
    RelaxPlan::new(
        views.collection().schema().relaxation(),
        &RelaxOptions::from_json(json).unwrap(),
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

fn run<'q>(
    views: &CollectionViews<'_>,
    query: impl Into<RelaxedQuery<'q>>,
    page: &SearchRequest,
    json: &str,
) -> RelaxedResults {
    views
        .search_relaxed(query, page, &plan(views, json), &exec())
        .unwrap()
}

fn ids(out: &RelaxedResults) -> Vec<(&str, RelaxLevel)> {
    out.hits
        .iter()
        .map(|h| (h.hit.document.id(), h.level))
        .collect()
}

fn strip(hits: &[RelaxedHit]) -> Vec<(String, usize, u64)> {
    hits.iter()
        .map(|h| {
            (
                h.hit.document.id().to_owned(),
                h.step,
                h.hit.score.to_bits(),
            )
        })
        .collect()
}

/// Every hit of `query` under `relax`, one page at a time of `base`, each after the last hit's cursor.
fn by_cursor<'q>(
    views: &CollectionViews<'_>,
    query: impl Into<RelaxedQuery<'q>> + Copy,
    base: &SearchRequest,
    relax: &RelaxPlan,
) -> Vec<RelaxedHit> {
    let mut all = Vec::new();
    let mut request = base.clone();
    loop {
        let page = views
            .search_relaxed(query, &request, relax, &exec())
            .unwrap();
        let Some(last) = page.hits.last() else {
            return all;
        };
        request.search_after = Some(last.cursor());
        all.extend(page.hits);
        assert!(all.len() <= 100, "paging does not end");
    }
}

#[test]
fn last_resort_drops_the_filters_only_after_every_level_found_too_few() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let red_shoes = compile(&views, "red shoes", true);

    // `"never"`: L0's `a`, then L5's `d` (the other document the filter keeps): two, fewer than 3, and no more.
    let never = run(&views, &red_shoes, &request(10), r#"{"min_results": 3}"#);
    assert_eq!(
        ids(&never),
        [("a", RelaxLevel::Exact), ("d", RelaxLevel::Fallback)]
    );
    assert!(!never.relaxation.filters_dropped);
    // `"last_resort"`: the same, then `red shoes` without the filter: `e` and `g`, the documents the filter rejected.
    let last = run(
        &views,
        &red_shoes,
        &request(10),
        r#"{"min_results": 3, "filters": "last_resort"}"#,
    );
    assert_eq!(strip(&last.hits[..2]), strip(&never.hits));
    let dropped: Vec<_> = ids(&last)[2..].to_vec();
    assert_eq!(dropped.len(), 2);
    assert!(
        dropped
            .iter()
            .all(|&(id, level)| ["e", "g"].contains(&id) && level == RelaxLevel::Filters)
    );
    let relaxation = &last.relaxation;
    assert_eq!(relaxation.level, RelaxLevel::Filters);
    assert_eq!(
        relaxation.applied.last().map(String::as_str),
        Some("filters")
    );
    assert_eq!(
        relaxation.applied[..relaxation.applied.len() - 1],
        never.relaxation.applied[..]
    );
    assert!(relaxation.filters_dropped);
    // The hit step's text is L0's: no word changed. No fallback flag: L5 is not the hit step.
    assert_eq!(relaxation.effective.as_deref(), Some("red shoes"));
    assert_eq!(relaxation.fallback, None);
    assert_eq!(relaxation.corrected, []);
    assert!(!relaxation.truncated);
    // Each hit's step follows its rank: the filter step is the last one.
    let step = last.hits[2].step;
    assert_eq!(step, relaxation.applied.len());
    assert!(last.hits[2..].iter().all(|h| h.step == step));

    // `min_results` 2 is reached at L5: the filter step is not built.
    let enough = run(
        &views,
        &red_shoes,
        &request(10),
        r#"{"min_results": 2, "filters": "last_resort"}"#,
    );
    assert_eq!(strip(&enough.hits), strip(&never.hits));
    assert!(!enough.relaxation.applied.iter().any(|a| a == "filters"));
    assert_eq!(enough.relaxation.level, RelaxLevel::Fallback);
    // A query without a filter has nothing to drop: the level is skipped, as `"never"`.
    let unfiltered = compile(&views, "red shoes", false);
    let last = run(
        &views,
        &unfiltered,
        &request(10),
        r#"{"min_results": 7, "filters": "last_resort"}"#,
    );
    let never = run(&views, &unfiltered, &request(10), r#"{"min_results": 7}"#);
    assert_eq!(strip(&last.hits), strip(&never.hits));
    assert_eq!(last.relaxation, never.relaxation);
}

/// ROADMAP 8.2.14: a query a caller put in place of a compiled one (`Compiled::with_query`) keeps the request filter at
/// every level under `"never"`, L5 included, whatever its shape; `"last_resort"` drops that filter alone.
#[test]
fn a_query_put_in_place_never_loses_the_request_filter() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let (body, tag) = (field(&views, "body"), field(&views, "tag"));
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: body,
            term: t.into(),
        })
    };
    let red_shoes = compile(&views, "red shoes", true);
    let Query::Bool(root) = red_shoes.query().clone() else {
        panic!()
    };
    let callers = [
        term("socks"),
        term("hat"),
        Query::MatchAll,
        Query::Bool(BoolQuery {
            should: vec![term("blue"), term("socks")],
            ..BoolQuery::default()
        }),
        Query::Boost(BoostQuery {
            query: Box::new(term("shoes")),
            boost: 2.0,
        }),
        Query::ConstantScore(ConstantScoreQuery {
            query: Box::new(term("red")),
            score: 1.0,
        }),
        // The compiled root with one more exclusion: it still holds the clauses, so the word levels build from them.
        Query::Bool(BoolQuery {
            must_not: vec![term("green")],
            ..root
        }),
    ];
    // Every document the filter keeps (`tag = x`).
    let kept = ["a", "d"];
    for caller in callers {
        let replaced = red_shoes.clone().with_query(caller.clone());
        // Every level the collection runs, and L5, until every document has been found.
        let never = run(&views, &replaced, &request(10), r#"{"min_results": 7}"#);
        for (id, level) in ids(&never) {
            assert!(
                kept.contains(&id),
                "{id} at {level:?} broke the filter: {caller:?}"
            );
        }
        // L5 reached every document the filter keeps that the caller's query does not exclude.
        let excluded = matches!(&caller, Query::Bool(b) if !b.must_not.is_empty());
        let mut found: Vec<&str> = ids(&never).into_iter().map(|(id, _)| id).collect();
        found.sort_unstable();
        assert_eq!(
            found,
            if excluded { &kept[..1] } else { &kept[..] },
            "{caller:?}"
        );
        assert_eq!(never.relaxation.level, RelaxLevel::Fallback, "{caller:?}");
    }
    // `"last_resort"` drops the request filter, and only it: the caller's own `tag = y` holds at every level.
    let own = compile(&views, "red shoes", true).with_query(Query::Bool(BoolQuery {
        must: vec![term("red")],
        filter: vec![tag_is(tag, "y")],
        ..BoolQuery::default()
    }));
    let never = run(&views, &own, &request(10), r#"{"min_results": 7}"#);
    assert_eq!(ids(&never), []);
    let last = run(
        &views,
        &own,
        &request(10),
        r#"{"min_results": 7, "filters": "last_resort"}"#,
    );
    let mut dropped: Vec<(&str, RelaxLevel)> = ids(&last);
    dropped.sort_unstable();
    assert_eq!(
        dropped,
        [
            ("b", RelaxLevel::Filters),
            ("e", RelaxLevel::Filters),
            ("g", RelaxLevel::Filters)
        ]
    );
}

#[test]
fn excluded_clauses_stay_excluded_and_a_step_that_finds_nothing_is_stated_but_not_flagged() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    // `-blue`: `e` holds both words but is blue, so only `g` breaks the filter.
    let query = compile(&views, "red shoes -blue", true);
    let out = run(
        &views,
        &query,
        &request(10),
        r#"{"min_results": 3, "filters": "last_resort"}"#,
    );
    assert_eq!(
        ids(&out),
        [
            ("a", RelaxLevel::Exact),
            ("d", RelaxLevel::Fallback),
            ("g", RelaxLevel::Filters)
        ]
    );
    // `-blue -again`: nothing left to find without the filter. The step was built (`level`, `applied`), but found no
    // hit: L5 is the hit step, so the response flags the fallback, not dropped filters.
    let query = compile(&views, "red shoes -blue -again", true);
    let out = run(
        &views,
        &query,
        &request(10),
        r#"{"min_results": 3, "filters": "last_resort"}"#,
    );
    assert_eq!(
        ids(&out),
        [("a", RelaxLevel::Exact), ("d", RelaxLevel::Fallback)]
    );
    assert_eq!(out.relaxation.level, RelaxLevel::Filters);
    assert_eq!(
        out.relaxation.applied.last().map(String::as_str),
        Some("filters")
    );
    assert!(!out.relaxation.filters_dropped);
    assert!(out.relaxation.fallback.is_some());
    assert_eq!(out.relaxation.effective.as_deref(), Some("-blue -again"));
}

#[test]
fn without_l5_the_filter_step_follows_the_last_word_level() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let query = compile(&views, "red shoes", true);
    // No level but the filter step: L0's `a`, then `e` and `g`.
    let out = run(
        &views,
        &query,
        &request(10),
        r#"{"min_results": 2, "levels": [], "fallback": "none", "filters": "last_resort"}"#,
    );
    assert_eq!(out.relaxation.applied, ["filters"]);
    assert_eq!(out.hits.len(), 3);
    assert_eq!(out.hits[0].level, RelaxLevel::Exact);
    assert!(
        out.hits[1..]
            .iter()
            .all(|h| h.level == RelaxLevel::Filters && h.step == 1)
    );
    // With L2 and no L5, `drop_terms:75%` searches `red` or `shoes` under the filter (`a` only), then the filter step.
    let out = run(
        &views,
        &query,
        &request(10),
        r#"{"min_results": 2, "levels": ["drop_terms"], "fallback": "none", "filters": "last_resort"}"#,
    );
    assert_eq!(out.relaxation.applied, ["drop_terms:75%", "filters"]);
    assert_eq!(out.hits.len(), 3);
    // A programmatic root `bool` drops its root filter too, and has no text to state.
    let body = field(&views, "body");
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: body,
            term: t.into(),
        })
    };
    let programmatic = Query::Bool(BoolQuery {
        must: vec![term("red"), term("shoes")],
        must_not: vec![term("again")],
        filter: vec![tag_is(field(&views, "tag"), "x")],
        ..BoolQuery::default()
    });
    let out = run(
        &views,
        &programmatic,
        &request(10),
        r#"{"min_results": 3, "filters": "last_resort"}"#,
    );
    assert_eq!(
        ids(&out),
        [
            ("a", RelaxLevel::Exact),
            ("d", RelaxLevel::Fallback),
            ("e", RelaxLevel::Filters)
        ]
    );
    assert!(out.relaxation.filters_dropped);
    assert_eq!(out.relaxation.effective, None);
}

#[test]
fn fill_never_builds_the_filter_step_but_it_fills_the_page_once_built() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let query = compile(&views, "red shoes", true);
    // `min_results` 1 is met at L0; `fill` takes L5's `d` for the page of 10, and stops there, the page not full.
    let out = run(
        &views,
        &query,
        &request(10),
        r#"{"fill": true, "filters": "last_resort"}"#,
    );
    assert_eq!(
        ids(&out),
        [("a", RelaxLevel::Exact), ("d", RelaxLevel::Fallback)]
    );
    assert!(!out.relaxation.applied.iter().any(|a| a == "filters"));
    // A deeper page by offset or by cursor does not build it either.
    let deep = SearchRequest {
        offset: 1,
        ..request(10)
    };
    let out = run(
        &views,
        &query,
        &deep,
        r#"{"fill": true, "filters": "last_resort"}"#,
    );
    assert_eq!(ids(&out), [("d", RelaxLevel::Fallback)]);
    let relax = plan(&views, r#"{"fill": true, "filters": "last_resort"}"#);
    let paged = by_cursor(&views, &query, &request(1), &relax);
    assert_eq!(paged.len(), 2);
    // `min_results` 3 is not met before it: built, it returns every match the page has room for, `fill` or not.
    for json in [
        r#"{"min_results": 3, "fill": true, "filters": "last_resort"}"#,
        r#"{"min_results": 3, "filters": "last_resort"}"#,
    ] {
        let out = run(&views, &query, &request(10), json);
        assert_eq!(out.hits.len(), 4, "{json}");
        assert_eq!(
            out.hits
                .iter()
                .filter(|h| h.level == RelaxLevel::Filters)
                .count(),
            2
        );
    }
}

#[test]
fn cursor_pages_across_the_filter_step_are_slices_of_one_ranking() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let query = compile(&views, "red shoes", true);
    let json = r#"{"min_results": 3, "filters": "last_resort"}"#;
    let relax = plan(&views, json);
    let all = run(&views, &query, &request(10), json);
    assert_eq!(all.hits.len(), 4);
    for limit in 1..=3 {
        let paged = by_cursor(&views, &query, &request(limit), &relax);
        assert_eq!(strip(&paged), strip(&all.hits), "limit {limit}");
        for offset in 0..all.hits.len() {
            let page = SearchRequest {
                offset,
                ..request(limit)
            };
            let out = views
                .search_relaxed(&query, &page, &relax, &exec())
                .unwrap();
            let end = (offset + limit).min(all.hits.len());
            assert_eq!(strip(&out.hits), strip(&all.hits[offset..end]));
            // Every page states the same cascade, a page holding no dropped hit included.
            assert_eq!(out.relaxation, all.relaxation, "offset {offset}");
        }
    }
    // A cursor from `"never"` does not page `"last_resort"`, nor the reverse: their settings differ.
    let never = plan(&views, r#"{"min_results": 3}"#);
    let from_never = views
        .search_relaxed(&query, &request(1), &never, &exec())
        .unwrap();
    let from_last = views
        .search_relaxed(&query, &request(1), &relax, &exec())
        .unwrap();
    for (cursor, under) in [
        (from_never.hits[0].cursor(), &relax),
        (from_last.hits[0].cursor(), &never),
    ] {
        let page = SearchRequest {
            search_after: Some(cursor),
            ..request(1)
        };
        let error = views
            .search_relaxed(&query, &page, under, &exec())
            .map(|_| ());
        assert!(
            matches!(
                &error,
                Err(RelaxedSearchError::Request(
                    RequestError::CursorRelax { .. }
                ))
            ),
            "{error:?}"
        );
        assert!(error.unwrap_err().to_string().contains("filters"));
    }
}

#[test]
fn every_cursor_page_across_the_filter_step_counts_the_same_facets() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let query = compile(&views, "red shoes", true);
    let tag = field(&views, "tag");
    let relax = plan(&views, r#"{"min_results": 3, "filters": "last_resort"}"#);
    let page = SearchRequest {
        facets: vec![FacetRequest::new(tag, 10, c.schema()).unwrap()],
        ..request(1)
    };
    let all = views
        .search_relaxed(
            &query,
            &SearchRequest {
                limit: 10,
                ..page.clone()
            },
            &relax,
            &exec(),
        )
        .unwrap();
    // Each page searches the steps before its cursor's to count them, L5 and L6 included: the same counts on every page.
    let mut request = page;
    let mut seen = Vec::new();
    loop {
        let out = views
            .search_relaxed(&query, &request, &relax, &exec())
            .unwrap();
        assert_eq!(out.facets, all.facets, "after {seen:?}");
        assert_eq!(out.relaxation, all.relaxation);
        let Some(last) = out.hits.last() else {
            break;
        };
        seen.push(last.hit.document.id().to_owned());
        request.search_after = Some(last.cursor());
    }
    assert_eq!(seen.len(), 4);
}

#[test]
fn a_query_without_words_never_drops_its_filters() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let tag = field(&views, "tag");
    let json = r#"{"min_results": 5, "filters": "last_resort"}"#;
    // An empty `q` and one of excluded words alone browse what the filter keeps: L0 and L5 hold them, and L6 builds no
    // step, so no document the filter rejects pads the page.
    for text in ["", "-shoes"] {
        let query = compile(&views, text, true);
        let out = run(&views, &query, &request(10), json);
        assert!(
            !out.relaxation
                .applied
                .iter()
                .any(|a| a.starts_with("filters")),
            "{text:?}"
        );
        assert!(!out.relaxation.filters_dropped);
        assert!(out.hits.len() < 5, "{text:?}");
        assert!(
            out.hits
                .iter()
                .all(|h| ["a", "d"].contains(&h.hit.document.id())),
            "{text:?}"
        );
    }
    let filter_only = Query::Bool(BoolQuery {
        filter: vec![tag_is(tag, "x")],
        ..BoolQuery::default()
    });
    let out = run(&views, &filter_only, &request(10), json);
    assert_eq!(out.hits.len(), 2);
    assert!(out.hits.iter().all(|h| h.level != RelaxLevel::Filters));
}

#[test]
fn a_root_of_should_clauses_requires_one_of_them_without_its_filters() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let body = field(&views, "body");
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: body,
            term: t.into(),
        })
    };
    // Under the filter the `should` clauses only score: L0 is every `x` document (`a`, `d`). Without it, one is required.
    let query = Query::Bool(BoolQuery {
        should: vec![term("socks"), term("blue")],
        filter: vec![tag_is(field(&views, "tag"), "x")],
        ..BoolQuery::default()
    });
    let out = run(
        &views,
        &query,
        &request(10),
        r#"{"min_results": 4, "levels": [], "filters": "last_resort"}"#,
    );
    let mut dropped: Vec<&str> = out
        .hits
        .iter()
        .filter(|h| h.level == RelaxLevel::Filters)
        .map(|h| h.hit.document.id())
        .collect();
    dropped.sort_unstable();
    assert_eq!(dropped, ["c", "e", "f"]);
    assert_eq!(out.hits.len(), 5);
}

#[test]
fn facets_count_the_filter_step_and_explain_states_it() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let query = compile(&views, "red shoes", true);
    let tag = field(&views, "tag");
    let page = SearchRequest {
        facets: vec![FacetRequest::new(tag, 10, c.schema()).unwrap()],
        explain: true,
        ..request(10)
    };
    let counts = |out: &RelaxedResults| -> Vec<(String, u64)> {
        out.facets[0]
            .values
            .iter()
            .map(|v| (String::from_utf8(v.value.clone()).unwrap(), v.count))
            .collect()
    };
    let never = run(&views, &query, &page, r#"{"min_results": 3}"#);
    assert_eq!(counts(&never), [("x".to_owned(), 2)]);
    // The interim rule of ROADMAP 8.1.9: every step built counts, so the dropped hits' `y` too.
    let last = run(
        &views,
        &query,
        &page,
        r#"{"min_results": 3, "filters": "last_resort"}"#,
    );
    assert_eq!(counts(&last), [("x".to_owned(), 2), ("y".to_owned(), 2)]);
    // Each dropped hit is explained inside a `relaxed` node naming the step.
    for hit in last.hits.iter().filter(|h| h.level == RelaxLevel::Filters) {
        let Some(Explanation::Relaxed(relaxed)) = &hit.hit.explanation else {
            panic!("no relaxed node: {:?}", hit.hit.explanation);
        };
        assert_eq!(relaxed.level, RelaxLevel::Filters);
        assert_eq!(relaxed.step, hit.step);
        assert_eq!(relaxed.applied.as_deref(), Some("filters"));
        // The node's score is the tree's, which is the hit's: the step's own.
        assert_eq!(
            relaxed.explanation.score().to_bits(),
            hit.hit.score.to_bits()
        );
        assert!(hit.hit.score > 0.0);
    }
}

#[test]
fn a_querys_filters_overrides_the_collections_both_ways() {
    let last_resort = RelaxationConfig {
        min_results: 3,
        filters: FilterRelaxation::LastResort,
        ..RelaxationConfig::default()
    };
    let never = RelaxationConfig {
        min_results: 3,
        ..RelaxationConfig::default()
    };
    let dropped = |out: &RelaxedResults| {
        out.hits
            .iter()
            .filter(|h| h.level == RelaxLevel::Filters)
            .count()
    };
    for (collection, json, want) in [
        (last_resort.clone(), "{}", 2),
        (last_resort.clone(), r#"{"filters": "never"}"#, 0),
        (never.clone(), "{}", 0),
        (never, r#"{"filters": "last_resort"}"#, 2),
        // `relax: false` runs L0 alone, whatever the collection's `filters`.
        (last_resort.clone(), "false", 0),
        // A query that turns the cascade on keeps the collection's opt-in.
        (
            RelaxationConfig {
                enabled: false,
                ..last_resort
            },
            "true",
            2,
        ),
    ] {
        let db = database(collection);
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let query = compile(&views, "red shoes", true);
        let out = run(&views, &query, &request(10), json);
        assert_eq!(dropped(&out), want, "{json}");
        assert_eq!(out.relaxation.filters_dropped, want > 0, "{json}");
    }
}

/// A level's own transform, building after moving `clock` by `by`: a level whose building takes time.
#[derive(Debug)]
struct Slow {
    inner: Arc<dyn LevelTransform>,
    clock: Arc<ManualClock>,
    by: Duration,
}

impl LevelTransform for Slow {
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        self.clock.advance(self.by);
        self.inner.steps(input)
    }

    fn nested(&self) -> bool {
        self.inner.nested()
    }

    fn covers_earlier(&self) -> bool {
        self.inner.covers_earlier()
    }

    fn fills(&self) -> bool {
        self.inner.fills()
    }
}

#[test]
fn a_budget_passed_before_the_filter_step_truncates_the_cascade_there() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let query = compile(&views, "red shoes", true);
    let json = r#"{"min_results": 3, "levels": [], "filters": "last_resort", "budget_ms": 50}"#;
    for (by_ms, truncated) in [(40, false), (60, true)] {
        let clock = Arc::new(ManualClock::new(0));
        let relax = plan(&views, json);
        let filters = relax
            .planned()
            .iter()
            .find(|l| l.level == RelaxLevel::Filters)
            .map(|l| Arc::clone(&l.transform))
            .unwrap();
        // The filter step's level takes `by_ms` to build, after L5 completed within the budget.
        let slow = Arc::new(Slow {
            inner: filters,
            clock: Arc::clone(&clock),
            by: Duration::from_millis(by_ms),
        });
        let relax = relax.with_level(RelaxLevel::Filters, slow);
        let exec = exec().budget_clock(clock);
        let out = views
            .search_relaxed(&query, &request(10), &relax, &exec)
            .unwrap();
        assert_eq!(out.relaxation.truncated, truncated, "{by_ms} ms");
        let built: Vec<_> = out.relaxation.applied.iter().map(String::as_str).collect();
        if truncated {
            // Checked before its step: not searched, not listed; L5's hits are the page's.
            assert_eq!(built, ["fallback"]);
            assert_eq!(out.relaxation.level, RelaxLevel::Fallback);
            assert_eq!(
                ids(&out),
                [("a", RelaxLevel::Exact), ("d", RelaxLevel::Fallback)]
            );
            assert!(!out.relaxation.filters_dropped);
            // Its cursor's last step is L5's. A cursor page builds the steps through it and L6 only when that page did,
            // `fill` or not (L6 never fills): the truncation before L6 holds for the query's later pages.
            let cursor = out.hits.last().unwrap().cursor();
            assert_eq!(cursor.relaxed.unwrap().last, 1);
            let next = SearchRequest {
                search_after: Some(cursor),
                ..request(10)
            };
            let after = views.search_relaxed(&query, &next, &relax, &exec).unwrap();
            assert_eq!(after.hits, []);
            assert_eq!(after.relaxation.applied, ["fallback"]);
            assert!(!after.relaxation.filters_dropped);
        } else {
            assert_eq!(built, ["fallback", "filters"]);
            assert_eq!(out.hits.len(), 4);
            assert!(out.relaxation.filters_dropped);
        }
    }
}

/// Words of the random documents and queries; no tag (`x`, `y`) is one, so a word never matches the keyword field.
const WORDS: [&str; 5] = ["red", "shoes", "hat", "blue", "green"];

#[derive(Debug, Clone)]
struct Case {
    /// Each document's words and whether its tag is `x` (the filter's).
    docs: Vec<(Vec<usize>, bool)>,
    commits: Vec<usize>,
    words: Vec<usize>,
    excluded: Option<usize>,
    filtered: bool,
    min_results: u32,
    fill: bool,
    /// Which of the four word levels the plan keeps (`None`: `"default"`).
    levels: Option<Vec<bool>>,
    fallback: bool,
    limit: usize,
    /// L0's form. 0 is compiled user text, 1 a root `bool` of `must` words, 2 of `should` words. 3 and above search
    /// no word: `should: [match_all]`, `boost` of `match_all`, `constant_score` of `match_all`, a nested empty
    /// `bool`, or a nested `bool` of exclusions alone. The `words` list is not searched in those shapes.
    shape: u8,
    /// With `shape` 0, the query a caller puts in place of the compiled one (`Compiled::with_query`, ROADMAP 8.2.14).
    caller: Option<Caller>,
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(
            (prop::collection::vec(0..WORDS.len(), 1..4), any::<bool>()),
            1..14,
        ),
        prop::collection::vec(0usize..14, 0..3),
        prop::collection::vec(0..WORDS.len(), 0..4),
        prop::option::of(0..WORDS.len()),
        prop::bool::weighted(0.8),
        1u32..8,
        any::<bool>(),
        prop::option::of(prop::collection::vec(any::<bool>(), 4)),
        any::<bool>(),
        1usize..4,
        0u8..8,
        prop::option::weighted(0.4, caller()),
    )
        .prop_map(
            |(
                docs,
                commits,
                words,
                excluded,
                filtered,
                min_results,
                fill,
                levels,
                fallback,
                limit,
                shape,
                caller,
            )| {
                Case {
                    docs,
                    commits,
                    words,
                    excluded,
                    filtered,
                    min_results,
                    fill,
                    levels,
                    fallback,
                    limit,
                    shape,
                    caller,
                }
            },
        )
}

/// A query a caller puts in place of a compiled one (`Compiled::with_query`, ROADMAP 8.2.14).
#[derive(Debug, Clone)]
enum Caller {
    /// The compiled root with one more excluded word: it still holds the clauses' parts, so the word levels read them.
    Exclude(usize),
    /// Any query of `body` words: `match_all`, `bool` (with a filter `tag = y` of its own or not), `boost`,
    /// `constant_score`.
    Any(Shape),
}

#[derive(Debug, Clone)]
enum Shape {
    Word(usize),
    All,
    Bool {
        must: Vec<Shape>,
        should: Vec<Shape>,
        must_not: Vec<Shape>,
        minimum_should_match: u32,
        own: bool,
    },
    Boost(Box<Shape>),
    Constant(Box<Shape>),
}

fn caller() -> impl Strategy<Value = Caller> {
    let leaf = prop_oneof![
        4 => (0..WORDS.len()).prop_map(Shape::Word),
        1 => Just(Shape::All),
    ];
    let shape = leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            3 => (
                prop::collection::vec(inner.clone(), 0..3),
                prop::collection::vec(inner.clone(), 0..3),
                prop::collection::vec(inner.clone(), 0..2),
                0u32..3,
                any::<bool>(),
            )
                .prop_map(|(must, should, must_not, minimum_should_match, own)| Shape::Bool {
                    must,
                    should,
                    must_not,
                    minimum_should_match,
                    own,
                }),
            1 => inner.clone().prop_map(|s| Shape::Boost(Box::new(s))),
            1 => inner.prop_map(|s| Shape::Constant(Box::new(s))),
        ]
    });
    prop_oneof![
        1 => (0..WORDS.len()).prop_map(Caller::Exclude),
        3 => shape.prop_map(Caller::Any),
    ]
}

impl Shape {
    fn query(&self, body: FieldId, tag: FieldId) -> Query {
        let all = |shapes: &[Shape]| shapes.iter().map(|s| s.query(body, tag)).collect();
        match self {
            Self::Word(w) => Query::Term(TermQuery {
                field: body,
                term: WORDS[*w].into(),
            }),
            Self::All => Query::MatchAll,
            Self::Bool {
                must,
                should,
                must_not,
                minimum_should_match,
                own,
            } => Query::Bool(BoolQuery {
                must: all(must),
                should: all(should),
                must_not: all(must_not),
                filter: if *own {
                    vec![tag_is(tag, "y")]
                } else {
                    Vec::new()
                },
                minimum_should_match: *minimum_should_match,
            }),
            Self::Boost(s) => Query::Boost(BoostQuery {
                query: Box::new(s.query(body, tag)),
                boost: 2.0,
            }),
            Self::Constant(s) => Query::ConstantScore(ConstantScoreQuery {
                query: Box::new(s.query(body, tag)),
                score: 1.0,
            }),
        }
    }

    /// Whether it searches a word (SPEC §7.6): it is one, or holds one in `must` or `should` of a `bool`, through
    /// `boost` and `constant_score`.
    fn searches_word(&self) -> bool {
        match self {
            Self::Word(_) => true,
            Self::All => false,
            Self::Bool { must, should, .. } => {
                must.iter().any(Self::searches_word) || should.iter().any(Self::searches_word)
            }
            Self::Boost(s) | Self::Constant(s) => s.searches_word(),
        }
    }
}

fn build(case: &Case) -> Database<SimFile> {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let tag = schema.field("tag").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    for (i, (words, x)) in case.docs.iter().enumerate() {
        let text: Vec<&str> = words.iter().map(|&w| WORDS[w]).collect();
        let doc = Document::new(format!("d{i}"))
            .with(body, Value::String(text.join(" ")))
            .with(tag, Value::String(if *x { "x" } else { "y" }.into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
        if case.commits.contains(&i) {
            db.commit().unwrap();
        }
    }
    db
}

fn options(case: &Case, filters: FilterRelaxation) -> RelaxOptions {
    RelaxOptions {
        min_results: Some(case.min_results),
        fill: Some(case.fill),
        levels: case.levels.as_ref().map(|keep| {
            Levels::Custom(
                Level::ALL
                    .into_iter()
                    .zip(keep)
                    .filter_map(|(l, &k)| k.then_some(l))
                    .collect(),
            )
        }),
        fallback: Some(if case.fallback {
            crate::schema::Fallback::StaticRank
        } else {
            crate::schema::Fallback::None
        }),
        filters: Some(filters),
        ..RelaxOptions::default()
    }
}

/// A case's L0, as its `shape` builds it, with the filter `tag = x` or without.
enum L0 {
    Compiled(Box<Compiled>),
    Query(Query),
}

impl L0 {
    fn of(case: &Case, views: &CollectionViews<'_>, filtered: bool) -> Self {
        if case.shape >= 3 {
            let body = field(views, "body");
            let term = |w: usize| {
                Query::Term(TermQuery {
                    field: body,
                    term: WORDS[w].into(),
                })
            };
            // None of these names a term the query searches. `shape` 3 puts `match_all` in `should`, which a
            // non-empty `should` list used to count as a word.
            let clause = match case.shape {
                3 => Query::MatchAll,
                4 => Query::Boost(BoostQuery {
                    query: Box::new(Query::MatchAll),
                    boost: 2.0,
                }),
                5 => Query::ConstantScore(ConstantScoreQuery {
                    query: Box::new(Query::MatchAll),
                    score: 1.0,
                }),
                6 => Query::Bool(BoolQuery::default()),
                _ => Query::Bool(BoolQuery {
                    must_not: vec![term(0)],
                    ..BoolQuery::default()
                }),
            };
            let (must, should) = if case.shape == 3 {
                (Vec::new(), vec![clause])
            } else {
                (vec![clause], Vec::new())
            };
            return Self::Query(Query::Bool(BoolQuery {
                must,
                should,
                must_not: case.excluded.map(term).into_iter().collect(),
                filter: if filtered {
                    vec![tag_is(field(views, "tag"), "x")]
                } else {
                    Vec::new()
                },
                ..BoolQuery::default()
            }));
        }
        if case.shape == 0 {
            let mut text: Vec<String> = case.words.iter().map(|&w| WORDS[w].to_owned()).collect();
            if let Some(w) = case.excluded {
                text.push(format!("-{}", WORDS[w]));
            }
            let compiled = compile(views, &text.join(" "), filtered);
            let (body, tag) = (field(views, "body"), field(views, "tag"));
            let compiled = match &case.caller {
                None => compiled,
                Some(Caller::Exclude(w)) => {
                    let excluded = Shape::Word(*w).query(body, tag);
                    let query = match compiled.query().clone() {
                        Query::Bool(mut root)
                            if root.should.is_empty() && root.minimum_should_match == 0 =>
                        {
                            root.must_not.push(excluded);
                            Query::Bool(root)
                        }
                        query => Query::Bool(BoolQuery {
                            must: vec![query],
                            must_not: vec![excluded],
                            ..BoolQuery::default()
                        }),
                    };
                    compiled.with_query(query)
                }
                Some(Caller::Any(shape)) => compiled.with_query(shape.query(body, tag)),
            };
            return Self::Compiled(Box::new(compiled));
        }
        let body = field(views, "body");
        let term = |w: usize| {
            Query::Term(TermQuery {
                field: body,
                term: WORDS[w].into(),
            })
        };
        let words: Vec<Query> = case.words.iter().map(|&w| term(w)).collect();
        let (must, should) = if case.shape == 1 {
            (words, Vec::new())
        } else {
            (Vec::new(), words)
        };
        Self::Query(Query::Bool(BoolQuery {
            must,
            should,
            must_not: case.excluded.map(term).into_iter().collect(),
            filter: if filtered {
                vec![tag_is(field(views, "tag"), "x")]
            } else {
                Vec::new()
            },
            ..BoolQuery::default()
        }))
    }

    fn relaxed(&self) -> RelaxedQuery<'_> {
        match self {
            Self::Compiled(compiled) => compiled.as_ref().into(),
            Self::Query(query) => query.into(),
        }
    }
}

/// The words and tag of `hit`'s document.
fn doc_of<'c>(case: &'c Case, hit: &RelaxedHit) -> &'c (Vec<usize>, bool) {
    let i: usize = hit
        .hit
        .document
        .id()
        .strip_prefix('d')
        .unwrap()
        .parse()
        .unwrap();
    &case.docs[i]
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let db = build(case);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let l0 = L0::of(case, &views, case.filtered);
    let query = l0.relaxed();
    let doc = |hit: &RelaxedHit| doc_of(case, hit);
    let all = request(1000);
    let never_plan = RelaxPlan::new(
        c.schema().relaxation(),
        &options(case, FilterRelaxation::Never),
    )
    .unwrap();
    let last_plan = RelaxPlan::new(
        c.schema().relaxation(),
        &options(case, FilterRelaxation::LastResort),
    )
    .unwrap();
    let never = views
        .search_relaxed(query, &all, &never_plan, &exec())
        .unwrap();
    let last = views
        .search_relaxed(query, &all, &last_plan, &exec())
        .unwrap();

    // 1. `"never"` keeps the filter at every level, and the excluded word out.
    for hit in &never.hits {
        prop_assert!(
            !case.filtered || doc(hit).1,
            "{} broke the filter",
            hit.hit.document.id()
        );
        prop_assert_ne!(hit.level, RelaxLevel::Filters);
    }
    prop_assert!(!never.relaxation.filters_dropped);
    prop_assert!(!never.relaxation.applied.iter().any(|a| a == "filters"));

    // 2. `"last_resort"`: `"never"`'s hits first, as they are; then, only when those are too few, the filter step's.
    let (kept, dropped): (Vec<RelaxedHit>, Vec<RelaxedHit>) = last
        .hits
        .iter()
        .cloned()
        .partition(|h| h.level != RelaxLevel::Filters);
    prop_assert_eq!(strip(&kept), strip(&never.hits));
    let built = last
        .relaxation
        .applied
        .last()
        .is_some_and(|a| a == "filters");
    let too_few = never.hits.len() < usize::try_from(case.min_results).unwrap();
    // A query of no word builds no step: without the filter it would match every document. Shapes 3 and above
    // are wordless even when `words` is not empty (`match_all`, a wrapper of it, an empty `bool`, exclusions).
    // A query a caller put in searches a word as SPEC §7.6 says (ROADMAP 8.2.14).
    let words = match (case.shape, &case.caller) {
        (0, Some(Caller::Any(shape))) => shape.searches_word(),
        (shape, _) => shape < 3 && !case.words.is_empty(),
    };
    prop_assert_eq!(built, case.filtered && too_few && words);
    if !built {
        prop_assert_eq!(&last.relaxation, &never.relaxation);
    }
    // Its hits are L0's words without the filter, less what the filter keeps: L0 unfiltered, rejected by the filter.
    let unfiltered = L0::of(case, &views, false);
    let l0 = views
        .search_relaxed(
            unfiltered.relaxed(),
            &all,
            &RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::disabled()).unwrap(),
            &exec(),
        )
        .unwrap();
    let mut want: Vec<&str> = l0
        .hits
        .iter()
        .filter(|h| !doc(h).1)
        .map(|h| h.hit.document.id())
        .collect();
    if !built {
        want.clear();
    }
    let mut got: Vec<&str> = dropped.iter().map(|h| h.hit.document.id()).collect();
    want.sort_unstable();
    got.sort_unstable();
    prop_assert_eq!(&got, &want);
    prop_assert_eq!(last.relaxation.filters_dropped, !dropped.is_empty());
    excluded_words_stay_out(case, &last.hits)?;
    // Each document once.
    let mut seen: Vec<&str> = last.hits.iter().map(|h| h.hit.document.id()).collect();
    seen.sort_unstable();
    let n = seen.len();
    seen.dedup();
    prop_assert_eq!(seen.len(), n);
    same_ranking(&views, query, case.limit, &last_plan, &last)
}

/// No hit holds a word the query excludes: the case's excluded word, unless a caller's query put in place of the
/// compiled one dropped it, and the word a caller excluded beside the clauses.
fn excluded_words_stay_out(case: &Case, hits: &[RelaxedHit]) -> Result<(), TestCaseError> {
    let replaced = case.shape == 0 && matches!(case.caller, Some(Caller::Any(_)));
    let caller = match (case.shape, &case.caller) {
        (0, Some(Caller::Exclude(w))) => Some(*w),
        _ => None,
    };
    let excluded = case.excluded.filter(|_| !replaced);
    for hit in hits {
        for w in [excluded, caller].into_iter().flatten() {
            prop_assert!(
                !doc_of(case, hit).0.contains(&w),
                "{} holds the excluded {}",
                hit.hit.document.id(),
                WORDS[w]
            );
        }
    }
    Ok(())
}

/// `last`, the page of every hit of `query` under `plan`, is the ranking that excluding every earlier step from each
/// step gives (L5 standing for them in the filter step's exclusions changes nothing), and that pages of `limit` by
/// cursor give (invariant 3).
fn same_ranking(
    views: &CollectionViews<'_>,
    query: RelaxedQuery<'_>,
    limit: usize,
    plan: &RelaxPlan,
    last: &RelaxedResults,
) -> Result<(), TestCaseError> {
    let every = plan.clone().excluding_every_step();
    let full = views
        .search_relaxed(query, &request(1000), &every, &exec())
        .unwrap();
    prop_assert_eq!(strip(&full.hits), strip(&last.hits));
    prop_assert_eq!(&full.relaxation, &last.relaxation);
    let paged = by_cursor(views, query, &request(limit), plan);
    prop_assert_eq!(strip(&paged), strip(&last.hits));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Invariants 1 to 3 over random documents, queries (words, an excluded word, a filter or none, and a random query
    /// a caller puts in place of a compiled one) and plans
    /// (levels, `fallback`, `fill`, `min_results`).
    #[test]
    fn never_keeps_every_filter_and_last_resort_only_adds_the_filtered_out_matches_last(case in case()) {
        check(&case)?;
    }
}
