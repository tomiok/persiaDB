//! The cascade with stub levels (ROADMAP 8.1.2): each stub builds fixed steps and counts its calls.
//!
//! Invariants:
//! 1. A query with `min_results` hits at L0 returns what [`CollectionViews::search_with`] returns, and no later level
//!    builds a step.
//! 2. Otherwise the cascade runs each step in order until the hits found from the top reach `min_results` (with
//!    `fill`, also `offset + limit`); a level that builds no step is skipped.
//! 3. A document is a hit of the first step that matches it, once, at the score of that step's query alone.
//! 4. The page is the steps' hits in step order, from `offset`; facets count every step's matches.
//! 5. `relax: false`, and a request with a cursor (until ROADMAP 8.1.3), run L0 alone.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use persia_storage::container::sim::SimFile;

use crate::db::{Database, DatabaseOptions, Execution, SearchRequest, Threads};
use crate::document::{Document, Value};
use crate::query::{Query, TermQuery};
use crate::relax::{LevelInput, LevelTransform, RelaxLevel, RelaxOptions, RelaxPlan, Step};
use crate::schema::{
    FieldId, FieldKind, KeywordOptions, Levels, RelaxationConfig, SchemaBuilder, TextOptions,
};
use crate::search::{FacetRequest, Projection, Pruning, SearchPlan};
use crate::wal::Durability;

/// Builds fixed steps and counts its calls.
#[derive(Debug, Default)]
struct Stub {
    steps: Vec<Step>,
    calls: AtomicUsize,
}

