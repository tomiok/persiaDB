//! The cascade with stub levels (ROADMAP 8.1.2, 8.1.3): each stub builds fixed steps and counts its calls.
//!
//! Invariants:
//! 1. A query with `min_results` hits at L0 returns what [`CollectionViews::search_with`] returns, and no later level
//!    builds a step.
//! 2. Otherwise the cascade runs each step in order until the hits found from the top reach `min_results` (with
//!    `fill`, also `offset + limit`); a level that builds no step is skipped.
//! 3. A document is a hit of the first step that matches it, once, at the score of that step's query alone.
//! 4. The page is the steps' hits in step order, from `offset`; facets count every step's matches.
//! 5. `relax: false` runs L0 alone.
//! 6. Hits rank by step first (ROADMAP 8.1.3); pages by cursor or by offset are slices of one ranking, every page
//!    tells the same cascade and counts the same facets, and with `fill` cursor pages relax past the first page.
//! 7. A cursor from a step after L0 is an error for a plain search, as one whose step is after its last.
//! 8. Rank scores never increase down a page by score.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use persia_storage::container::sim::SimFile;

use crate::db::{
    Database, DatabaseOptions, Execution, ManualClock, SearchRequest, StepCursor, Threads,
};
use crate::document::{Document, Value};
use crate::query::{Query, TermQuery};
use crate::relax::{LevelInput, LevelTransform, RelaxLevel, RelaxOptions, RelaxPlan, Step};
use crate::schema::{
    FieldId, FieldKind, KeywordOptions, Levels, RelaxationConfig, SchemaBuilder, TextOptions,
};
use crate::search::{
    FacetRequest, Interrupt, Projection, Pruning, RequestError, SearchError, SearchPlan,
};
use crate::segment::ReadError;
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
    fn steps(&self, _input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.steps.clone())
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
        effective: None,
        alternatives: None,
        corrected: Vec::new(),
    }
}

/// On the calling thread, with the budget read on a clock that never moves: a cascade is truncated only where a test
/// moves the clock (`mod budget`), never by how long a step took on the machine.
fn exec() -> Execution {
    Execution::new(Pruning::default())
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
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
fn a_level_of_several_steps_runs_them_in_order_and_stops_between_them() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    // `red` matches a and b, `hat` b, d and e: b is the first step's.
    let drop = Stub::new(vec![
        step(term(body, "red"), "drop_terms:75%"),
        step(term(body, "hat"), "drop_terms:50%"),
    ]);
    let after = Stub::new(vec![step(term(body, "shoes"), "prefix")]);
    let stubs = [(RelaxLevel::DropTerms, &drop), (RelaxLevel::Prefix, &after)];
    let purple = term(body, "purple");
    let out = views
        .search_relaxed(&purple, &request(10), &plan(&min(3), &stubs), &exec())
        .unwrap();
    let got = ids(&out);
    assert_eq!(got.len(), 4, "{got:?}");
    let mut first: Vec<_> = got[..2].to_vec();
    first.sort_unstable();
    assert_eq!(first, [("a", 1), ("b", 1)]);
    let mut second: Vec<_> = got[2..].to_vec();
    second.sort_unstable();
    assert_eq!(second, [("d", 2), ("e", 2)]);
    assert!(out.hits.iter().all(|h| h.level == RelaxLevel::DropTerms));
    assert_eq!(out.relaxation.level, RelaxLevel::DropTerms);
    assert_eq!(out.relaxation.applied, ["drop_terms:75%", "drop_terms:50%"]);
    // The level's transform runs once for both steps; 4 hits reach 3, so the next level is not built.
    assert_eq!((drop.calls(), after.calls()), (1, 0));
    // 2 hits reach 2 after the level's first step: its second is neither searched nor listed.
    let out = views
        .search_relaxed(&purple, &request(10), &plan(&min(2), &stubs), &exec())
        .unwrap();
    let mut got = ids(&out);
    got.sort_unstable();
    assert_eq!(got, [("a", 1), ("b", 1)]);
    assert_eq!(out.relaxation.level, RelaxLevel::DropTerms);
    assert_eq!(out.relaxation.applied, ["drop_terms:75%"]);
    assert_eq!(after.calls(), 0);
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
fn levels_without_a_step_are_skipped_and_one_term_relaxes_to_l3_and_l4() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    // Every level of the collection's defaults. For one programmatic term L1 builds no step (no text) and L2 skips it
    // (ROADMAP 8.2.2.3); L3 expands it (ROADMAP 8.2.3), to no term here, and so does L4 (ROADMAP 8.2.4: `purple` has 6
    // characters, distance 1); L5 waits on ROADMAP 8.2.5.
    let relax = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::default()).unwrap();
    assert!(relax.relaxes());
    let out = views
        .search_relaxed(&term(body, "purple"), &request(10), &relax, &exec())
        .unwrap();
    assert_eq!(out.hits.len(), 0);
    assert_eq!(out.relaxation.level, RelaxLevel::Fuzzy);
    assert_eq!(out.relaxation.applied, ["prefix", "fuzzy"]);
}

#[test]
fn relax_false_runs_l0_alone() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    // `relax: false` over a collection whose defaults relax.
    let disabled = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::disabled()).unwrap();
    assert!(!disabled.relaxes());
    let out = views
        .search_relaxed(&term(body, "purple"), &request(10), &disabled, &exec())
        .unwrap();
    assert_eq!(out.hits.len(), 0);
}

/// Every page of `limit` hits by cursor from the first, concatenated.
fn by_cursor(
    views: &crate::db::CollectionViews<'_>,
    query: &Query,
    base: &SearchRequest,
    relax: &RelaxPlan,
) -> Vec<super::RelaxedHit> {
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

fn strip(hits: &[super::RelaxedHit]) -> Vec<(String, usize, u64)> {
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

#[test]
fn a_cursor_pages_across_steps_as_offsets_do() {
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
    // 3 hits needed: red's 2 and hat's d and e (4); shoes is not reached.
    let relax = plan(&min(3), &stubs);
    let full = views
        .search_relaxed(&red, &request(10), &relax, &exec())
        .unwrap()
        .hits;
    assert_eq!(full.len(), 4);
    for k in 1..=4 {
        let paged = by_cursor(&views, &red, &request(k), &relax);
        assert_eq!(strip(&paged), strip(&full), "pages of {k}");
        for offset in 0..5 {
            let page = SearchRequest {
                offset,
                ..request(k)
            };
            let got = views.search_relaxed(&red, &page, &relax, &exec()).unwrap();
            let end = (offset + k).min(full.len());
            assert_eq!(strip(&got.hits), strip(&full[offset.min(end)..end]));
            // Every page tells the same cascade.
            assert_eq!(got.relaxation.level, RelaxLevel::Synonyms);
            assert_eq!(got.relaxation.applied, ["synonyms"]);
        }
    }
    // With `fill`, pages by cursor keep relaxing past the first page's last step: shoes adds c.
    let filling = plan(
        &RelaxOptions {
            fill: Some(true),
            ..min(1)
        },
        &stubs,
    );
    let paged = by_cursor(&views, &red, &request(2), &filling);
    let ids: Vec<(&str, usize)> = paged
        .iter()
        .map(|h| (h.hit.document.id(), h.step))
        .collect();
    assert_eq!(ids.len(), 5);
    assert_eq!(ids[4], ("c", 2));
}

#[test]
fn a_cursor_page_counts_the_facets_of_the_steps_before_it() {
    let (db, body, tag) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let relax = plan(&min(3), &[(RelaxLevel::Synonyms, &synonyms)]);
    let base = SearchRequest {
        facets: vec![FacetRequest::new(tag, 10, c.schema()).unwrap()],
        ..request(3)
    };
    let red = term(body, "red");
    let first = views.search_relaxed(&red, &base, &relax, &exec()).unwrap();
    let next = SearchRequest {
        search_after: Some(first.hits[2].cursor()),
        ..base
    };
    let second = views.search_relaxed(&red, &next, &relax, &exec()).unwrap();
    assert_eq!(second.hits.len(), 1);
    assert_eq!(second.hits[0].step, 1);
    assert_eq!(second.facets, first.facets);
}

#[test]
fn a_cursor_from_a_later_step_is_an_error_where_it_cannot_page() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let relax = plan(&min(5), &[(RelaxLevel::Synonyms, &synonyms)]);
    let red = term(body, "red");
    let out = views
        .search_relaxed(&red, &request(10), &relax, &exec())
        .unwrap();
    let later = out.hits.last().unwrap();
    assert_eq!(later.step, 1);
    let after = SearchRequest {
        search_after: Some(later.cursor()),
        ..request(10)
    };
    // A plain search returns L0's hits only, all ranking before the cursor.
    let plain =
        SearchPlan::with_terms(&red, c.schema(), &c.search_statistics(&memtable), &views).unwrap();
    assert!(matches!(
        views.search_with(&plain, &after, &exec()),
        Err(RequestError::CursorStep {
            step: 1,
            last: 1,
            ..
        })
    ));
    // A step-0 cursor pages a plain search as before.
    let l0 = SearchRequest {
        search_after: Some(out.hits[0].cursor()),
        ..request(10)
    };
    assert_eq!(
        views.search_with(&plain, &l0, &exec()).unwrap().hits.len(),
        1
    );
    // A cursor whose step is after its last was not made by a cascade.
    let mut broken = later.cursor();
    broken.relaxed = Some(StepCursor { step: 2, last: 1 });
    let broken = SearchRequest {
        search_after: Some(broken),
        ..request(10)
    };
    assert!(matches!(
        views.search_relaxed(&red, &broken, &relax, &exec()),
        Err(super::RelaxedSearchError::Request(
            RequestError::CursorStep {
                step: 2,
                last: 1,
                ..
            }
        ))
    ));
}

#[test]
fn rank_scores_never_increase_down_a_page_by_score() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let drop = Stub::new(vec![step(term(body, "shoes"), "drop_terms:50%")]);
    let relax = plan(
        &min(10),
        &[
            (RelaxLevel::Synonyms, &synonyms),
            (RelaxLevel::DropTerms, &drop),
        ],
    );
    let out = views
        .search_relaxed(&term(body, "red"), &request(10), &relax, &exec())
        .unwrap();
    assert_eq!(out.hits.len(), 5);
    for pair in out.hits.windows(2) {
        assert!(pair[0].step <= pair[1].step);
        assert!(pair[0].rank_score >= pair[1].rank_score, "{pair:?}");
    }
    for hit in &out.hits {
        assert_eq!(
            hit.rank_score,
            crate::relax::rank_score(hit.step, hit.hit.score)
        );
    }
}

