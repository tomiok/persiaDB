//! The request's `"proximity"` (ROADMAP 10.1.7; SPEC §7.1.4, §7.2, §7.4; ADR-0036, ADR-0062).
//!
//! Invariants:
//! 1. `"proximity": true` plans the search with the proximity bonus (`SearchPlan::with_proximity`), `false` or a
//!    request without the key plans it without (`without_proximity`): the hits are those of the engine's own search
//!    under that plan, scores bit for bit.
//! 2. Only a search that does not relax takes the bonus: `"proximity": true` on a request whose settings relax (the
//!    default) is an error that names the fix, `"relax": false`.
//! 3. A cursor records whether its plan scored the bonus. A page after a cursor from a proximity-on search, asked with
//!    `"proximity": false`, is an error (`RequestError::CursorProximity`), and so is the reverse; with the same
//!    setting the pages are the slices of one ranking.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    missing_docs
)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_engine::db::{
    CollectionSnapshot, Database, DatabaseOptions, Execution, ManualClock, SearchResults,
    SegmentIds, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, QueryLimits};
use persia_engine::relax::RelaxOptions;
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldKind, RelaxationConfig, SchemaBuilder, TextOptions,
};
use persia_engine::search::request::{QueryRequest, QueryRequestError, QueryResults, RunOptions};
use persia_engine::search::{Explanation, Pruning, RequestError, SearchPlan};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use serde_json::{Value as J, json};

/// `body` with positions under `whitespace`, so a token's position is its index.
fn schema(relaxation: RelaxationConfig) -> CollectionSchema {
    let body = TextOptions {
        analyzer: AnalyzerRef::new("whitespace", 1).unwrap(),
        positions: true,
        ..TextOptions::default()
    };
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(body))
        .relaxation(relaxation)
        .build()
        .unwrap()
}

/// Every document holds `red` and `shoes` once among four tokens, so BM25F ties them all and only the distance
/// between the two words tells them apart: 3, 2, 1, 1, 2 and reversed (2).
const DOCS: [(&str, &str); 6] = [
    ("far", "red x x shoes"),
    ("near", "red x shoes x"),
    ("first", "red shoes x x"),
    ("second", "x red shoes x"),
    ("near2", "x red x shoes"),
    ("reversed", "shoes red x x"),
];

/// By the bonus, then by location: adjacent, then one apart (`reversed` costs one more than its distance), then two.
const BY_PROXIMITY: [&str; 6] = ["first", "second", "near", "near2", "reversed", "far"];

/// Without the bonus every score ties, and the hits rank by location: the order written.
const BY_LOCATION: [&str; 6] = ["far", "near", "first", "second", "near2", "reversed"];

fn database(relaxation: RelaxationConfig) -> Database<SimFile> {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(7),
        ..DatabaseOptions::default()
    };
    let schema = schema(relaxation);
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [7; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    for (i, (id, text)) in DOCS.into_iter().enumerate() {
        let doc = Document::new(id).with(body, Value::String(text.into()));
        db.put("c", &doc, Durability::Fsync).unwrap();
        // Half in a segment, half in the memtable.
        if i == 2 {
            db.commit().unwrap();
        }
    }
    db
}