impl Stub {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl LevelTransform for Stub {
    fn steps(&self, _input: &LevelInput<'_>) -> Vec<Step> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.steps.clone()
    }
}

/// Five documents over two segments and the memtable: `body` text, `tag` a facet.
fn database() -> (Database<SimFile>, FieldId, FieldId) {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field(
            "tag",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let tag = schema.field("tag").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    let docs = [
        ("a", "red shoes", "x"),
        ("b", "red hat hat", "y"),
        ("c", "blue shoes", "x"),
        ("d", "green hat", "y"),
        ("e", "blue hat and more words", "x"),
    ];
    for (i, (id, text, t)) in docs.into_iter().enumerate() {
        let doc = Document::new(id)
            .with(body, Value::String(text.into()))
            .with(tag, Value::String(t.into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i == 1 || i == 3 {
            db.commit().unwrap();
        }
    }
    (db, body, tag)
}

fn term(field: FieldId, t: &str) -> Query {
    Query::Term(TermQuery {
        field,
        term: t.into(),
    })
}

fn step(query: Query, applied: &str) -> Step {
    Step {
        query,
        applied: applied.into(),
    }
}

fn exec() -> Execution {
    Execution::new(Pruning::default()).threads(Threads::Current)
}

/// A plan of L0 and the stubs given, every other level dropped.
fn plan(options: &RelaxOptions, stubs: &[(RelaxLevel, &Arc<Stub>)]) -> RelaxPlan {
    let options = RelaxOptions {
        levels: Some(Levels::Custom(Vec::new())),
        fallback: Some(crate::schema::Fallback::None),
        ..options.clone()
    };
    stubs.iter().fold(
        RelaxPlan::new(&RelaxationConfig::default(), &options).unwrap(),
        |plan, (level, stub)| plan.with_level(*level, Arc::clone(stub) as _),
    )
}

fn min(n: u32) -> RelaxOptions {
    RelaxOptions {
        min_results: Some(n),
        ..RelaxOptions::default()
    }
}

fn ids(results: &super::RelaxedResults) -> Vec<(&str, usize)> {
    results
        .hits
        .iter()
        .map(|h| (h.hit.document.id(), h.step))
        .collect()
}

fn request(limit: usize) -> SearchRequest {
    SearchRequest {
        limit,
        projection: Projection::None,
        ..SearchRequest::default()
    }
}

#[test]
fn l0_with_enough_hits_is_the_plain_search_and_builds_no_level() {
    let (db, body, tag) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let later = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let relax = plan(&min(2), &[(RelaxLevel::Synonyms, &later)]);
    let query = term(body, "red");
    let request = SearchRequest {
        facets: vec![FacetRequest::new(tag, 10, c.schema()).unwrap()],
        explain: true,
        ..SearchRequest::default()
    };
    let relaxed = views
        .search_relaxed(&query, &request, &relax, &exec())
        .unwrap();
    let plain_plan =
        SearchPlan::with_terms(&query, c.schema(), &c.search_statistics(&memtable), &views)
            .unwrap();
    let plain = views.search_with(&plain_plan, &request, &exec()).unwrap();
    assert_eq!(later.calls(), 0);
    let hits: Vec<_> = relaxed.hits.iter().map(|h| h.hit.clone()).collect();
    assert_eq!(hits, plain.hits);
    assert!(
        relaxed
            .hits
            .iter()
            .all(|h| h.step == 0 && h.level == RelaxLevel::Exact)
    );
    assert_eq!(relaxed.facets, plain.facets);
    assert_eq!(relaxed.relaxation.level, RelaxLevel::Exact);
    assert_eq!(relaxed.relaxation.applied.len(), 0);
}

#[test]
fn an_empty_l0_escalates_to_the_first_level_that_builds_a_step_and_stops_there() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let none = Stub::new(Vec::new());
    let drop = Stub::new(vec![step(term(body, "shoes"), "drop_terms:50%")]);
    let after = Stub::new(vec![step(term(body, "hat"), "prefix")]);
    let relax = plan(
        &min(1),
        &[
            (RelaxLevel::Synonyms, &none),
            (RelaxLevel::DropTerms, &drop),
            (RelaxLevel::Prefix, &after),
        ],
    );
    let out = views
        .search_relaxed(&term(body, "purple"), &request(10), &relax, &exec())
        .unwrap();
    let mut got = ids(&out);
    got.sort_unstable();
    assert_eq!(got, [("a", 1), ("c", 1)]);
    assert!(out.hits.iter().all(|h| h.level == RelaxLevel::DropTerms));
    assert_eq!(out.relaxation.level, RelaxLevel::DropTerms);
    assert_eq!(out.relaxation.applied, ["drop_terms:50%"]);
    assert_eq!((none.calls(), drop.calls(), after.calls()), (1, 1, 0));
}

#[test]
fn min_results_counts_the_hits_of_every_step_and_each_document_once() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    // `hat` matches b, d and e; b is already an L0 hit of `red`.
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let later = Stub::new(vec![step(term(body, "shoes"), "drop_terms:50%")]);
    let relax = plan(
        &min(3),
        &[
            (RelaxLevel::Synonyms, &synonyms),
            (RelaxLevel::DropTerms, &later),
        ],
    );
    let out = views
        .search_relaxed(&term(body, "red"), &request(10), &relax, &exec())
        .unwrap();
    let got = ids(&out);
    assert_eq!(got.len(), 4, "{got:?}");
    let mut l0: Vec<_> = got[..2].to_vec();
    l0.sort_unstable();
    assert_eq!(l0, [("a", 0), ("b", 0)]);
    let mut l1: Vec<_> = got[2..].to_vec();
    l1.sort_unstable();
    assert_eq!(l1, [("d", 1), ("e", 1)]);
    // 4 hits reach 3: the next level is not built.
    assert_eq!(later.calls(), 0);
    assert_eq!(out.relaxation.level, RelaxLevel::Synonyms);
}

#[test]
fn a_relaxed_step_ranks_by_its_own_query_alone() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let relax = plan(&min(5), &[(RelaxLevel::Synonyms, &synonyms)]);
    let out = views
        .search_relaxed(&term(body, "red"), &request(10), &relax, &exec())
        .unwrap();
    let hat = SearchPlan::new(
        &term(body, "hat"),
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    let alone = views.search_with(&hat, &request(10), &exec()).unwrap();
    let step1: Vec<_> = out.hits.iter().filter(|h| h.step == 1).collect();
    assert_eq!(step1.len(), 2);
    for hit in step1 {
        let same = alone
            .hits
            .iter()
            .find(|h| h.location == hit.hit.location)
            .unwrap();
        assert_eq!(same.score.to_bits(), hit.hit.score.to_bits());
    }
    // In each step, by score: the order `hat` alone gives, b left out.
    let order: Vec<_> = alone
        .hits
        .iter()
        .map(|h| h.document.id())
        .filter(|&id| id != "b")
        .collect();
    let got: Vec<_> = out.hits[2..].iter().map(|h| h.hit.document.id()).collect();
    assert_eq!(got, order);
}

#[test]
fn without_fill_it_stops_at_min_results_and_with_fill_it_fills_the_page() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let drop = Stub::new(vec![step(term(body, "shoes"), "drop_terms:50%")]);
    let stubs = [
        (RelaxLevel::Synonyms, &synonyms),
        (RelaxLevel::DropTerms, &drop),
    ];
    let red = term(body, "red");
    let out = views
        .search_relaxed(&red, &request(5), &plan(&min(1), &stubs), &exec())
        .unwrap();
    assert_eq!(out.hits.len(), 2);
    assert_eq!(synonyms.calls(), 0);
    let fill = RelaxOptions {
        fill: Some(true),
        ..min(1)
    };
    // A window of 4: red's 2, then hat's d and e fill it.
    let out = views
        .search_relaxed(&red, &request(4), &plan(&fill, &stubs), &exec())
        .unwrap();
    assert_eq!(out.hits.len(), 4);
    assert_eq!((synonyms.calls(), drop.calls()), (1, 0));
    // A window of 5: shoes adds c (a is red's).
    let out = views
        .search_relaxed(&red, &request(5), &plan(&fill, &stubs), &exec())
        .unwrap();
    let got = ids(&out);
    assert_eq!(got.len(), 5);
    assert_eq!(got[4], ("c", 2));
    assert_eq!(out.relaxation.applied, ["synonyms", "drop_terms:50%"]);
    assert_eq!(out.relaxation.level, RelaxLevel::DropTerms);
}

#[test]
fn offset_and_limit_page_across_steps_and_min_results_counts_from_the_top() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let relax = plan(&min(4), &[(RelaxLevel::Synonyms, &synonyms)]);
    let red = term(body, "red");
    let all = views
        .search_relaxed(&red, &request(10), &relax, &exec())
        .unwrap();
    let all = ids(&all);
    assert_eq!(all.len(), 4);
    let page = SearchRequest {
        offset: 1,
        limit: 2,
        ..request(2)
    };
    let out = views.search_relaxed(&red, &page, &relax, &exec()).unwrap();
    assert_eq!(ids(&out), all[1..3]);
    // A page of one still relaxes: 2 hits from the top are fewer than 4.
    let out = views
        .search_relaxed(&red, &request(1), &relax, &exec())
        .unwrap();
    assert_eq!(ids(&out), all[..1]);
    assert_eq!(out.relaxation.level, RelaxLevel::Synonyms);
    // And a page of one does not relax a query with `min_results` hits from the top, more than the page holds.
    let calls = synonyms.calls();
    let two = plan(&min(2), &[(RelaxLevel::Synonyms, &synonyms)]);
    let out = views
        .search_relaxed(&red, &request(1), &two, &exec())
        .unwrap();
    assert_eq!(ids(&out), all[..1]);
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    assert_eq!(synonyms.calls(), calls);
    // A page past every hit is empty, the cascade run all the same.
    let past = SearchRequest {
        offset: 8,
        ..request(2)
    };
    let out = views.search_relaxed(&red, &past, &relax, &exec()).unwrap();
    assert_eq!(out.hits.len(), 0);
    assert_eq!(out.relaxation.level, RelaxLevel::Synonyms);
}