mod random {
    //! Random corpora and fixed stub steps (ROADMAP 8.1.3): invariants 3, 4 and 6 against an oracle built from plain
    //! searches of each step's query alone.

    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::db::CollectionViews;
    use crate::query::BoolQuery;
    use crate::search::{Order, Sort, SortKey};

    const WORDS: [&str; 5] = ["w0", "w1", "w2", "w3", "w4"];

    #[derive(Debug, Clone)]
    struct Case {
        /// Each document's words and `n`.
        docs: Vec<(Vec<usize>, i64)>,
        /// Commit after these document indexes.
        commits: Vec<usize>,
        min_results: u32,
        fill: bool,
        k: usize,
        by_n: bool,
    }

    fn case() -> impl Strategy<Value = Case> {
        (
            prop::collection::vec(
                (prop::collection::vec(0..WORDS.len(), 1..5), -3i64..3),
                1..30,
            ),
            prop::collection::vec(0usize..30, 0..3),
            1u32..40,
            any::<bool>(),
            1usize..6,
            any::<bool>(),
        )
            .prop_map(|(docs, commits, min_results, fill, k, by_n)| Case {
                docs,
                commits,
                min_results,
                fill,
                k,
                by_n,
            })
    }

    /// The case's documents; `tag` is `t0` or `t1` by the parity of `n`, a facet.
    fn build(case: &Case) -> (Database<SimFile>, FieldId, FieldId, FieldId) {
        let schema = SchemaBuilder::new("c")
            .field("body", FieldKind::Text(TextOptions::default()))
            .field("n", FieldKind::I64 { stored: false })
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
        let n = schema.field("n").unwrap().id();
        let tag = schema.field("tag").unwrap().id();
        let mut db =
            Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default()).unwrap();
        db.create_collection(schema).unwrap();
        for (i, (words, value)) in case.docs.iter().enumerate() {
            let text: Vec<&str> = words.iter().map(|&w| WORDS[w]).collect();
            let doc = Document::new(format!("d{i}"))
                .with(body, Value::String(text.join(" ")))
                .with(n, Value::I64(*value))
                .with(tag, Value::String(tag_of(*value)));
            db.put("c", &doc, Durability::Buffered).unwrap();
            if case.commits.contains(&i) {
                db.commit().unwrap();
            }
        }
        (db, body, n, tag)
    }

    fn tag_of(n: i64) -> String {
        format!("t{}", n.rem_euclid(2))
    }

    /// Facet counts of `ids` (documents `d{i}`), as `(value, count)` sorted by value.
    fn tag_counts<'a>(case: &Case, ids: impl Iterator<Item = &'a str>) -> Vec<(Vec<u8>, u64)> {
        let mut counts = std::collections::BTreeMap::<Vec<u8>, u64>::new();
        for id in ids {
            let i: usize = id.strip_prefix('d').unwrap().parse().unwrap();
            *counts
                .entry(tag_of(case.docs[i].1).into_bytes())
                .or_default() += 1;
        }
        counts.into_iter().collect()
    }

    fn counted(out: &super::super::RelaxedResults) -> Vec<(Vec<u8>, u64)> {
        let mut got: Vec<_> = out.facets[0]
            .values
            .iter()
            .map(|v| (v.value.clone(), v.count))
            .collect();
        got.sort_unstable();
        got
    }

    /// L0 `w0 AND w1`, then `w0 OR w1` (synonyms), then `w2` and `w3` (two `drop_terms` steps), nothing at prefix, and
    /// every document (fuzzy, scoring 0: ties by location).
    fn steps(body: FieldId) -> (Query, Vec<(RelaxLevel, Vec<Step>)>) {
        let t = |w: &str| term(body, w);
        let l0 = Query::Bool(BoolQuery {
            must: vec![t("w0"), t("w1")],
            ..BoolQuery::default()
        });
        let or = Query::Bool(BoolQuery {
            should: vec![t("w0"), t("w1")],
            ..BoolQuery::default()
        });
        let levels = vec![
            (RelaxLevel::Synonyms, vec![step(or, "synonyms")]),
            (
                RelaxLevel::DropTerms,
                vec![
                    step(t("w2"), "drop_terms:75%"),
                    step(t("w3"), "drop_terms:50%"),
                ],
            ),
            (RelaxLevel::Prefix, Vec::new()),
            (RelaxLevel::Fuzzy, vec![step(Query::MatchAll, "fuzzy")]),
        ];
        (l0, levels)
    }

    type Row = (String, usize, u64);

    /// The ranking the cascade must give: each step's plain hits, by the sort, without the documents of earlier
    /// steps, through the step where the cumulative count first reaches `min_results` (every step with `fill` and a
    /// window past the corpus).
    fn oracle(
        views: &CollectionViews<'_>,
        c: &crate::db::CollectionSnapshot,
        memtable: &crate::memtable::MemtableView,
        queries: &[Query],
        sort: &Sort,
        case: &Case,
    ) -> Vec<Row> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for (i, q) in queries.iter().enumerate() {
            let plan = SearchPlan::with_terms(q, c.schema(), &c.search_statistics(memtable), views)
                .unwrap();
            let all = SearchRequest {
                sort: sort.clone(),
                ..request(1000)
            };
            for hit in views.search_with(&plan, &all, &exec()).unwrap().hits {
                let id = hit.document.id().to_owned();
                if seen.insert(id.clone()) {
                    out.push((id, i, hit.score.to_bits()));
                }
            }
            if !case.fill && out.len() >= usize::try_from(case.min_results).unwrap() {
                break;
            }
        }
        out
    }

    fn rows(hits: &[crate::db::RelaxedHit]) -> Vec<Row> {
        strip(hits)
    }

    #[allow(clippy::too_many_lines)] // One case, every invariant against the same oracle.
    fn check(case: &Case) -> Result<(), TestCaseError> {
        let (db, body, n, tag) = build(case);
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let (l0, levels) = steps(body);
        let options = RelaxOptions {
            min_results: Some(case.min_results),
            fill: Some(case.fill),
            ..RelaxOptions::default()
        };
        let stubs: Vec<(RelaxLevel, Arc<Stub>)> = levels
            .iter()
            .map(|(level, steps)| (*level, Stub::new(steps.clone())))
            .collect();
        let refs: Vec<(RelaxLevel, &Arc<Stub>)> = stubs.iter().map(|(l, s)| (*l, s)).collect();
        let relax = plan(&options, &refs);
        let sort = if case.by_n {
            Sort::new([SortKey::field(n, Order::Asc)], c.schema()).unwrap()
        } else {
            Sort::score()
        };
        let mut queries = vec![l0.clone()];
        queries.extend(
            levels
                .iter()
                .flat_map(|(_, s)| s.iter().map(|s| s.query.clone())),
        );
        let want = oracle(&views, c, &memtable, &queries, &sort, case);

        let base = SearchRequest {
            sort: sort.clone(),
            ..request(1000)
        };
        let full = views.search_relaxed(&l0, &base, &relax, &exec()).unwrap();
        prop_assert_eq!(rows(&full.hits), want.clone());
        // Step order, and each document once.
        prop_assert!(full.hits.windows(2).all(|w| w[0].step <= w[1].step));
        if !case.by_n {
            prop_assert!(
                full.hits
                    .windows(2)
                    .all(|w| w[0].rank_score >= w[1].rank_score)
            );
        }
        // Facets count every step built, each document once; explanations score their hits, bit for bit. Without
        // `fill`, a page by offset or by cursor builds the same steps and counts the same.
        let facets = vec![FacetRequest::new(tag, 10, c.schema()).unwrap()];
        let faceted = SearchRequest {
            facets: facets.clone(),
            explain: true,
            ..base.clone()
        };
        let out = views
            .search_relaxed(&l0, &faceted, &relax, &exec())
            .unwrap();
        let union = tag_counts(case, want.iter().map(|(id, ..)| id.as_str()));
        prop_assert_eq!(counted(&out), union.clone());
        for hit in &out.hits {
            let e = hit.hit.explanation.as_ref().unwrap();
            prop_assert_eq!(e.score().to_bits(), hit.hit.score.to_bits());
        }
        if !case.fill {
            let first = SearchRequest {
                sort: sort.clone(),
                facets,
                ..request(case.k)
            };
            let page = views.search_relaxed(&l0, &first, &relax, &exec()).unwrap();
            prop_assert_eq!(counted(&page), union.clone());
            if let Some(last) = page.hits.last() {
                let next = SearchRequest {
                    search_after: Some(last.cursor()),
                    ..first.clone()
                };
                let next = views.search_relaxed(&l0, &next, &relax, &exec()).unwrap();
                prop_assert_eq!(counted(&next), union.clone());
                prop_assert_eq!(&next.relaxation, &page.relaxation);
            }
        }
        // Every pruning strategy and thread layout gives the same hits, bit for bit.
        for pruning in [
            Pruning::Exhaustive,
            Pruning::MaxScore,
            Pruning::BlockMaxWand,
        ] {
            for threads in [Threads::Current, Threads::Global] {
                let e = Execution::new(pruning)
                    .threads(threads)
                    .budget_clock(Arc::new(ManualClock::new(0)));
                let got = views.search_relaxed(&l0, &base, &relax, &e).unwrap();
                prop_assert_eq!(rows(&got.hits), want.clone(), "{:?}", pruning);
            }
        }
        // Pages by offset and by cursor are slices of the ranking.
        let page = SearchRequest {
            sort: sort.clone(),
            ..request(case.k)
        };
        for offset in [0, 1, case.k, 2 * case.k + 1] {
            let at = SearchRequest {
                offset,
                ..page.clone()
            };
            let got = views.search_relaxed(&l0, &at, &relax, &exec()).unwrap();
            let end = (offset + case.k).min(want.len());
            prop_assert_eq!(rows(&got.hits), want[offset.min(end)..end].to_vec());
        }
        let paged = by_cursor(&views, &l0, &page, &relax);
        prop_assert_eq!(rows(&paged), want);
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 512 } else { 48 }))]

        #[test]
        fn the_cascade_ranks_pages_and_prunes_as_one_ranking(case in case()) {
            check(&case)?;
        }
    }
}

