//! A query's `relax` over the collection's settings, through the cascade with the engine's levels (ROADMAP 8.3.1,
//! ADR-0045): each setting a query gives overrides the collection's, `relax: false` runs L0 alone, a query that
//! restates the collection's settings is the collection's search, and a cursor pages only under its own settings.

use std::sync::Arc;
use std::time::Duration;

use persia_storage::container::sim::SimFile;
use proptest::prelude::*;

use super::{exec, ids, request, step, strip, term};
use crate::db::relaxed::{RelaxedResults, RelaxedSearchError};
use crate::db::{
    CollectionViews, Database, DatabaseOptions, Execution, ManualClock, SearchRequest,
};
use crate::document::{Document, Value};
use crate::query::{BoolQuery, Query};
use crate::relax::{LevelInput, LevelTransform, RelaxLevel, RelaxOptions, RelaxPlan, Step};
use crate::schema::{
    Fallback, FieldId, FieldKind, FilterRelaxation, Level, Levels, RelaxationConfig, SchemaBuilder,
    TextOptions,
};
use crate::search::{Pruning, RequestError};
use crate::segment::ReadError;
use crate::wal::Durability;

const DOCS: [(&str, &str); 5] = [
    ("a", "red shoes"),
    ("b", "red hat hat"),
    ("c", "blue shoes"),
    ("d", "green hat"),
    ("e", "blue hat and more words"),
];