#[test]
fn facets_count_the_matches_of_every_step_run_once_each() {
    let (db, body, tag) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let relax = plan(&min(3), &[(RelaxLevel::Synonyms, &synonyms)]);
    let request = SearchRequest {
        limit: 1,
        facets: vec![FacetRequest::new(tag, 10, c.schema()).unwrap()],
        ..request(1)
    };
    let out = views
        .search_relaxed(&term(body, "red"), &request, &relax, &exec())
        .unwrap();
    // a, b (red) and d, e (hat): x for a and e, y for b and d; b counted once.
    let counts: Vec<(&[u8], u64)> = out.facets[0]
        .values
        .iter()
        .map(|v| (v.value.as_slice(), v.count))
        .collect();
    assert_eq!(counts, [(&b"x"[..], 2), (&b"y"[..], 2)]);
}

#[test]
fn explained_hits_add_up_to_their_scores_at_every_step() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let relax = plan(&min(4), &[(RelaxLevel::Synonyms, &synonyms)]);
    let request = SearchRequest {
        explain: true,
        ..request(10)
    };
    let out = views
        .search_relaxed(&term(body, "red"), &request, &relax, &exec())
        .unwrap();
    assert_eq!(out.hits.len(), 4);
    for hit in &out.hits {
        let explanation = hit.hit.explanation.as_ref().unwrap();
        assert_eq!(explanation.score().to_bits(), hit.hit.score.to_bits());
    }
}

#[test]
fn levels_without_a_step_are_skipped_and_the_default_plan_stops_at_l0_for_now() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    // Every level of the collection's defaults, none of which builds a step before ROADMAP 8.2.
    let relax = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::default()).unwrap();
    assert!(relax.relaxes());
    let out = views
        .search_relaxed(&term(body, "purple"), &request(10), &relax, &exec())
        .unwrap();
    assert_eq!(out.hits.len(), 0);
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    assert_eq!(out.relaxation.applied.len(), 0);
}

#[test]
fn relax_false_and_a_cursor_run_l0_alone() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    // `relax: false` over a collection whose defaults relax.
    let disabled = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::disabled()).unwrap();
    assert!(!disabled.relaxes());
    let out = views
        .search_relaxed(&term(body, "purple"), &request(10), &disabled, &exec())
        .unwrap();
    assert_eq!(out.hits.len(), 0);

    let relax = plan(&min(10), &[(RelaxLevel::Synonyms, &synonyms)]);
    let red = term(body, "red");
    let first = views
        .search_relaxed(&red, &request(1), &relax, &exec())
        .unwrap();
    assert_eq!(synonyms.calls(), 1);
    let next = SearchRequest {
        search_after: Some(first.hits[0].hit.cursor()),
        ..request(10)
    };
    let out = views.search_relaxed(&red, &next, &relax, &exec()).unwrap();
    // L0 alone after the cursor: red's other hit.
    assert_eq!(synonyms.calls(), 1);
    assert_eq!(out.hits.len(), 1);
    assert_eq!(out.hits[0].step, 0);
}