mod budget {
    //! The cascade's budget (ROADMAP 8.1.4, SPEC §7.6), on injected clocks only: it starts when L0 finishes, is
    //! checked before each level and step and inside each step's search, and drops a step it passes whole.

    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::db::Clock;
    use crate::search::Interrupt;

    /// Moves `by` on every read of [`Clock::now`].
    #[derive(Debug)]
    struct Ticking {
        now: Mutex<Instant>,
        by: Duration,
    }

    impl Ticking {
        fn new(by_ms: u64) -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(Instant::now()),
                by: Duration::from_millis(by_ms),
            })
        }
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

    /// Builds fixed steps and moves `clock` by `by` each time it does: a level whose building takes time.
    #[derive(Debug)]
    struct Slow {
        steps: Vec<Step>,
        clock: Arc<ManualClock>,
        by: Duration,
        calls: AtomicUsize,
    }

    impl LevelTransform for Slow {
        fn steps(&self, _input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.clock.advance(self.by);
            Ok(self.steps.clone())
        }
    }

    fn slow(clock: &Arc<ManualClock>, by_ms: u64, steps: Vec<Step>) -> Arc<Slow> {
        Arc::new(Slow {
            steps,
            clock: Arc::clone(clock),
            by: Duration::from_millis(by_ms),
            calls: AtomicUsize::new(0),
        })
    }

    fn budget(ms: u32, n: u32) -> RelaxOptions {
        RelaxOptions {
            budget_ms: Some(ms),
            ..min(n)
        }
    }

    #[test]
    fn l0_is_never_cut_and_a_budget_passed_after_it_stops_before_the_next_level() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
        let relax = plan(&budget(1, 10), &[(RelaxLevel::Synonyms, &synonyms)]);
        // 10 ms a read, 1 ms of budget: past it at the first read after L0.
        let exec = exec().budget_clock(Ticking::new(10));
        let out = views
            .search_relaxed(&term(body, "red"), &request(10), &relax, &exec)
            .unwrap();
        assert_eq!(out.hits.len(), 2);
        assert!(out.hits.iter().all(|h| h.step == 0));
        assert!(out.relaxation.truncated);
        assert_eq!(out.relaxation.level, RelaxLevel::Exact);
        assert_eq!(out.relaxation.applied.len(), 0);
        assert_eq!(synonyms.calls(), 0);
    }

    #[test]
    fn a_level_that_ends_past_the_budget_keeps_the_steps_completed_before() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let clock = Arc::new(ManualClock::new(0));
        // Each level takes 30 ms to build: synonyms' step starts at 30 ms of a 50 ms budget, drop_terms' at 60.
        let synonyms = slow(&clock, 30, vec![step(term(body, "hat"), "synonyms")]);
        let drop = slow(
            &clock,
            30,
            vec![step(term(body, "shoes"), "drop_terms:50%")],
        );
        let relax = plan(&budget(50, 10), &[])
            .with_level(RelaxLevel::Synonyms, Arc::clone(&synonyms) as _)
            .with_level(RelaxLevel::DropTerms, Arc::clone(&drop) as _);
        let exec = exec().budget_clock(Arc::clone(&clock) as _);
        let out = views
            .search_relaxed(&term(body, "red"), &request(10), &relax, &exec)
            .unwrap();
        let steps: Vec<usize> = out.hits.iter().map(|h| h.step).collect();
        assert_eq!(steps, [0, 0, 1, 1]);
        assert!(out.relaxation.truncated);
        assert_eq!(out.relaxation.level, RelaxLevel::Synonyms);
        assert_eq!(out.relaxation.applied, ["synonyms"]);
        // drop_terms was built, then found the budget spent before its step ran.
        assert_eq!(drop.calls.load(Ordering::Relaxed), 1);
        // The page's cursor records the last step completed.
        assert_eq!(
            out.hits.last().unwrap().cursor().relaxed,
            Some(StepCursor { step: 1, last: 1 })
        );
        // The same cascade with time to spare is not truncated and reaches drop_terms.
        let roomy = plan(&budget(1_000, 10), &[])
            .with_level(RelaxLevel::Synonyms, Arc::clone(&synonyms) as _)
            .with_level(RelaxLevel::DropTerms, Arc::clone(&drop) as _);
        let out = views
            .search_relaxed(&term(body, "red"), &request(10), &roomy, &exec)
            .unwrap();
        assert!(!out.relaxation.truncated);
        assert_eq!(out.hits.len(), 5);
    }

    #[test]
    fn a_step_the_budget_passes_inside_is_dropped_whole() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
        let relax = plan(&budget(10, 10), &[(RelaxLevel::Synonyms, &synonyms)]);
        // Reads after L0: the budget's start (t), before the level (t + 4), before the step (t + 8, within 10), then
        // the step's search reads t + 12, past it.
        let exec = exec().budget_clock(Ticking::new(4));
        let out = views
            .search_relaxed(&term(body, "red"), &request(10), &relax, &exec)
            .unwrap();
        assert_eq!(synonyms.calls(), 1);
        assert_eq!(out.hits.len(), 2);
        assert!(out.hits.iter().all(|h| h.step == 0));
        assert!(out.relaxation.truncated);
        assert_eq!(out.relaxation.level, RelaxLevel::Exact);
        assert_eq!(out.relaxation.applied.len(), 0);
    }

    /// Walks the views' terms of `field` as it builds, as L4 does for its corrections, then builds `steps`.
    #[derive(Debug)]
    struct Walking {
        field: FieldId,
        steps: Vec<Step>,
    }

    impl LevelTransform for Walking {
        fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
            let terms = input.terms.expect("the search gives the views' terms");
            terms.each_term(
                self.field,
                std::ops::Bound::Unbounded,
                std::ops::Bound::Unbounded,
                &crate::search::Expansion::all(),
                &mut |_, _| Ok(()),
            )?;
            Ok(self.steps.clone())
        }
    }

    #[test]
    fn a_level_whose_term_walk_the_budget_passes_truncates_the_cascade() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let walking = Arc::new(Walking {
            field: body,
            steps: vec![step(term(body, "hat"), "fuzzy")],
        });
        let relax = plan(&budget(6, 10), &[]).with_level(RelaxLevel::Fuzzy, walking.clone() as _);
        // Reads after L0: the budget's start (t), before the level (t + 4), then the walk's first check (t + 8), past
        // it: the cascade stops with L0's hits, not an error.
        let out = views
            .search_relaxed(
                &term(body, "red"),
                &request(10),
                &relax,
                &exec().budget_clock(Ticking::new(4)),
            )
            .unwrap();
        assert!(out.relaxation.truncated);
        assert_eq!(out.relaxation.level, RelaxLevel::Exact);
        assert!(out.hits.iter().all(|h| h.step == 0));
        // With time to spare, the walk ends and the step runs.
        let roomy = plan(&budget(1_000, 10), &[]).with_level(RelaxLevel::Fuzzy, walking as _);
        let out = views
            .search_relaxed(&term(body, "red"), &request(10), &roomy, &exec())
            .unwrap();
        assert!(!out.relaxation.truncated);
        assert_eq!(out.relaxation.level, RelaxLevel::Fuzzy);
    }

    /// Moves the request's deadline clock past its deadline, then walks as [`Walking`] does.
    #[derive(Debug)]
    struct LateWalk {
        walk: Walking,
        deadline: Arc<ManualClock>,
    }

    impl LevelTransform for LateWalk {
        fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
            self.deadline.advance(Duration::from_millis(10));
            self.walk.steps(input)
        }
    }

    #[test]
    fn the_requests_deadline_passing_during_a_levels_walk_stays_an_error() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let deadline = Arc::new(ManualClock::new(0));
        let late = Arc::new(LateWalk {
            walk: Walking {
                field: body,
                steps: vec![step(term(body, "hat"), "fuzzy")],
            },
            deadline: Arc::clone(&deadline),
        });
        // A budget that never passes (its own frozen clock), and a deadline 5 ms away that the level's build passes.
        let relax = plan(&budget(1_000, 10), &[]).with_level(RelaxLevel::Fuzzy, late as _);
        let at = Clock::now(&*deadline) + Duration::from_millis(5);
        let exec = exec().deadline(Arc::clone(&deadline) as _, at);
        let out = views.search_relaxed(&term(body, "red"), &request(10), &relax, &exec);
        assert!(
            matches!(
                out,
                Err(crate::db::RelaxedSearchError::Plan(
                    SearchError::Interrupted(Interrupt::DeadlineExceeded)
                ))
            ),
            "{out:?}"
        );
    }

    #[test]
    fn a_truncated_step_counts_no_facet_and_its_cursor_page_resumes() {
        let (db, body, tag) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
        let relax = plan(&budget(10, 10), &[(RelaxLevel::Synonyms, &synonyms)]);
        let request = SearchRequest {
            facets: vec![FacetRequest::new(tag, 10, c.schema()).unwrap()],
            ..request(10)
        };
        // As `a_step_the_budget_passes_inside_is_dropped_whole`: the step is built, then cut inside its search.
        let out = views
            .search_relaxed(
                &term(body, "red"),
                &request,
                &relax,
                &exec().budget_clock(Ticking::new(4)),
            )
            .unwrap();
        assert!(out.relaxation.truncated);
        let counted: u64 = out.facets[0].values.iter().map(|v| v.count).sum();
        assert_eq!(counted, 2, "only red's documents: hat's step was dropped");
        // Its last hit's cursor records the last step completed, L0; a page after it stays at L0, with time to spare too.
        let cursor = out.hits.last().unwrap().cursor();
        assert_eq!(cursor.relaxed, None);
        let next = SearchRequest {
            search_after: Some(cursor),
            ..request
        };
        let out = views
            .search_relaxed(&term(body, "red"), &next, &relax, &exec())
            .unwrap();
        assert!(!out.relaxation.truncated);
        assert_eq!(
            out.hits.len(),
            0,
            "the cursor's cascade stopped at L0, and a page after it does too"
        );
    }

    #[test]
    fn without_a_budget_clock_the_budget_reads_the_deadlines() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let clock = Arc::new(ManualClock::new(0));
        let synonyms = slow(&clock, 60, vec![step(term(body, "hat"), "synonyms")]);
        let relax = plan(&budget(50, 10), &[]).with_level(RelaxLevel::Synonyms, synonyms as _);
        // A deadline far away on the manual clock, and no budget clock: the budget reads the manual one too, which
        // synonyms moves 60 ms, past the 50 ms budget, before its step.
        let far = clock.now() + Duration::from_secs(3600);
        let exec = Execution::new(Pruning::default())
            .threads(Threads::Current)
            .deadline(Arc::clone(&clock) as _, far);
        let out = views
            .search_relaxed(&term(body, "red"), &request(10), &relax, &exec)
            .unwrap();
        assert!(out.relaxation.truncated);
        assert_eq!(out.hits.len(), 2);
    }
    #[test]
    fn the_requests_own_deadline_and_token_are_errors_not_a_truncation() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let clock = Arc::new(ManualClock::new(0));
        let synonyms = slow(&clock, 30, vec![step(term(body, "hat"), "synonyms")]);
        let relax = plan(&budget(50, 10), &[]).with_level(RelaxLevel::Synonyms, synonyms as _);
        // The deadline at 20 ms passes while synonyms builds, inside the budget.
        let at = clock.now() + Duration::from_millis(20);
        let exec = exec()
            .budget_clock(Arc::clone(&clock) as _)
            .deadline(Arc::clone(&clock) as _, at);
        let err = views
            .search_relaxed(&term(body, "red"), &request(10), &relax, &exec)
            .unwrap_err();
        assert!(
            matches!(
                err,
                super::super::RelaxedSearchError::Request(RequestError::Interrupted(
                    Interrupt::DeadlineExceeded
                ))
            ),
            "{err}"
        );
        // A token cancelled is an error too.
        let token = crate::db::CancelToken::new();
        token.cancel();
        let err = views
            .search_relaxed(
                &term(body, "red"),
                &request(10),
                &relax,
                &super::exec().cancel(token),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            super::super::RelaxedSearchError::Request(RequestError::Interrupted(
                Interrupt::Cancelled
            ))
        ));
    }
}

