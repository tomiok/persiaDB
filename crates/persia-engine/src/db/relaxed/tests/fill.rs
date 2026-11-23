//! `fill` as a request option, end to end (ROADMAP 8.3.2, SPEC §7.6): through a query's `relax` over the engine's own
//! levels, pages by cursor and by offset are slices of one ranking; the budget truncates a filling page and the next
//! one tries again, while the request's deadline stays an error; and `fill` reaches L5. Facets under `fill` are
//! `tests/facets.rs`'s (ROADMAP 8.1.9: every page counts the same).

use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;

use super::{by_cursor, database, exec, ids, request, step, strip, term};
use crate::db::relaxed::{RelaxedResults, RelaxedSearchError};
use crate::db::{Clock, CollectionViews, ManualClock, SearchRequest};
use crate::query::{BoolQuery, Query};
use crate::relax::{
    FallbackOrder, LevelInput, LevelTransform, RelaxLevel, RelaxOptions, RelaxPlan, Step,
};
use crate::schema::{FieldId, Level, Levels};
use crate::search::{Interrupt, RequestError};
use crate::segment::ReadError;

fn all_of(body: FieldId, words: &[&str]) -> Query {
    Query::Bool(BoolQuery {
        must: words.iter().map(|w| term(body, w)).collect(),
        ..BoolQuery::default()
    })
}

/// The collection's defaults with `json`'s `relax` over them.
fn plan(views: &CollectionViews<'_>, json: &str) -> RelaxPlan {
    RelaxPlan::new(
        views.collection().schema().relaxation(),
        &RelaxOptions::from_json(json).unwrap(),
    )
    .unwrap()
}

fn search(
    views: &CollectionViews<'_>,
    query: &Query,
    page: &SearchRequest,
    relax: &RelaxPlan,
) -> RelaxedResults {
    views.search_relaxed(query, page, relax, &exec()).unwrap()
}

const WORDS: [&str; 8] = [
    "red", "shoes", "hat", "blue", "shoez", "sho", "purple", "words",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// With `fill`, the pages of one query by cursor, and by offset, are slices of the ranking one page of every hit
    /// gives, at every `min_results` and set of levels: each page builds the steps it needs, never other ones.
    #[test]
    fn fill_pages_by_cursor_and_by_offset_are_slices_of_one_ranking(
        words in prop::collection::vec(0..WORDS.len(), 1..=3),
        limit in 1usize..=3,
        min_results in 1u32..=4,
        levels in prop::option::of(prop::collection::vec(any::<bool>(), 4)),
    ) {
        let (db, body, _) = database();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let query = all_of(body, &words.iter().map(|&w| WORDS[w]).collect::<Vec<_>>());
        let options = RelaxOptions {
            fill: Some(true),
            min_results: Some(min_results),
            levels: levels.map(|keep| {
                Levels::Custom(Level::ALL.into_iter().zip(keep).filter_map(|(l, k)| k.then_some(l)).collect())
            }),
            ..RelaxOptions::default()
        };
        let relax = RelaxPlan::new(c.schema().relaxation(), &options).unwrap();
        let all = search(&views, &query, &request(10), &relax);
        // L5 is in every plan here and no filter is set: filling reaches every document.
        prop_assert_eq!(all.hits.len(), 5);
        let paged = by_cursor(&views, &query, &request(limit), &relax);
        prop_assert_eq!(strip(&paged), strip(&all.hits));
        for offset in 0..5 {
            let page = SearchRequest { offset, ..request(limit) };
            let out = search(&views, &query, &page, &relax);
            let end = (offset + limit).min(5);
            prop_assert_eq!(strip(&out.hits), strip(&all.hits[offset..end]));
        }
    }
}