/// [`DOCS`] over two segments and the memtable, in a collection whose relaxation defaults are `relaxation`.
fn database(relaxation: RelaxationConfig) -> (Database<SimFile>, FieldId) {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .relaxation(relaxation)
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    for (i, (id, text)) in DOCS.into_iter().enumerate() {
        let doc = Document::new(id).with(body, Value::String(text.into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i == 1 || i == 3 {
            db.commit().unwrap();
        }
    }
    (db, body)
}

fn all_of(body: FieldId, words: &[&str]) -> Query {
    Query::Bool(BoolQuery {
        must: words.iter().map(|w| term(body, w)).collect(),
        ..BoolQuery::default()
    })
}

fn options(json: &str) -> RelaxOptions {
    RelaxOptions::from_json(json).unwrap()
}

/// `query` searched over `views` under `json`'s `relax` on the collection's settings.
fn run(
    views: &CollectionViews<'_>,
    query: &Query,
    page: &SearchRequest,
    json: &str,
) -> RelaxedResults {
    let relax = RelaxPlan::new(views.collection().schema().relaxation(), &options(json)).unwrap();
    views.search_relaxed(query, page, &relax, &exec()).unwrap()
}

fn levels(out: &RelaxedResults) -> Vec<RelaxLevel> {
    out.hits.iter().map(|h| h.level).collect()
}

#[test]
fn each_setting_a_query_gives_overrides_the_collections() {
    let (db, body) = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let red_shoes = all_of(body, &["red", "shoes"]);

    // The collection's `min_results` 1: L0's `a` is enough.
    let out = run(&views, &red_shoes, &request(10), "{}");
    assert_eq!(ids(&out), [("a", 0)]);
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    // `min_results` 3: L0's one hit is too few; L2's one-term rung adds `b` and `c`, and the cascade stops there.
    let out = run(&views, &red_shoes, &request(10), r#"{"min_results": 3}"#);
    assert_eq!(
        levels(&out),
        [
            RelaxLevel::Exact,
            RelaxLevel::DropTerms,
            RelaxLevel::DropTerms
        ]
    );
    assert_eq!(out.relaxation.applied, ["drop_terms:75%"]);
    // `levels` replaces the collection's: no L2, so L3 (its tier step too, ROADMAP 8.2.7) and L4 build and add nothing,
    // and L5 ranks every other document.
    let out = run(
        &views,
        &red_shoes,
        &request(10),
        r#"{"min_results": 3, "levels": ["prefix", "fuzzy"]}"#,
    );
    assert_eq!(
        out.relaxation.applied,
        ["prefix:last", "prefix", "fuzzy", "fallback"]
    );
    assert_eq!(out.hits.len(), 5);
    assert!(
        out.hits[1..]
            .iter()
            .all(|h| h.level == RelaxLevel::Fallback)
    );
    // `fallback: "none"` with no level: L0 alone builds, and the page may stay short.
    let out = run(
        &views,
        &red_shoes,
        &request(10),
        r#"{"min_results": 3, "levels": [], "fallback": "none"}"#,
    );
    assert_eq!(ids(&out), [("a", 0)]);
    assert!(!out.relaxation.relaxed());
    // `fill`: past `min_results` 1, every level adds hits until the page is full.
    let out = run(&views, &red_shoes, &request(4), r#"{"fill": true}"#);
    assert_eq!(
        levels(&out),
        [
            RelaxLevel::Exact,
            RelaxLevel::DropTerms,
            RelaxLevel::DropTerms,
            RelaxLevel::Fallback
        ]
    );
    // `filters: "last_resort"` is accepted (SPEC §6.2.3); no level relaxes a filter until ROADMAP 8.2.6.
    let last_resort = run(
        &views,
        &red_shoes,
        &request(10),
        r#"{"min_results": 3, "filters": "last_resort"}"#,
    );
    let never = run(&views, &red_shoes, &request(10), r#"{"min_results": 3}"#);
    assert_eq!(strip(&last_resort.hits), strip(&never.hits));
    assert_eq!(last_resort.relaxation, never.relaxation);
}

#[test]
fn a_query_turns_on_a_cascade_the_collection_turned_off() {
    let (db, body) = database(RelaxationConfig {
        enabled: false,
        min_results: 3,
        ..RelaxationConfig::default()
    });
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let red_shoes = all_of(body, &["red", "shoes"]);
    assert_eq!(
        ids(&run(&views, &red_shoes, &request(10), "{}")),
        [("a", 0)]
    );
    // `true` relaxes with the collection's other settings: its `min_results` 3.
    let out = run(&views, &red_shoes, &request(10), "true");
    assert_eq!(out.hits.len(), 3);
    assert_eq!(out.relaxation.level, RelaxLevel::DropTerms);
}

#[test]
fn relax_false_returns_l0_alone_states_exact_and_may_be_empty() {
    let (db, body) = database(RelaxationConfig {
        min_results: 5,
        fill: true,
        ..RelaxationConfig::default()
    });
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    for json in [
        "false",
        r#"{"enabled": false}"#,
        r#"{"enabled": false, "min_results": 9, "fill": false}"#,
    ] {
        let out = run(&views, &term(body, "purple"), &request(10), json);
        assert!(out.hits.is_empty(), "{json}");
        let r = &out.relaxation;
        assert_eq!(r.level, RelaxLevel::Exact);
        assert!(r.applied.is_empty() && !r.relaxed() && !r.truncated && r.fallback.is_none());
        let out = run(&views, &all_of(body, &["red", "shoes"]), &request(10), json);
        assert_eq!(ids(&out), [("a", 0)]);
        // Its cursors record no settings, as a plain search's, so they page a plain search.
        assert_eq!(out.hits[0].cursor().relax, None);
        let plain = crate::search::SearchPlan::with_terms(
            &all_of(body, &["red", "shoes"]),
            c.schema(),
            &c.search_statistics(&memtable),
            &views,
        )
        .unwrap();
        let after = SearchRequest {
            search_after: Some(out.hits[0].cursor()),
            ..request(10)
        };
        let rest = views.search_with(&plain, &after, &exec()).unwrap();
        assert_eq!(rest.hits.len(), 0);
    }
    // A value out of range is an error also when the query turns the cascade off.
    assert!(matches!(
        RelaxOptions::from_json(r#"{"enabled": false, "min_results": 0}"#),
        Err(crate::relax::RelaxError::OutOfRange {
            option: "min_results",
            ..
        })
    ));
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
fn a_query_budget_overrides_the_collections() {
    let (db, body) = database(RelaxationConfig {
        budget_ms: 50,
        ..RelaxationConfig::default()
    });
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let clock = Arc::new(ManualClock::new(0));
    let exec = Execution::new(Pruning::default())
        .threads(crate::db::Threads::Current)
        .budget_clock(Arc::clone(&clock) as _);
    let slow = Arc::new(Slow {
        step: step(term(body, "hat"), "synonyms"),
        clock: Arc::clone(&clock),
        by: Duration::from_millis(100),
    });
    let search = |json: &str| {
        let relax = RelaxPlan::new(c.schema().relaxation(), &options(json))
            .unwrap()
            .with_level(RelaxLevel::Synonyms, Arc::clone(&slow) as _);
        views
            .search_relaxed(&term(body, "purple"), &request(10), &relax, &exec)
            .unwrap()
    };
    // The collection's 50 ms pass while L1 builds: the cascade stops before its step.
    let out = search("{}");
    assert!(out.relaxation.truncated && out.hits.is_empty());
    // A query's 500 ms do not: L1's step finds the three `hat` documents.
    let out = search(r#"{"budget_ms": 500}"#);
    assert!(!out.relaxation.truncated);
    assert_eq!(out.hits.len(), 3);
    assert!(out.hits.iter().all(|h| h.level == RelaxLevel::Synonyms));
}

#[test]
fn a_cursor_pages_only_under_the_settings_that_made_it() {
    let (db, body) = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let red_shoes = all_of(body, &["red", "shoes"]);
    let made = r#"{"min_results": 3}"#;
    let first = run(&views, &red_shoes, &request(2), made);
    let after = SearchRequest {
        search_after: Some(first.hits[1].cursor()),
        ..request(2)
    };
    // The same settings, and another budget (it only truncates a page), page on.
    let rest = run(&views, &red_shoes, &after, made);
    let all = run(&views, &red_shoes, &request(10), made);
    assert_eq!(ids(&rest), ids(&all)[2..]);
    assert_eq!(
        run(
            &views,
            &red_shoes,
            &after,
            r#"{"min_results": 3, "budget_ms": 900}"#
        )
        .hits,
        rest.hits
    );
    // Any setting that decides the steps or where they stop is an error, the collection's defaults included.
    for json in [
        "{}",
        r#"{"min_results": 4}"#,
        r#"{"min_results": 3, "fill": true}"#,
        r#"{"min_results": 3, "fallback": "none"}"#,
        r#"{"min_results": 3, "levels": ["drop_terms"]}"#,
        r#"{"min_results": 3, "filters": "last_resort"}"#,
    ] {
        let relax = RelaxPlan::new(c.schema().relaxation(), &options(json)).unwrap();
        let error = views
            .search_relaxed(&red_shoes, &after, &relax, &exec())
            .map(|_| ());
        assert!(
            matches!(
                &error,
                Err(RelaxedSearchError::Request(RequestError::CursorRelax {
                    cursor: Some(_),
                    search: Some(_)
                }))
            ),
            "{json}: {error:?}"
        );
        let message = error.unwrap_err().to_string();
        assert!(message.contains("min_results 3"), "{message}");
    }
    // `relax: false` rejects the cursor of a later step as a plain search does, and the step-0 cursor for its settings.
    let off = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::disabled()).unwrap();
    assert!(matches!(
        views.search_relaxed(&red_shoes, &after, &off, &exec()),
        Err(RelaxedSearchError::Request(RequestError::CursorStep {
            step: 1,
            ..
        }))
    ));
    let l0 = SearchRequest {
        search_after: Some(first.hits[0].cursor()),
        ..request(2)
    };
    assert!(matches!(
        views.search_relaxed(&red_shoes, &l0, &off, &exec()),
        Err(RelaxedSearchError::Request(RequestError::CursorRelax {
            cursor: Some(_),
            search: None
        }))
    ));
}

/// A collection's relaxation settings, each from a small range.
fn relaxation() -> impl Strategy<Value = RelaxationConfig> {
    (
        any::<bool>(),
        1u32..=6,
        prop::option::of(prop::collection::vec(any::<bool>(), 4)),
        any::<bool>(),
        any::<bool>(),
        1u32..=1_000,
    )
        .prop_map(
            |(enabled, min_results, levels, fallback, fill, budget_ms)| RelaxationConfig {
                enabled,
                min_results,
                levels: levels.map_or(Levels::Default, |keep| {
                    Levels::Custom(
                        Level::ALL
                            .into_iter()
                            .zip(keep)
                            .filter_map(|(l, k)| k.then_some(l))
                            .collect(),
                    )
                }),
                fallback: if fallback {
                    Fallback::StaticRank
                } else {
                    Fallback::None
                },
                fill,
                filters: FilterRelaxation::Never,
                budget_ms,
            },
        )
}

/// The collection's settings, every one restated by the query.
fn restated(r: &RelaxationConfig) -> RelaxOptions {
    RelaxOptions {
        enabled: Some(r.enabled),
        min_results: Some(r.min_results),
        levels: Some(r.levels.clone()),
        fallback: Some(r.fallback),
        fill: Some(r.fill),
        filters: Some(r.filters),
        budget_ms: Some(r.budget_ms),
    }
}

const WORDS: [&str; 7] = ["red", "shoes", "hat", "blue", "shoez", "sho", "purple"];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// A query whose `relax` restates the collection's settings searches as one that gives none: the same hits,
    /// facets, relaxation and cursors, on every page by offset and by cursor, and each one's cursors page the other.
    #[test]
    fn a_query_restating_the_collections_settings_searches_as_one_that_gives_none(
        collection in relaxation(),
        words in prop::collection::vec(0..WORDS.len(), 1..=3),
        limit in 1usize..=4,
        offset in 0usize..=2,
    ) {
        let (db, body) = database(collection.clone());
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let query = all_of(body, &words.iter().map(|&w| WORDS[w]).collect::<Vec<_>>());
        let given = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::default()).unwrap();
        let same = RelaxPlan::new(c.schema().relaxation(), &restated(&collection)).unwrap();
        prop_assert_eq!(given.settings(), same.settings());
        let page = SearchRequest { offset, ..request(limit) };
        let a = views.search_relaxed(&query, &page, &given, &exec()).unwrap();
        let b = views.search_relaxed(&query, &page, &same, &exec()).unwrap();
        prop_assert_eq!(&a, &b);
        // Page on by cursor, alternating the two plans.
        let mut after = a.hits.last().map(crate::db::RelaxedHit::cursor);
        let mut pages = 0;
        while let Some(cursor) = after {
            let plan = if pages % 2 == 0 { &same } else { &given };
            let other = if pages % 2 == 0 { &given } else { &same };
            let next = SearchRequest { search_after: Some(cursor), ..request(limit) };
            let x = views.search_relaxed(&query, &next, plan, &exec()).unwrap();
            let y = views.search_relaxed(&query, &next, other, &exec()).unwrap();
            prop_assert_eq!(&x, &y);
            after = x.hits.last().map(crate::db::RelaxedHit::cursor);
            pages += 1;
            prop_assert!(pages <= DOCS.len());
        }
    }
}