#[test]
fn the_response_states_what_the_cascade_did() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let none = Stub::new(Vec::new());
    let drop = Stub::new(vec![
        step(term(body, "purple"), "drop_terms:75%"),
        step(term(body, "shoes"), "drop_terms:50%"),
    ]);
    let relax = plan(
        &min(1),
        &[
            (RelaxLevel::Synonyms, &none),
            (RelaxLevel::DropTerms, &drop),
        ],
    );
    // L0 has no hit; synonyms builds nothing and is not listed; drop_terms' first step has no hit either.
    let out = views
        .search_relaxed(&term(body, "purple"), &request(10), &relax, &exec())
        .unwrap();
    let r = &out.relaxation;
    assert_eq!(r.level, RelaxLevel::DropTerms);
    assert_eq!(r.level.name(), "drop_terms");
    assert_eq!(r.applied, ["drop_terms:75%", "drop_terms:50%"]);
    assert!(r.relaxed());
    assert!(!r.truncated);
    // A programmatic query has no text (ROADMAP 8.1.7); corrections wait on ROADMAP 8.2.4.3.
    assert_eq!(r.original, None);
    assert_eq!(r.effective, None);
    assert_eq!(r.corrected, []);
    assert!(
        out.hits
            .iter()
            .all(|h| h.level == RelaxLevel::DropTerms && h.step == 2)
    );
    // A query with hits at L0 states that nothing was relaxed.
    let out = views
        .search_relaxed(&term(body, "red"), &request(10), &relax, &exec())
        .unwrap();
    assert!(!out.relaxation.relaxed());
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    assert_eq!(out.relaxation.applied.len(), 0);
}

