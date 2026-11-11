//! L2 `drop_terms` through a database (SPEC §7.6, ROADMAP 8.2.2): the `minimum_should_match` ladder over a compiled
//! query's terms, with the collection's own statistics.
//!
//! Invariants:
//! 1. Each step after L0 requires its count of the terms, any of them (75%, 50%, then 1; a count repeating the one
//!    before, L0's first, is not built), and a document is a hit of the first step it matches.
//! 2. Each step's `effective` is the text of the terms it keeps, most important first by stopword, IDF and position,
//!    with every excluded clause, in text order; `applied` lists the steps built, cut or skipped to fit the limits.
//! 3. A query of one term (one required clause, whatever its parts) skips L2, which `applied` does not list.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, missing_docs)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, RelaxedResults, SearchRequest, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{
    BoolQuery, DefaultFields, Filter, FilterValue, PrefixQuery, Query, QueryCompiler, QueryLimits,
    TermQuery,
};
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    AnalyzerRef, FieldKind, KeywordOptions, RelaxationConfig, SchemaBuilder, TextOptions,
};
use persia_engine::search::Pruning;
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

/// `title` (standard), `body` (en) and `brand` (keyword: `acme` for `a`, `c`, `e`, `other` for the rest), over a segment
/// and the memtable.
fn database() -> Database<SimFile> {
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field(
            "body",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("en").unwrap(),
                ..TextOptions::default()
            }),
        )
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let brand = schema.field("brand").unwrap().id();
    let title = schema.field("title").unwrap().id();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [3; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    let docs = [
        ("a", "red running shoes", ""),
        ("b", "red hat", ""),
        ("c", "blue running shoes", ""),
        ("d", "green socks", ""),
        ("e", "", "the shoes of the day"),
        ("f", "shoes box", ""),
    ];
    for (i, (id, t, b)) in docs.into_iter().enumerate() {
        let doc = Document::new(id)
            .with(title, Value::String(t.into()))
            .with(body, Value::String(b.into()))
            .with(
                brand,
                Value::String(if i % 2 == 0 { "acme" } else { "other" }.into()),
            );
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i == 2 {
            db.commit().unwrap();
        }
    }
    db
}

fn exec() -> Execution {
    Execution::new(Pruning::default())
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

/// The collection's levels (the defaults, as `database` builds it), relaxing until `min_results` hits.
fn plan(min_results: u32) -> RelaxPlan {
    let options = RelaxOptions {
        min_results: Some(min_results),
        ..RelaxOptions::default()
    };
    RelaxPlan::new(&RelaxationConfig::default(), &options).unwrap()
}

/// Each hit's id and step, by step then id (a step's order is its scores').
fn by_step(out: &RelaxedResults) -> Vec<(usize, &str)> {
    let mut hits: Vec<(usize, &str)> = out
        .hits
        .iter()
        .map(|h| (h.step, h.hit.document.id()))
        .collect();
    hits.sort_unstable();
    hits
}

fn search(
    db: &Database<SimFile>,
    text: &str,
    min_results: u32,
    limits: Option<QueryLimits>,
) -> RelaxedResults {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let query_compiler = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin()).unwrap();
    let compiled = query_compiler.compile(
        text,
        &DefaultFields::all(c.schema()),
        &QueryLimits::default(),
    );
    let mut relax = plan(min_results);
    if let Some(limits) = limits {
        relax = relax.with_limits(limits);
    }
    let request = SearchRequest {
        limit: 100,
        ..SearchRequest::default()
    };
    views
        .search_relaxed(&compiled, &request, &relax, &exec())
        .unwrap()
}

#[test]
fn the_ladder_requires_fewer_terms_step_by_step_and_states_the_text_it_keeps() {
    let db = database();
    // Three terms, no document with all of them. `purple` is in no document (the rarest), `red` in two titles,
    // `shoes` in three titles and a body: `shoes` drops first, then `red`.
    let out = search(&db, "purple red shoes", 1, None);
    let r = &out.relaxation;
    assert_eq!(r.level, RelaxLevel::DropTerms);
    assert_eq!(r.applied, ["drop_terms:75%"]);
    assert_eq!(r.original.as_deref(), Some("purple red shoes"));
    assert_eq!(r.effective.as_deref(), Some("purple red"));
    // Two of the three: `a` holds `red` and `shoes`.
    assert_eq!(by_step(&out), [(1, "a")]);

    // Relaxing until every step ran: one term of three is the 50% step (⌊1.5⌋), so the `1` step repeats it.
    let out = search(&db, "purple red shoes", 10_000, None);
    let r = &out.relaxation;
    assert_eq!(r.applied, ["drop_terms:75%", "drop_terms:50%"]);
    assert_eq!(r.effective.as_deref(), Some("purple"));
    assert_eq!(
        by_step(&out),
        [(1, "a"), (2, "b"), (2, "c"), (2, "e"), (2, "f")]
    );
    // A stricter step ranks above a looser one, whatever their scores.
    assert!(out.hits.windows(2).all(|w| w[0].step <= w[1].step));

    // Four terms: 75% is 3, 50% is 2, then 1.
    let out = search(&db, "purple red running shoes", 10_000, None);
    assert_eq!(
        out.relaxation.applied,
        ["drop_terms:75%", "drop_terms:50%", "drop_terms:1"]
    );
    assert_eq!(by_step(&out)[0], (1, "a"));
}