fn options() -> RunOptions {
    RunOptions::new()
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

fn parse(c: &CollectionSnapshot, json: &J) -> Result<QueryRequest, QueryRequestError> {
    QueryRequest::from_json(&json.to_string(), c.schema(), &QueryLimits::default())
}

fn run(c: &CollectionSnapshot, json: &J) -> Result<QueryResults, QueryRequestError> {
    Ok(parse(c, json)?.run(c, &options())?.results)
}

fn plain(results: QueryResults) -> SearchResults {
    match results {
        QueryResults::Plain(r) => r,
        other => panic!("not a plain search: {other:?}"),
    }
}

fn ids(r: &SearchResults) -> Vec<&str> {
    r.hits.iter().map(|h| h.document.id()).collect()
}

/// The engine's own plain search of `red shoes`, planned with the bonus or without it.
fn by_hand(c: &CollectionSnapshot, request: &QueryRequest, proximity: bool) -> SearchResults {
    let exec = Execution::new(Pruning::default()).threads(Threads::Current);
    let compiled = c.compiler().compile(
        request.q(),
        &DefaultFields::all(c.schema()),
        &QueryLimits::default(),
    );
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let stats = c.search_statistics(&memtable);
    let plan =
        SearchPlan::with_terms(compiled.query(), c.schema(), &stats, &exec.terms(&views)).unwrap();
    let plan = if proximity {
        plan.with_proximity()
    } else {
        plan.without_proximity()
    };
    assert_eq!(plan.proximity(), proximity);
    views.search_with(&plan, request.page(), &exec).unwrap()
}

#[test]
fn true_plans_the_bonus_and_false_or_absent_plans_without_it() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let on = json!({"q": "red shoes", "relax": false, "proximity": true, "explain": true});
    let off = json!({"q": "red shoes", "relax": false, "proximity": false, "explain": true});
    let absent = json!({"q": "red shoes", "relax": false, "explain": true});

    let with = plain(run(c, &on).unwrap());
    assert_eq!(ids(&with), BY_PROXIMITY);
    assert_eq!(with, by_hand(c, &parse(c, &on).unwrap(), true));
    // The bonus is in the score, and in the explanation's root: a `bool` with a proximity node.
    let without = plain(run(c, &off).unwrap());
    assert_eq!(ids(&without), BY_LOCATION);
    assert_eq!(without, by_hand(c, &parse(c, &off).unwrap(), false));
    let score =
        |r: &SearchResults, id: &str| r.hits.iter().find(|h| h.document.id() == id).unwrap().score;
    assert!(score(&with, "first") > score(&without, "first"));
    assert!(score(&with, "first") > score(&with, "near"));
    assert!(score(&with, "near") > score(&with, "far"));
    assert_eq!(score(&without, "first"), score(&without, "far"));
    let bonus = |r: &SearchResults| match r.hits[0].explanation.as_ref().unwrap() {
        Explanation::Bool { proximity, .. } => proximity.is_some(),
        other => panic!("{other:?}"),
    };
    assert!(bonus(&with) && !bonus(&without));
    // A request that does not say is `false`.
    assert_eq!(plain(run(c, &absent).unwrap()), without);
    assert!(!parse(c, &absent).unwrap().proximity());
    assert!(parse(c, &on).unwrap().proximity());
    assert!(!parse(c, &off).unwrap().proximity());
}

#[test]
fn the_setter_is_the_json_key() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let built = QueryRequest::new("red shoes")
        .with_relax(RelaxOptions::disabled())
        .with_proximity(true);
    let json = json!({"q": "red shoes", "relax": false, "proximity": true});
    assert_eq!(parse(c, &json).unwrap(), built);
    assert_ne!(built, built.clone().with_proximity(false));
    assert_eq!(
        ids(&plain(built.run(c, &options()).unwrap().results)),
        BY_PROXIMITY
    );
    // `true` or `false`, nothing else.
    for bad in [
        json!(1),
        json!("true"),
        json!(null),
        json!([true]),
        json!({}),
    ] {
        let json = json!({"q": "red shoes", "relax": false, "proximity": bad});
        assert!(
            matches!(parse(c, &json), Err(QueryRequestError::Json(_))),
            "{json}"
        );
    }
    assert_eq!(
        parse(c, &json!({"proximity": 1})).unwrap_err().to_string(),
        "request: `proximity` takes true or false, found an integer"
    );
}

#[test]
fn proximity_on_a_search_that_relaxes_is_an_error_naming_the_fix() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    // The collection relaxes by default: without `"relax": false` the request relaxes.
    for relax in [None, Some(json!(true)), Some(json!({"min_results": 2}))] {
        let mut json = json!({"q": "red shoes", "proximity": true});
        if let Some(relax) = relax {
            json["relax"] = relax;
        }
        // It parses: whether it relaxes is the collection's settings too, known when it runs.
        let request = parse(c, &json).unwrap();
        let err = request.run(c, &options()).unwrap_err();
        assert!(
            matches!(err, QueryRequestError::ProximityRelaxed),
            "{json}: {err}"
        );
        assert_eq!(
            err.to_string(),
            "\"proximity\": true needs \"relax\": false: a search that relaxes does not score the proximity bonus"
        );
        assert_eq!(err.interrupt(), None);
        // `false` relaxes as ever.
        json["proximity"] = json!(false);
        assert!(
            matches!(run(c, &json), Ok(QueryResults::Relaxed(_))),
            "{json}"
        );
    }
    // Every way to run L0 alone takes it.
    for relax in [
        json!(false),
        json!({"enabled": false}),
        json!({"levels": [], "fallback": "none"}),
    ] {
        let json = json!({"q": "red shoes", "proximity": true, "relax": relax});
        assert_eq!(ids(&plain(run(c, &json).unwrap())), BY_PROXIMITY, "{json}");
    }
}