#[test]
fn with_fill_each_page_states_the_steps_it_built() {
    let (db, body, tag) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let fill = RelaxOptions {
        fill: Some(true),
        ..min(1)
    };
    let relax = plan(&fill, &[(RelaxLevel::Synonyms, &synonyms)]);
    let base = SearchRequest {
        facets: vec![FacetRequest::new(tag, 10, c.schema()).unwrap()],
        ..request(2)
    };
    let red = term(body, "red");
    // Page 1 (window 2) is full at L0: it builds nothing more, and counts red's documents only.
    let first = views.search_relaxed(&red, &base, &relax, &exec()).unwrap();
    assert_eq!(first.relaxation.applied.len(), 0);
    let count =
        |out: &super::RelaxedResults| -> u64 { out.facets[0].values.iter().map(|v| v.count).sum() };
    assert_eq!(count(&first), 2);
    // Page 2, by cursor or by offset, needs more: it builds synonyms and counts its documents too.
    let by_cursor = SearchRequest {
        search_after: Some(first.hits[1].cursor()),
        ..base.clone()
    };
    let by_offset = SearchRequest {
        offset: 2,
        ..base.clone()
    };
    for next in [by_cursor, by_offset] {
        let second = views.search_relaxed(&red, &next, &relax, &exec()).unwrap();
        assert_eq!(second.relaxation.applied, ["synonyms"]);
        assert_eq!(second.relaxation.level, RelaxLevel::Synonyms);
        assert_eq!(count(&second), 4);
        assert_eq!(second.hits.len(), 2);
    }
}

#[test]
fn cursors_from_other_searches_page_as_their_step_says() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let synonyms = Stub::new(vec![step(term(body, "hat"), "synonyms")]);
    let relax = plan(&min(10), &[(RelaxLevel::Synonyms, &synonyms)]);
    let red = term(body, "red");
    // A plain search's cursor (no step) is step 0 of a cascade that stopped at L0: L0 after it, and no further.
    let plain =
        SearchPlan::with_terms(&red, c.schema(), &c.search_statistics(&memtable), &views).unwrap();
    let first = views.search_with(&plain, &request(1), &exec()).unwrap();
    assert_eq!(first.hits[0].cursor().relaxed, None);
    let next = SearchRequest {
        search_after: Some(first.hits[0].cursor()),
        ..request(10)
    };
    let out = views.search_relaxed(&red, &next, &relax, &exec()).unwrap();
    assert_eq!(out.hits.len(), 1);
    assert_eq!(
        (out.hits[0].step, out.relaxation.level),
        (0, RelaxLevel::Exact)
    );
    // The inner hit's cursor is the relaxed hit's: it keeps the step.
    let relaxed = views
        .search_relaxed(&red, &request(10), &relax, &exec())
        .unwrap();
    let later = relaxed.hits.last().unwrap();
    assert_eq!(later.hit.cursor(), later.cursor());
    assert_eq!(
        later.cursor().relaxed,
        Some(StepCursor { step: 1, last: 1 })
    );
    // A cursor from a step this plan no longer builds (another plan) pages nothing, without an error.
    let nothing = Stub::new(Vec::new());
    let fewer = plan(&min(10), &[(RelaxLevel::Synonyms, &nothing)]);
    let after = SearchRequest {
        search_after: Some(later.cursor()),
        ..request(10)
    };
    let out = views.search_relaxed(&red, &after, &fewer, &exec()).unwrap();
    assert_eq!(out.hits.len(), 0);
    // `relax: false` with a cursor from a later step is an error, as for a plain search.
    let off = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::disabled()).unwrap();
    assert!(matches!(
        views.search_relaxed(&red, &after, &off, &exec()),
        Err(super::RelaxedSearchError::Request(
            RequestError::CursorStep { step: 1, .. }
        ))
    ));
}

/// ROADMAP 8.1.7: a query compiled from user text states that text (`original`) and the text of its last step
/// (`effective`); its levels build from that compilation. A programmatic query states neither.
#[test]
fn a_compiled_query_states_its_text_and_its_levels_read_it() {
    use std::sync::Mutex;

    use persia_analysis::AnalyzerRegistry;

    use crate::query::{DefaultFields, QueryCompiler, QueryLimits};

    /// Builds `steps` and records the text of the compilation it was given.
    #[derive(Debug)]
    struct Reads {
        steps: Vec<Step>,
        seen: Mutex<Vec<Option<String>>>,
    }
    impl LevelTransform for Reads {
        fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
            self.seen
                .lock()
                .unwrap()
                .push(input.compiled.map(|c| c.text.clone()));
            Ok(self.steps.clone())
        }
    }

    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let compiler = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin()).unwrap();
    let fields = DefaultFields::all(c.schema());
    let compile = |text: &str| compiler.compile(text, &fields, &QueryLimits::default());

    let shoes = Step {
        effective: Some("shoes".into()),
        ..step(term(body, "shoes"), "drop_terms:50%")
    };
    let reads = Arc::new(Reads {
        steps: vec![step(term(body, "purple"), "drop_terms:75%"), shoes],
        seen: Mutex::new(Vec::new()),
    });
    let relax = plan(&min(1), &[]).with_level(RelaxLevel::DropTerms, Arc::clone(&reads) as _);

    // L0 has no hit: the last step built states its own text.
    let purple = compile("  purple \t shoes ");
    let out = views
        .search_relaxed(&purple, &request(10), &relax, &exec())
        .unwrap();
    let r = &out.relaxation;
    assert_eq!(r.applied, ["drop_terms:75%", "drop_terms:50%"]);
    assert_eq!(r.original.as_deref(), Some("  purple \t shoes "));
    assert_eq!(r.effective.as_deref(), Some("shoes"));
    assert_eq!(*reads.seen.lock().unwrap(), [Some(purple.text.clone())]);

    // L0 has hits: `effective` is the clauses that compiled, one space apart.
    let red = compile("red   shoes");
    let out = views
        .search_relaxed(&red, &request(10), &relax, &exec())
        .unwrap();
    assert!(!out.relaxation.relaxed());
    assert_eq!(out.relaxation.original.as_deref(), Some("red   shoes"));
    assert_eq!(out.relaxation.effective.as_deref(), Some("red shoes"));

    // The same query built in code has no text, and its levels get none.
    let out = views
        .search_relaxed(&purple.query, &request(10), &relax, &exec())
        .unwrap();
    assert_eq!(out.relaxation.applied.len(), 2);
    assert_eq!(
        (out.relaxation.original, out.relaxation.effective),
        (None, None)
    );
    assert_eq!(reads.seen.lock().unwrap().last(), Some(&None));
}

/// ROADMAP 8.1.8: each step after L0 is held to the plan's limits as the query it runs, with the earlier steps it
/// excludes (SPEC §7.1.3). Expansions are cut to fit; a step no cut fits is skipped and listed.
mod limits {
    use super::*;
    use crate::query::{BoolQuery, PrefixQuery, QueryError, QueryLimits};
    use crate::search::{FacetRequest, SearchError};