#[test]
fn excluded_clauses_stay_excluded_and_in_the_text() {
    let db = database();
    // Two terms: 75% is one of them. `-blue` keeps `c` out of every step.
    let out = search(&db, "purple shoes -blue", 1, None);
    let r = &out.relaxation;
    assert_eq!(r.applied, ["drop_terms:75%"]);
    assert_eq!(r.effective.as_deref(), Some("purple -blue"));
    assert_eq!(by_step(&out), [(1, "a"), (1, "e"), (1, "f")]);
}

#[test]
fn a_stopword_is_dropped_before_rarer_terms() {
    let db = database();
    // `the` is a stopword of `body` (en) that `title` (standard) keeps: in no title, it would be the rarest term, and
    // `red` (two titles) would drop before `hat` (one). As a stopword it drops first, and `b` holds the other two.
    let out = search(&db, "the red hat", 1, None);
    let r = &out.relaxation;
    assert_eq!(r.applied, ["drop_terms:75%"]);
    assert_eq!(r.effective.as_deref(), Some("red hat"));
    assert_eq!(by_step(&out), [(1, "b")]);
    // Then `red`, the commoner of the two.
    let out = search(&db, "the red hat", 10_000, None);
    assert_eq!(out.relaxation.effective.as_deref(), Some("hat"));
}

#[test]
fn steps_that_do_not_fit_the_limits_are_listed_and_not_searched() {
    let db = database();
    // L0 is 4 clauses (its `bool` and three words); a step needs 9: the root, its own 4 and L0's, which it excludes.
    let tight = QueryLimits {
        max_clauses: 8,
        ..QueryLimits::default()
    };
    let out = search(&db, "purple red shoes", 1, Some(tight));
    let r = &out.relaxation;
    assert_eq!(r.level, RelaxLevel::Exact);
    assert_eq!(
        r.applied,
        ["drop_terms:75%:skipped", "drop_terms:50%:skipped"]
    );
    // The text is the last step's built and not skipped: L0's.
    assert_eq!(r.effective.as_deref(), Some("purple red shoes"));
    assert_eq!(out.hits, []);
}

#[test]
fn a_programmatic_query_relaxes_its_must_clauses_without_text() {
    let db = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let title = c.schema().field("title").unwrap().id();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: title,
            term: t.into(),
        })
    };
    let query = Query::Bool(BoolQuery {
        must: vec![term("purple"), term("hat")],
        ..BoolQuery::default()
    });
    let request = SearchRequest::default();
    let out = views
        .search_relaxed(&query, &request, &plan(1), &exec())
        .unwrap();
    let r = &out.relaxation;
    assert_eq!(r.applied, ["drop_terms:75%"]);
    assert_eq!(
        (r.original.as_deref(), r.effective.as_deref()),
        (None, None)
    );
    assert_eq!(by_step(&out), [(1, "b")]);

    // A `prefix` term's expansions are cut so the step fits beside L0's 50; the next step, beside both, fits no cut.
    let query = Query::Bool(BoolQuery {
        must: vec![
            Query::Prefix(PrefixQuery {
                field: title,
                prefix: "s".into(),
                max_expansions: 50,
            }),
            term("purple"),
            term("red"),
        ],
        ..BoolQuery::default()
    });
    let limits = QueryLimits {
        max_expansions: 60,
        ..QueryLimits::default()
    };
    let relax = plan(10_000).with_limits(limits);
    let out = views
        .search_relaxed(&query, &request, &relax, &exec())
        .unwrap();
    assert_eq!(
        out.relaxation.applied,
        ["drop_terms:75%:cut", "drop_terms:50%:skipped"]
    );
}

#[test]
fn a_query_of_one_term_skips_the_level() {
    let db = database();
    // No hit at L0, and nothing to drop: L2 builds no step, so `applied` does not list it and the search stays at L0.
    for text in [
        "purple",
        "purple -shoes",
        "\"purple running\"",
        "e-mail",
        "-red",
    ] {
        let out = search(&db, text, 10_000, None);
        let r = &out.relaxation;
        assert_eq!(r.level, RelaxLevel::Exact, "{text:?}");
        assert_eq!(r.applied, Vec::<String>::new(), "{text:?}");
        assert!(!r.relaxed(), "{text:?}");
    }
    // `-red` matches every other document at L0, and only those.
    let out = search(&db, "-red", 10_000, None);
    assert_eq!(by_step(&out), [(0, "c"), (0, "d"), (0, "e"), (0, "f")]);
}

/// SPEC §7.6: filters are never relaxed. Every step keeps L0's filter, so no hit of any step is outside it.
#[test]
fn filters_are_never_relaxed() {
    let db = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let title = c.schema().field("title").unwrap().id();
    let brand = c.schema().field("brand").unwrap().id();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: title,
            term: t.into(),
        })
    };
    let acme = Filter::Term {
        field: brand,
        value: FilterValue::Keyword("acme".into()),
    };
    let query = Query::Bool(BoolQuery {
        must: vec![term("purple"), term("red"), term("shoes")],
        filter: vec![acme],
        ..BoolQuery::default()
    });
    let out = views
        .search_relaxed(&query, &SearchRequest::default(), &plan(10_000), &exec())
        .unwrap();
    assert_eq!(out.relaxation.applied, ["drop_terms:75%", "drop_terms:50%"]);
    // Unfiltered, `b` (`red hat`) and `f` (`shoes box`) would match the 50% step: they are `other`.
    assert_eq!(by_step(&out), [(1, "a"), (2, "c")]);
}