#[test]
fn a_collection_that_does_not_relax_takes_proximity_without_relax_false() {
    let disabled = RelaxationConfig {
        enabled: false,
        ..RelaxationConfig::default()
    };
    let db = database(disabled);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let json = json!({"q": "red shoes", "proximity": true});
    assert_eq!(ids(&plain(run(c, &json).unwrap())), BY_PROXIMITY);
    // Turning the cascade on for the request brings the error back.
    let json = json!({"q": "red shoes", "proximity": true, "relax": true});
    assert!(matches!(
        run(c, &json),
        Err(QueryRequestError::ProximityRelaxed)
    ));
}

/// Every hit of `request`, `limit` at a time, each page asked for with the token of the last hit of the one before.
fn paged(c: &CollectionSnapshot, mut request: J, limit: usize) -> Vec<String> {
    request["limit"] = json!(limit);
    let mut all = Vec::new();
    loop {
        let page = plain(run(c, &request).unwrap());
        let Some(last) = page.hits.last() else {
            return all;
        };
        assert!(page.hits.len() <= limit);
        all.extend(page.hits.iter().map(|h| h.document.id().to_owned()));
        request["search_after"] = json!(last.cursor().to_token());
    }
}

/// The leaf's test: a cursor from a proximity-on search pages only a proximity-on search.
#[test]
fn a_cursor_pages_only_under_its_own_proximity_setting() {
    let db = database(RelaxationConfig::default());
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let request = |proximity: Option<bool>, after: Option<&str>| {
        let mut json = json!({"q": "red shoes", "relax": false, "limit": 2});
        if let Some(proximity) = proximity {
            json["proximity"] = json!(proximity);
        }
        if let Some(token) = after {
            json["search_after"] = json!(token);
        }
        json
    };
    let first = plain(run(c, &request(Some(true), None)).unwrap());
    assert_eq!(ids(&first), BY_PROXIMITY[..2]);
    let cursor = first.hits[1].cursor();
    assert!(cursor.proximity);
    let token = cursor.to_token();

    // Asked with `"proximity": false`, or without the key: an error, not a page of another ranking.
    for other in [Some(false), None] {
        let err = run(c, &request(other, Some(&token))).unwrap_err();
        assert!(
            matches!(
                err,
                QueryRequestError::Request(RequestError::CursorProximity {
                    cursor: true,
                    plan: false
                })
            ),
            "{err}"
        );
        assert_eq!(
            err.to_string(),
            "cursor from a plan with proximity true; this search's plan has proximity false"
        );
    }
    // With the same setting it pages exactly: the next two hits of the same ranking.
    let second = plain(run(c, &request(Some(true), Some(&token))).unwrap());
    assert_eq!(ids(&second), BY_PROXIMITY[2..4]);
    for limit in [1, 2, 3, 4, 6] {
        let on = json!({"q": "red shoes", "relax": false, "proximity": true});
        assert_eq!(paged(c, on, limit), BY_PROXIMITY, "{limit}");
        let off = json!({"q": "red shoes", "relax": false});
        assert_eq!(paged(c, off, limit), BY_LOCATION, "{limit}");
    }

    // The reverse: a cursor from a search without the bonus, given to one with it.
    let first = plain(run(c, &request(None, None)).unwrap());
    let cursor = first.hits[1].cursor();
    assert!(!cursor.proximity);
    let plain_token = cursor.to_token();
    assert_ne!(plain_token, token);
    assert!(matches!(
        run(c, &request(Some(true), Some(&plain_token))),
        Err(QueryRequestError::Request(RequestError::CursorProximity {
            cursor: false,
            plan: true
        }))
    ));
    // A search that relaxes never scores the bonus, so a proximity-on cursor does not page it either.
    let relaxing = json!({"q": "red shoes", "limit": 2, "search_after": token});
    assert!(matches!(
        run(c, &relaxing),
        Err(QueryRequestError::Request(RequestError::CursorProximity {
            cursor: true,
            plan: false
        }))
    ));
}