    fn any_of(body: FieldId, words: &[&str]) -> Query {
        Query::Bool(BoolQuery {
            should: words.iter().map(|w| term(body, w)).collect(),
            ..BoolQuery::default()
        })
    }

    fn all_of(body: FieldId, words: &[&str]) -> Query {
        Query::Bool(BoolQuery {
            must: words.iter().map(|w| term(body, w)).collect(),
            ..BoolQuery::default()
        })
    }

    fn prefix(body: FieldId, p: &str, max_expansions: u32) -> Query {
        Query::Prefix(PrefixQuery {
            field: body,
            prefix: p.into(),
            max_expansions,
        })
    }

    fn sorted(out: &super::super::RelaxedResults) -> Vec<(&str, usize)> {
        let mut ids = ids(out);
        ids.sort_unstable();
        ids
    }

    fn terms(max_terms: usize) -> QueryLimits {
        QueryLimits {
            max_terms,
            ..QueryLimits::default()
        }
    }

    #[test]
    fn a_step_over_the_limits_is_skipped_listed_and_not_excluded() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let drop = Stub::new(vec![
            step(any_of(body, &["red", "shoes", "hat", "blue"]), "wide"),
            step(term(body, "red"), "red"),
        ]);
        let relax = plan(&min(1), &[(RelaxLevel::DropTerms, &drop)]).with_limits(terms(3));
        assert_eq!(relax.limits(), Some(&terms(3)));
        let out = views
            .search_relaxed(&term(body, "purple"), &request(10), &relax, &exec())
            .unwrap();
        let r = &out.relaxation;
        assert_eq!(r.applied, ["wide:skipped", "red"]);
        assert_eq!(r.level, RelaxLevel::DropTerms);
        assert!(r.relaxed());
        // The wide step would have matched `a` and `b` first; skipped, it excludes nothing.
        assert_eq!(sorted(&out), [("a", 2), ("b", 2)]);
        // The default limits fit it: it runs, and the next step is not needed.
        let relax = plan(&min(1), &[(RelaxLevel::DropTerms, &drop)]);
        let out = views
            .search_relaxed(&term(body, "purple"), &request(10), &relax, &exec())
            .unwrap();
        assert_eq!(out.relaxation.applied, ["wide"]);
    }

    #[test]
    fn the_steps_a_step_excludes_count_against_the_limits() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        // L0 holds two terms. The first step's two fit alone but not beside them; the second's one does.
        let drop = Stub::new(vec![
            step(any_of(body, &["red", "blue"]), "two"),
            step(term(body, "blue"), "one"),
        ]);
        let relax = plan(&min(1), &[(RelaxLevel::DropTerms, &drop)]).with_limits(terms(3));
        let l0 = all_of(body, &["purple", "violet"]);
        let out = views
            .search_relaxed(&l0, &request(10), &relax, &exec())
            .unwrap();
        assert_eq!(out.relaxation.applied, ["two:skipped", "one"]);
        assert_eq!(sorted(&out), [("c", 2), ("e", 2)]);
        // A searched step counts against the later ones: after `violet` (one term), `red` no longer fits.
        let drop = Stub::new(vec![
            step(term(body, "violet"), "one"),
            step(term(body, "red"), "red"),
        ]);
        let relax = plan(&min(1), &[(RelaxLevel::DropTerms, &drop)]).with_limits(terms(3));
        let out = views
            .search_relaxed(&l0, &request(10), &relax, &exec())
            .unwrap();
        assert_eq!(out.relaxation.applied, ["one", "red:skipped"]);
        assert_eq!(out.relaxation.level, RelaxLevel::DropTerms);
        assert_eq!(out.hits, []);
    }

    #[test]
    fn expansions_are_cut_to_what_is_left_and_listed() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let prefixes = Stub::new(vec![step(prefix(body, "sh", 50), "prefix")]);
        let limits = QueryLimits {
            max_expansions: 51,
            ..QueryLimits::default()
        };
        let relax = plan(&min(1), &[(RelaxLevel::Prefix, &prefixes)]).with_limits(limits);
        // L0's 50 expansions leave the step 1: `shoes`, the only `sh` term.
        let out = views
            .search_relaxed(&prefix(body, "pur", 50), &request(10), &relax, &exec())
            .unwrap();
        assert_eq!(out.relaxation.applied, ["prefix:cut"]);
        assert_eq!(out.relaxation.level, RelaxLevel::Prefix);
        assert_eq!(sorted(&out), [("a", 1), ("c", 1)]);
        // The cut step scores as the cut query alone.
        let plain = views
            .search_relaxed(
                &prefix(body, "sh", 1),
                &request(10),
                &RelaxPlan::new(&RelaxationConfig::default(), &RelaxOptions::disabled()).unwrap(),
                &exec(),
            )
            .unwrap();
        let scores = |out: &super::super::RelaxedResults| -> Vec<(String, u64)> {
            out.hits
                .iter()
                .map(|h| (h.hit.document.id().to_owned(), h.hit.score.to_bits()))
                .collect()
        };
        assert_eq!(scores(&out), scores(&plain));
        // 50 left: nothing to cut.
        let relax = relax.with_limits(QueryLimits {
            max_expansions: 100,
            ..QueryLimits::default()
        });
        let out = views
            .search_relaxed(&prefix(body, "pur", 50), &request(10), &relax, &exec())
            .unwrap();
        assert_eq!(out.relaxation.applied, ["prefix"]);
        // None left: skipped.
        let relax = relax.with_limits(QueryLimits {
            max_expansions: 50,
            ..QueryLimits::default()
        });
        let out = views
            .search_relaxed(&prefix(body, "pur", 50), &request(10), &relax, &exec())
            .unwrap();
        assert_eq!(out.relaxation.applied, ["prefix:skipped"]);
    }

    #[test]
    fn a_cascade_whose_steps_were_all_skipped_did_not_relax() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let drop = Stub::new(vec![step(any_of(body, &["red", "blue"]), "drop_terms:50%")]);
        let relax = plan(&min(1), &[(RelaxLevel::DropTerms, &drop)]).with_limits(terms(1));
        let out = views
            .search_relaxed(&term(body, "purple"), &request(10), &relax, &exec())
            .unwrap();
        let r = &out.relaxation;
        assert_eq!(r.applied, ["drop_terms:50%:skipped"]);
        assert_eq!(r.level, RelaxLevel::Exact);
        assert!(!r.relaxed());
        assert_eq!(out.hits, []);
    }

    #[test]
    fn an_invalid_step_is_an_error_not_a_skip() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let bad = Stub::new(vec![step(prefix(body, "", 5), "prefix")]);
        let relax = plan(&min(1), &[(RelaxLevel::Prefix, &bad)]).with_limits(terms(1));
        let err = views
            .search_relaxed(&term(body, "purple"), &request(10), &relax, &exec())
            .unwrap_err();
        assert!(
            matches!(
                err,
                super::super::RelaxedSearchError::Plan(SearchError::Query(
                    QueryError::EmptyPrefix { .. }
                ))
            ),
            "{err:?}"
        );
    }

    #[test]
    fn cursor_pages_skip_the_same_steps() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let drop = Stub::new(vec![
            step(any_of(body, &["red", "shoes", "hat", "blue"]), "wide"),
            step(term(body, "shoes"), "shoes"),
            step(any_of(body, &["red", "blue"]), "colors"),
        ]);
        let relax = plan(
            &RelaxOptions {
                fill: Some(true),
                ..min(1)
            },
            &[(RelaxLevel::DropTerms, &drop)],
        )
        .with_limits(terms(4));
        let purple = term(body, "purple");
        let paged = by_cursor(&views, &purple, &request(1), &relax);
        let whole = views
            .search_relaxed(&purple, &request(10), &relax, &exec())
            .unwrap();
        assert_eq!(
            whole.relaxation.applied,
            ["wide:skipped", "shoes", "colors"]
        );
        assert_eq!(strip(&paged), strip(&whole.hits));
        // `shoes` first, then the colors it did not match; the skipped step holds none.
        let steps: Vec<usize> = whole.hits.iter().map(|h| h.step).collect();
        assert_eq!(steps, [2, 2, 3, 3]);
    }

    #[test]
    fn a_cut_step_pages_by_cursor_and_is_excluded_as_cut() {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let levels = Stub::new(vec![
            step(prefix(body, "sh", 50), "prefix"),
            step(any_of(body, &["red", "blue"]), "colors"),
        ]);
        // One expansion: the prefix is cut to it. Excluded uncut, its 50 would leave `colors` no room.
        let limits = QueryLimits {
            max_terms: 4,
            max_expansions: 1,
            ..QueryLimits::default()
        };
        let relax = plan(
            &RelaxOptions {
                fill: Some(true),
                ..min(1)
            },
            &[(RelaxLevel::Prefix, &levels)],
        )
        .with_limits(limits);
        let purple = term(body, "purple");
        let whole = views
            .search_relaxed(&purple, &request(10), &relax, &exec())
            .unwrap();
        assert_eq!(whole.relaxation.applied, ["prefix:cut", "colors"]);
        assert_eq!(sorted(&whole), [("a", 1), ("b", 2), ("c", 1), ("e", 2)]);
        let paged = by_cursor(&views, &purple, &request(1), &relax);
        assert_eq!(strip(&paged), strip(&whole.hits));
    }

    #[test]
    fn a_skipped_step_counts_no_facet() {
        let (db, body, tag) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let drop = Stub::new(vec![
            step(any_of(body, &["red", "shoes", "hat", "blue"]), "wide"),
            step(term(body, "hat"), "hat"),
        ]);
        let relax = plan(&min(1), &[(RelaxLevel::DropTerms, &drop)]).with_limits(terms(3));
        let request = SearchRequest {
            facets: vec![FacetRequest::new(tag, 10, c.schema()).unwrap()],
            ..request(10)
        };
        let out = views
            .search_relaxed(&term(body, "purple"), &request, &relax, &exec())
            .unwrap();
        // `hat`: b and d (y), e (x). The wide step would have counted a and c too.
        let counts: Vec<(&[u8], u64)> = out.facets[0]
            .values
            .iter()
            .map(|v| (v.value.as_slice(), v.count))
            .collect();
        assert_eq!(counts, [(&b"y"[..], 2), (&b"x"[..], 1)]);
    }

    #[test]
    fn a_skipped_last_step_leaves_the_level_and_text_of_the_step_before() {
        use persia_analysis::AnalyzerRegistry;

        use crate::query::{DefaultFields, QueryCompiler};

        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let compiler = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin()).unwrap();
        let fields = DefaultFields::all(c.schema());
        let text = |query: Query, applied: &str, text: &str| Step {
            effective: Some(text.into()),
            ..step(query, applied)
        };
        let drop = Stub::new(vec![text(term(body, "shoes"), "shoes", "shoes")]);
        let wide = Stub::new(vec![text(
            any_of(body, &["red", "shoes", "hat", "blue"]),
            "wide",
            "red shoes hat blue",
        )]);
        let relax = plan(
            &min(3),
            &[(RelaxLevel::DropTerms, &drop), (RelaxLevel::Fuzzy, &wide)],
        );
        // The compilation's own limits apply: the plan sets none.
        let tight = terms(3);
        let purple = compiler.compile("purple", &fields, &tight);
        assert_eq!(purple.limits, tight);
        let out = views
            .search_relaxed(&purple, &request(10), &relax, &exec())
            .unwrap();
        let r = &out.relaxation;
        assert_eq!(r.applied, ["shoes", "wide:skipped"]);
        assert_eq!(r.level, RelaxLevel::DropTerms);
        assert!(r.relaxed());
        assert_eq!(r.effective.as_deref(), Some("shoes"));
        // The plan's limits win over the compilation's.
        let relax = relax.with_limits(QueryLimits::default());
        let out = views
            .search_relaxed(&purple, &request(10), &relax, &exec())
            .unwrap();
        assert_eq!(out.relaxation.applied, ["shoes", "wide"]);
        assert_eq!(out.relaxation.level, RelaxLevel::Fuzzy);
        assert_eq!(
            out.relaxation.effective.as_deref(),
            Some("red shoes hat blue")
        );
    }
}