#[test]
fn fill_reaches_l5_and_a_deeper_page_states_the_steps_it_built() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let red_shoes = all_of(body, &["red", "shoes"]);
    let relax = plan(&views, r#"{"fill": true}"#);
    // L0 `a`; L2's one-term rung `b`, `c`; L3 and L4 add nothing; L5 ranks the rest by recency (no static rank).
    let all = search(&views, &red_shoes, &request(5), &relax);
    let levels: Vec<RelaxLevel> = all.hits.iter().map(|h| h.level).collect();
    assert_eq!(
        levels,
        [
            RelaxLevel::Exact,
            RelaxLevel::DropTerms,
            RelaxLevel::DropTerms,
            RelaxLevel::Fallback,
            RelaxLevel::Fallback
        ]
    );
    assert_eq!(
        all.relaxation.applied,
        ["drop_terms:75%", "prefix", "fuzzy", "fallback"]
    );
    assert_eq!(all.relaxation.fallback, Some(FallbackOrder::Recency));
    // Without `fill` the same query stops at L0, which has `min_results` 1.
    let plain = search(&views, &red_shoes, &request(5), &plan(&views, "{}"));
    assert_eq!(ids(&plain), [("a", 0)]);
    // By cursor, two hits a page: the first page stops at L2, the last reaches L5 and is flagged.
    let first = search(&views, &red_shoes, &request(2), &relax);
    assert_eq!(first.relaxation.level, RelaxLevel::DropTerms);
    assert_eq!(first.relaxation.fallback, None);
    let second = search(
        &views,
        &red_shoes,
        &SearchRequest {
            search_after: Some(first.hits[1].cursor()),
            ..request(2)
        },
        &relax,
    );
    assert_eq!(
        second.hits.iter().map(|h| h.level).collect::<Vec<_>>(),
        [RelaxLevel::DropTerms, RelaxLevel::Fallback]
    );
    assert_eq!(second.relaxation.level, RelaxLevel::Fallback);
    assert_eq!(second.relaxation.fallback, Some(FallbackOrder::Recency));
    assert_eq!(strip(&second.hits), strip(&all.hits[2..4]));
}

/// Builds one step and moves `clock` by `by` while it does.
#[derive(Debug)]
struct Slow {
    step: Step,
    clock: Arc<ManualClock>,
    by: Duration,
}

impl LevelTransform for Slow {
    fn steps(&self, _input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        self.clock.advance(self.by);
        Ok(vec![self.step.clone()])
    }
}

#[test]
fn the_budget_truncates_a_filling_page_and_the_next_one_tries_again() {
    let (db, body, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let clock = Arc::new(ManualClock::new(0));
    let exec = exec().budget_clock(Arc::clone(&clock) as _);
    let slow = Arc::new(Slow {
        step: step(term(body, "hat"), "synonyms"),
        clock: Arc::clone(&clock),
        by: Duration::from_millis(100),
    });
    let filling =
        |json: &str| plan(&views, json).with_level(RelaxLevel::Synonyms, Arc::clone(&slow) as _);
    let red = term(body, "red");
    // L0 has `a` and `b`, two of the four the page wants: `fill` builds L1, whose 100 ms pass the 50 ms budget.
    let short = filling(r#"{"fill": true, "budget_ms": 50}"#);
    let first = views
        .search_relaxed(&red, &request(4), &short, &exec)
        .unwrap();
    assert!(first.relaxation.truncated);
    assert_eq!(first.hits.len(), 2);
    assert!(first.hits.iter().all(|h| h.step == 0));
    // The next page, with a budget long enough (the budget is not one of the cursor's settings), builds L1 again and
    // fills from it: `hat` without `b`, which L0 took.
    let next = SearchRequest {
        search_after: Some(first.hits[1].cursor()),
        ..request(2)
    };
    let long = filling(r#"{"fill": true, "budget_ms": 500}"#);
    let out = views.search_relaxed(&red, &next, &long, &exec).unwrap();
    assert!(!out.relaxation.truncated);
    let mut found: Vec<&str> = out.hits.iter().map(|h| h.hit.document.id()).collect();
    found.sort_unstable();
    assert_eq!(found, ["d", "e"]);
    assert!(out.hits.iter().all(|h| h.level == RelaxLevel::Synonyms));
    // The request's own deadline passing while `fill` builds L1 is an error, not a truncation.
    let at = clock.now() + Duration::from_millis(20);
    let deadline = exec.clone().deadline(Arc::clone(&clock) as _, at);
    let err = views
        .search_relaxed(&red, &request(4), &long, &deadline)
        .unwrap_err();
    assert!(
        matches!(
            err,
            RelaxedSearchError::Request(RequestError::Interrupted(Interrupt::DeadlineExceeded))
                | RelaxedSearchError::Plan(crate::search::SearchError::Interrupted(
                    Interrupt::DeadlineExceeded
                ))
        ),
        "{err}"
    );
}