/// A level that cannot read a statistic fails the search (ROADMAP 8.2.2.2: L2 reads its terms' `df`), as a step that
/// cannot be planned does.
#[test]
fn a_level_that_cannot_read_its_statistics_fails_the_search() {
    #[derive(Debug)]
    struct Unreadable;
    impl LevelTransform for Unreadable {
        fn steps(&self, _input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
            Err(ReadError::UnknownField {
                field: FieldId::new(99),
            })
        }
    }
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let relax = plan(&min(1), &[]).with_level(RelaxLevel::DropTerms, Arc::new(Unreadable));
    let out = views.search_relaxed(&term(body, "purple"), &request(10), &relax, &exec());
    assert!(
        matches!(
            out,
            Err(super::RelaxedSearchError::Plan(SearchError::Read(
                ReadError::UnknownField { .. }
            )))
        ),
        "{out:?}"
    );
    // A query with enough hits at L0 never builds the level.
    let out = views.search_relaxed(&term(body, "red"), &request(10), &relax, &exec());
    assert_eq!(out.unwrap().hits.len(), 2);
}

/// The request's deadline stops a level before it builds its steps (SPEC §7.3, §7.6), as an error: unlike the budget,
/// a deadline is not a truncated response.
#[test]
fn a_deadline_passed_while_a_level_builds_stops_the_next_one_before_it_reads() {
    /// Builds no step, and takes 10 ms of the deadline's clock to do so.
    #[derive(Debug)]
    struct Slow(Arc<ManualClock>);
    impl LevelTransform for Slow {
        fn steps(&self, _input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
            self.0.advance(Duration::from_millis(10));
            Ok(Vec::new())
        }
    }
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let clock = Arc::new(ManualClock::new(0));
    let drop_terms = Stub::new(vec![step(term(body, "hat"), "drop_terms:75%")]);
    let relax = plan(&min(1), &[(RelaxLevel::DropTerms, &drop_terms)])
        .with_level(RelaxLevel::Synonyms, Arc::new(Slow(Arc::clone(&clock))));
    let at = crate::db::Clock::now(&*clock) + Duration::from_millis(5);
    let exec = exec().deadline(Arc::clone(&clock) as _, at);
    let out = views.search_relaxed(&term(body, "purple"), &request(10), &relax, &exec);
    assert!(
        matches!(
            out,
            Err(super::RelaxedSearchError::Plan(SearchError::Interrupted(
                Interrupt::DeadlineExceeded
            )))
        ),
        "{out:?}"
    );
    assert_eq!(drop_terms.calls(), 0);
}

mod nested {
    //! A nested level excludes only its latest searched step (ROADMAP 8.2.2.4): L2's ladder, after a stub L1 step that no L2
    //! step holds, over random corpora and programmatic queries, with prefixes the limits may cut.
    //!
    //! Invariants:
    //! 1. Within limits nothing reaches, the cascade gives the same hits, steps, scores (bit for bit) and relaxation
    //!    as one where every step excludes every earlier step: excluding the latest of a nested level excludes them all.
    //! 2. Under tight limits, where steps are cut or skipped, no document is a hit of two steps: a cut step does not
    //!    stand for its level's earlier steps.
    //! 3. A step of a nested level never excludes more than the old rule did, so it is cut or skipped no more often.
    //! 4. Pages by cursor under tight limits are slices of the one ranking, and state the same cascade without `fill`.

    use std::collections::BTreeSet;
    use std::ops::Bound;

    use proptest::prelude::*;

    use super::*;
    use crate::query::{BoolQuery, Filter, FilterValue, PrefixQuery, QueryLimits};

    const WORDS: [&str; 6] = ["a0", "a1", "a2", "b0", "b1", "b2"];

    #[derive(Debug, Clone)]
    enum Clause {
        Word(usize),
        /// `a` or `b`, with this `max_expansions`.
        Prefix(bool, u32),
    }

    #[derive(Debug, Clone)]
    struct Case {
        docs: Vec<(Vec<usize>, i64)>,
        commits: Vec<usize>,
        must: Vec<Clause>,
        must_not: Option<usize>,
        /// An L1 step searching this word: not nested, and not inside any L2 step, so it stays excluded from them.
        l1: Option<usize>,
        filter: bool,
        min_results: u32,
        fill: bool,
        /// `max_expansions` and `max_terms` of the tight limits.
        tight: (usize, usize),
    }

    fn clause() -> impl Strategy<Value = Clause> {
        prop_oneof![
            (0..WORDS.len()).prop_map(Clause::Word),
            (any::<bool>(), 1u32..4).prop_map(|(a, n)| Clause::Prefix(a, n)),
        ]
    }

    fn case() -> impl Strategy<Value = Case> {
        (
            prop::collection::vec(
                (prop::collection::vec(0..WORDS.len(), 1..5), -2i64..3),
                1..30,
            ),
            prop::collection::vec(0usize..30, 0..3),
            prop::collection::vec(clause(), 2..7),
            prop::option::of(0..WORDS.len()),
            prop::option::of(0..WORDS.len()),
            any::<bool>(),
            1u32..40,
            any::<bool>(),
            (1usize..16, 8usize..40),
        )
            .prop_map(
                |(docs, commits, must, must_not, l1, filter, min_results, fill, tight)| Case {
                    docs,
                    commits,
                    must,
                    must_not,
                    l1,
                    filter,
                    min_results,
                    fill,
                    tight,
                },
            )
    }

    fn build(case: &Case) -> (Database<SimFile>, FieldId, FieldId) {
        let schema = SchemaBuilder::new("c")
            .field("body", FieldKind::Text(TextOptions::default()))
            .field("n", FieldKind::I64 { stored: false })
            .build()
            .unwrap();
        let body = schema.field("body").unwrap().id();
        let n = schema.field("n").unwrap().id();
        let mut db =
            Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default()).unwrap();
        db.create_collection(schema).unwrap();
        for (i, (words, value)) in case.docs.iter().enumerate() {
            let text: Vec<&str> = words.iter().map(|&w| WORDS[w]).collect();
            let doc = Document::new(format!("d{i}"))
                .with(body, Value::String(text.join(" ")))
                .with(n, Value::I64(*value));
            db.put("c", &doc, Durability::Buffered).unwrap();
            if case.commits.contains(&i) {
                db.commit().unwrap();
            }
        }
        (db, body, n)
    }

    /// L0: the case's `must` clauses, its excluded word and its filter (`n ≥ 0`).
    fn query(case: &Case, body: FieldId, n: FieldId) -> Query {
        let must = case
            .must
            .iter()
            .map(|c| match *c {
                Clause::Word(w) => term(body, WORDS[w]),
                Clause::Prefix(a, max_expansions) => Query::Prefix(PrefixQuery {
                    field: body,
                    prefix: if a { "a" } else { "b" }.into(),
                    max_expansions,
                }),
            })
            .collect();
        Query::Bool(BoolQuery {
            must,
            must_not: case
                .must_not
                .map(|w| term(body, WORDS[w]))
                .into_iter()
                .collect(),
            filter: if case.filter {
                vec![Filter::Range {
                    field: n,
                    lower: Bound::Included(FilterValue::I64(0)),
                    upper: Bound::Unbounded,
                }]
            } else {
                Vec::new()
            },
            ..BoolQuery::default()
        })
    }

    /// The collection's levels, each step held to `limits`: L2 the real one, L1 the case's stub step (a programmatic query
    /// builds none of its own).
    fn plans(case: &Case, body: FieldId, limits: QueryLimits) -> (RelaxPlan, RelaxPlan) {
        let options = RelaxOptions {
            min_results: Some(case.min_results),
            fill: Some(case.fill),
            ..RelaxOptions::default()
        };
        let mut latest = RelaxPlan::new(&RelaxationConfig::default(), &options)
            .unwrap()
            .with_limits(limits);
        if let Some(w) = case.l1 {
            let stub = Stub::new(vec![step(term(body, WORDS[w]), "synonyms")]);
            latest = latest.with_level(RelaxLevel::Synonyms, stub);
        }
        let every = latest.clone().excluding_every_step();
        (latest, every)
    }

    fn check(case: &Case) -> Result<(), TestCaseError> {
        let (db, body, n) = build(case);
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let l0 = query(case, body, n);
        let all = request(1000);

        // 1. Within limits nothing reaches, the same cascade.
        let (latest, every) = plans(case, body, QueryLimits::default());
        let got = views.search_relaxed(&l0, &all, &latest, &exec()).unwrap();
        let want = views.search_relaxed(&l0, &all, &every, &exec()).unwrap();
        prop_assert_eq!(strip(&got.hits), strip(&want.hits));
        prop_assert_eq!(&got.relaxation, &want.relaxation);

        // 2. and 3. Under tight limits, each document once, and no step skipped that the old rule searched.
        let (max_expansions, max_terms) = case.tight;
        let tight = QueryLimits {
            max_terms,
            max_expansions,
            ..QueryLimits::default()
        };
        let (latest, every) = plans(case, body, tight);
        let got = views.search_relaxed(&l0, &all, &latest, &exec()).unwrap();
        let mut seen = BTreeSet::new();
        for hit in &got.hits {
            prop_assert!(
                seen.insert(hit.hit.document.id()),
                "{} twice: {:?}",
                hit.hit.document.id(),
                got.relaxation.applied
            );
        }
        let old = views.search_relaxed(&l0, &all, &every, &exec()).unwrap();
        // Both build the same steps in order, until one has its hits: per step, the old rule's exclusions hold the new
        // rule's, so a step it cuts or skips was cut or skipped by the old rule too.
        for (new, was) in got.relaxation.applied.iter().zip(&old.relaxation.applied) {
            let held = |label: &str| label.ends_with(":skipped") || label.ends_with(":cut");
            prop_assert!(
                !new.ends_with(":skipped") || was.ends_with(":skipped"),
                "{:?} against {:?}",
                got.relaxation.applied,
                old.relaxation.applied
            );
            prop_assert!(
                !new.ends_with(":cut") || held(was),
                "{:?} against {:?}",
                got.relaxation.applied,
                old.relaxation.applied
            );
        }
        // A page after a cursor builds the same steps again without searching those before it: the pages by cursor are
        // the one ranking, and, without `fill`, each states the same cascade.
        let page = request(3);
        let paged = by_cursor(&views, &l0, &page, &latest);
        prop_assert_eq!(strip(&paged), strip(&got.hits));
        if !case.fill {
            let first = views.search_relaxed(&l0, &page, &latest, &exec()).unwrap();
            if let Some(last) = first.hits.last() {
                let next = SearchRequest {
                    search_after: Some(last.cursor()),
                    ..page
                };
                let next = views.search_relaxed(&l0, &next, &latest, &exec()).unwrap();
                prop_assert_eq!(&next.relaxation, &first.relaxation);
            }
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 1024 } else { 128 }))]

        #[test]
        fn excluding_the_latest_nested_step_excludes_them_all(case in case()) {
            check(&case)?;
        }
    }
}

/// SPEC §6.3.1, §7.6 L3 (ROADMAP 8.2.3): a field whose analyzer stacks tokens (`edge_ngram`) already holds each word's
/// prefixes, so L3 leaves its terms exact. No schema names such an analyzer yet (built-in ones only, none stacking),
/// so the collection's memtable is given one.
#[test]
fn l3_does_not_expand_a_field_whose_analyzer_stacks_tokens() {
    use std::num::NonZeroUsize;

    use persia_analysis::{Analyzer, AnalyzerRegistry, EdgeNgramFilter, StandardTokenizer};

    use crate::memtable::MemField;
    use crate::query::{DefaultFields, QueryCompiler, QueryLimits};

    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let compiled = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(
            "shoesx",
            &DefaultFields::all(c.schema()),
            &QueryLimits::default(),
        );
    let options = RelaxOptions {
        levels: Some(Levels::Custom(vec![crate::schema::Level::Prefix])),
        ..min(1)
    };
    let relax = RelaxPlan::new(&RelaxationConfig::default(), &options).unwrap();
    let applied = || {
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        views
            .search_relaxed(&compiled, &request(10), &relax, &exec())
            .unwrap()
            .relaxation
            .applied
    };
    // `body` is `standard`: `shoesx` is expanded, to no term.
    assert_eq!(applied(), ["prefix"]);
    let n = |n| NonZeroUsize::new(n).unwrap();
    let grams = Analyzer::builder(StandardTokenizer)
        .filter(EdgeNgramFilter::new(n(2), n(4)).unwrap())
        .build();
    {
        let mut inner = c.memtable.shared().write().unwrap();
        let field = usize::try_from(body.get()).unwrap();
        let MemField::Text { analyzer, .. } = &mut inner.fields[field] else {
            panic!("body is text")
        };
        *analyzer = Arc::new(grams);
    }
    // Stacking now: no field to expand, no step.
    assert_eq!(applied(), Vec::<String>::new());
}
