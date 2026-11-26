//! The response JSON (ROADMAP 10.1.13; SPEC §7.5.2, §7.6, §7.7; ADR-0065).
//!
//! Invariants:
//! 1. Golden responses: a plain search and relaxed ones (fuzzy with corrections, the L5 fallback), with facets, sort
//!    values of every column type, a projection, a blob reference and explanations, written as the committed text.
//! 2. Every response is valid JSON, and it states what the typed response holds: the hits in order with their scores
//!    and sort values, the facets and the relaxation.
//! 3. A hit's cursor token pages the request's next page: the pages by token are the pages by offset.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_engine::db::{Database, DatabaseOptions, ManualClock, SegmentIds, Threads};
use persia_engine::document::{Document, Value};
use persia_engine::query::QueryLimits;
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::search::SortValue;
use persia_engine::search::request::{QueryRequest, QueryResponse, QueryResults, RunOptions};
use persia_engine::wal::Durability;
use persia_format::{BlobId, BlobRef};
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestRunner};
use serde_json::{Value as J, json};

const PROPTEST_SEED: u64 = 0x1011_3000_0001;

fn schema() -> CollectionSchema {
    let facet = KeywordOptions::default().facet(true);
    SchemaBuilder::new("shoes")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("brand", FieldKind::Keyword(facet))
        .field("price", FieldKind::F64 { stored: true })
        .field("stock", FieldKind::I64 { stored: true })
        .field("active", FieldKind::Bool { stored: true })
        .field("at", FieldKind::Datetime { stored: true })
        .field("notes", FieldKind::Stored)
        .build()
        .unwrap()
}

/// `(id, title, brand, price, stock, active, at)`: the first three are committed to a segment, the rest stay in the
/// memtable. `c` has a blob reference, `d` no price and no stock.
type Doc = (
    &'static str,
    &'static str,
    &'static str,
    Option<f64>,
    Option<i64>,
    bool,
    i64,
);

const DOCS: [Doc; 6] = [
    (
        "a",
        "red running shoes",
        "nike",
        Some(80.5),
        Some(3),
        true,
        1_700_000_000_000_000,
    ),
    (
        "b",
        "red trail shoes",
        "acme",
        Some(120.0),
        Some(-2),
        false,
        1_600_000_000_000_000,
    ),
    (
        "c",
        "blue running shoes",
        "nike",
        Some(60.0),
        Some(0),
        true,
        1_650_000_000_000_000,
    ),
    ("d", "red hat", "acme", None, None, true, 0),
    (
        "e",
        "green boots",
        "zapatería",
        Some(150.25),
        Some(7),
        false,
        -86_400_000_000,
    ),
    (
        "f",
        "red sandals",
        "nike",
        Some(40.0),
        Some(1),
        true,
        1_700_000_000_000_001,
    ),
];

fn database() -> Database<SimFile> {
    let schema = schema();
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(13),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [13; 16], options).unwrap();
    let field = |n: &str| schema.field(n).unwrap().id();
    let (title, brand, price, stock, active, at, notes) = (
        field("title"),
        field("brand"),
        field("price"),
        field("stock"),
        field("active"),
        field("at"),
        field("notes"),
    );
    db.create_collection(schema).unwrap();
    for (i, (id, text, by, cost, count, on, when)) in DOCS.into_iter().enumerate() {
        let mut doc = Document::new(id)
            .with(title, Value::String(text.into()))
            .with(brand, Value::String(by.into()))
            .with(active, Value::Bool(on))
            .with(at, Value::Datetime(when))
            .with(notes, Value::Json(json!({"n": i, "tags": ["x", "\"q\""]})));
        if let Some(cost) = cost {
            doc = doc.with(price, Value::F64(cost));
        }
        if let Some(count) = count {
            doc = doc.with(stock, Value::I64(count));
        }
        if id == "c" {
            let meta = BTreeMap::from([("k".to_owned(), "v".to_owned())]);
            doc.set_blob_ref(Some(
                BlobRef::new(
                    BlobId::from_bytes([7; 32]),
                    1234,
                    "image/png",
                    64 << 10,
                    meta,
                )
                .unwrap(),
            ));
        }
        db.put("shoes", &doc, Durability::Fsync).unwrap();
        if i == 2 {
            db.commit().unwrap();
        }
    }
    db
}

/// On the calling thread, with a budget clock that never moves: nothing here reads the wall clock.
fn options() -> RunOptions {
    RunOptions::new()
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

fn run(db: &Database<SimFile>, request: &J) -> (QueryResponse, String) {
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let parsed =
        QueryRequest::from_json(&request.to_string(), c.schema(), &QueryLimits::default()).unwrap();
    let response = parsed.run(c, &options()).unwrap();
    let json = response.to_json(c.schema()).unwrap();
    (response, json)
}

/// `actual` is the committed golden text (a file of one line). On a mismatch the message holds the text to commit,
/// after checking the change is wanted.
fn golden(name: &str, committed: &str, actual: &str) {
    assert_eq!(
        committed.trim_end(),
        actual,
        "{name}: the response JSON changed; if on purpose, commit this line to tests/response_json/{name}.json:\n{actual}"
    );
}

// --- 1: golden responses ----------------------------------------------------------------------------------------------

#[test]
fn golden_plain_response() {
    let db = database();
    let request = json!({
        "q": "red shoes",
        "relax": false,
        "sort": ["score", {"field": "price", "order": "desc", "missing": "first"},
                 {"field": "stock"}, {"field": "active"}, {"field": "at", "order": "desc"},
                 {"field": "brand"}],
        "facets": ["brand"],
        "explain": true,
        "projection": ["price", "notes"],
    });
    let (response, json) = run(&db, &request);
    golden("plain", include_str!("response_json/plain.json"), &json);
    let QueryResults::Plain(results) = &response.results else {
        panic!("not plain")
    };
    let parsed: J = serde_json::from_str(&json).unwrap();
    // A plain search has no level, step or rank score per hit, and states level `exact` with nothing applied.
    let hit = &parsed["hits"][0];
    assert!(
        hit.get("level").is_none() && hit.get("step").is_none() && hit.get("rank_score").is_none()
    );
    assert_eq!(parsed["relaxation"]["level"], "exact");
    assert_eq!(parsed["relaxation"]["applied"], json!([]));
    assert_eq!(parsed["relaxation"]["original"], "red shoes");
    assert_eq!(parsed["relaxation"]["effective"], "red shoes");
    assert_eq!(parsed["relaxation"]["truncated"], false);
    assert!(parsed["relaxation"].get("fallback").is_none());
    // The projection: `price` and `notes` only, beside the id.
    let document = hit["document"].as_object().unwrap();
    let keys: Vec<&str> = document.keys().map(String::as_str).collect();
    assert_eq!(keys, ["id", "notes", "price"]);
    // Explained: the tree of a `bool` of two words, without a `relaxed` node.
    assert!(hit["explanation"]["bool"].is_object(), "{hit}");
    assert_eq!(results.hits.len(), parsed["hits"].as_array().unwrap().len());
}

#[test]
fn golden_relaxed_responses() {
    let db = database();
    // L4 corrects `snadals` to `sandals`; facets and explanations of a relaxed search; the blob reference of `c`.
    let fuzzy = json!({
        "q": "red snadals",
        "relax": {"levels": ["fuzzy"]},
        "facets": [{"field": "brand", "size": 2}],
        "explain": true,
    });
    let (_, json) = run(&db, &fuzzy);
    golden("fuzzy", include_str!("response_json/fuzzy.json"), &json);
    let parsed: J = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["relaxation"]["level"], "fuzzy");
    assert_eq!(
        parsed["relaxation"]["corrected"],
        json!({"snadals": "sandals"})
    );
    let hit = &parsed["hits"][0];
    assert_eq!(hit["level"], "fuzzy");
    assert_eq!(hit["explanation"]["relaxed"]["level"], "fuzzy");
    assert_eq!(hit["explanation"]["relaxed"]["applied"], "fuzzy");

    // No word matches: L5 ranks the collection by recency, the explicit flag says so, and `c`'s blob comes back.
    let fallback = json!({"q": "purple", "limit": 6, "explain": true, "projection": "none"});
    let (_, json) = run(&db, &fallback);
    golden(
        "fallback",
        include_str!("response_json/fallback.json"),
        &json,
    );
    let parsed: J = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["relaxation"]["level"], "fallback");
    assert_eq!(parsed["relaxation"]["fallback"], "recency");
    let hits = parsed["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 6);
    let c = hits.iter().find(|h| h["document"]["id"] == "c").unwrap();
    assert_eq!(c["document"]["blob"]["mime"], "image/png");
    assert!(hits.iter().all(|h| h["level"] == "fallback"));
    assert!(
        hits.iter()
            .all(|h| h["explanation"]["relaxed"]["explanation"]["static_rank"].is_object())
    );
}

// --- 2, 3: every response, and its cursors ----------------------------------------------------------------------------

fn runner(cases: u32) -> TestRunner {
    let config = ProptestConfig {
        cases,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    };
    TestRunner::new(config)
}

/// A request of the grammar's keys that change what a response holds: the text, the sort, relaxation, facets,
/// explain and the projection.
fn any_request() -> impl Strategy<Value = J> {
    let words = prop_oneof![
        Just("red"),
        Just("shoes"),
        Just("snadals"),
        Just("runing"),
        Just("purple"),
        Just("zap"),
        Just("the"),
    ];
    let q = proptest::collection::vec(words, 0..4).prop_map(|w| w.join(" "));
    let key = prop_oneof![
        Just(json!("score")),
        Just(json!({"score": {"order": "asc"}})),
        Just(json!({"field": "price", "order": "desc"})),
        Just(json!({"field": "stock", "missing": "first"})),
        Just(json!({"field": "active"})),
        Just(json!({"field": "at", "order": "desc"})),
        Just(json!({"field": "brand"})),
    ];
    let sort = proptest::option::of(proptest::collection::vec(key, 1..4));
    let relax = prop_oneof![
        Just(None),
        Just(Some(json!(false))),
        Just(Some(json!({"fill": true}))),
        Just(Some(
            json!({"min_results": 3, "levels": ["drop_terms", "prefix", "fuzzy"]})
        )),
        Just(Some(json!({"fallback": "none"}))),
    ];
    let projection = prop_oneof![
        Just(None),
        Just(Some(json!("none"))),
        Just(Some(json!(["price", "brand"]))),
    ];
    (
        q,
        sort,
        relax,
        any::<bool>(),
        any::<bool>(),
        projection,
        1..4_usize,
    )
        .prop_map(|(q, sort, relax, facets, explain, projection, limit)| {
            let mut o = serde_json::Map::new();
            o.insert("q".into(), json!(q));
            o.insert("limit".into(), json!(limit));
            o.insert("explain".into(), json!(explain));
            if let Some(sort) = sort {
                o.insert("sort".into(), json!(sort));
            }
            if let Some(relax) = relax {
                o.insert("relax".into(), relax);
            }
            if facets {
                o.insert("facets".into(), json!(["brand"]));
            }
            if let Some(projection) = projection {
                o.insert("projection".into(), projection);
            }
            J::Object(o)
        })
}

/// The hit ids, scores and sort values of `response`, as the typed response holds them.
fn typed_hits(response: &QueryResponse) -> Vec<(String, f64, usize)> {
    let hits: Vec<_> = match &response.results {
        QueryResults::Relaxed(r) => r.hits.iter().map(|h| &h.hit).collect(),
        QueryResults::Plain(r) => r.hits.iter().collect(),
        other => panic!("{other:?}"),
    };
    hits.iter()
        .map(|h| (h.document.id().to_owned(), h.score, h.sort.len()))
        .collect()
}

#[test]
fn every_response_is_valid_json_that_states_the_typed_one_and_its_cursors_page_on() {
    let db = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    runner(256)
        .run(&any_request(), |request| {
            let parsed =
                QueryRequest::from_json(&request.to_string(), c.schema(), &QueryLimits::default())
                    .unwrap();
            let response = parsed.run(c, &options()).unwrap();
            let text = response.to_json(c.schema()).unwrap();
            let json: J = serde_json::from_str(&text)
                .map_err(|e| TestCaseError::fail(format!("{e}: {text}")))?;
            // Invariant 2: the hits, their scores and sort values, the facets, the relaxation.
            let hits = json["hits"].as_array().unwrap();
            let typed = typed_hits(&response);
            prop_assert_eq!(hits.len(), typed.len());
            for (hit, (id, score, sorts)) in hits.iter().zip(&typed) {
                prop_assert_eq!(hit["document"]["id"].as_str(), Some(id.as_str()));
                prop_assert_eq!(hit["score"].as_f64(), Some(*score));
                prop_assert_eq!(hit["sort"].as_array().map(Vec::len), Some(*sorts));
                prop_assert!(hit["cursor"].is_string());
                prop_assert_eq!(
                    hit.get("explanation").is_some(),
                    request["explain"] == json!(true)
                );
            }
            let relaxation = response.relaxation();
            prop_assert_eq!(
                json["relaxation"]["level"].as_str(),
                Some(relaxation.level.name())
            );
            prop_assert_eq!(
                json["relaxation"]["truncated"].as_bool(),
                Some(relaxation.truncated)
            );
            prop_assert_eq!(
                json["facets"].as_array().map(Vec::len),
                Some(request.get("facets").map_or(0, |_| 1))
            );
            prop_assert_eq!(
                json["query_truncated"].as_bool(),
                Some(response.query_truncated)
            );
            // Invariant 3: the last hit's token pages on, as the offset does.
            if let Some(token) = hits.last().and_then(|h| h["cursor"].as_str()) {
                let limit = request["limit"].as_u64().unwrap();
                let mut next = request.clone();
                next["search_after"] = json!(token);
                let by_token = run(&db, &next).0;
                let mut deeper = request.clone();
                deeper["offset"] = json!(limit);
                let by_offset = run(&db, &deeper).0;
                let ids =
                    |r: &QueryResponse| typed_hits(r).into_iter().map(|h| h.0).collect::<Vec<_>>();
                prop_assert_eq!(ids(&by_token), ids(&by_offset), "{}", request);
            }
            Ok(())
        })
        .unwrap();
}

/// Each sort value is written as its key's field types it (SPEC §7.5.2): an `i64` as an integer, an `f64` as a number, a
/// `bool`, a `datetime` in the document's output text, a keyword as a string, the score as a number, and `null` for a
/// document without a value (`d` has no stock and no price).
#[test]
fn sort_values_are_typed_by_their_key() {
    let db = database();
    let request = json!({
        "q": "",
        "relax": false,
        "limit": 6,
        "sort": [{"field": "stock", "order": "desc"}, {"field": "price"}, {"field": "active"},
                 {"field": "at"}, {"field": "brand"}, "score"],
    });
    let (response, json) = run(&db, &request);
    let QueryResults::Plain(results) = &response.results else {
        panic!("not plain")
    };
    let parsed: J = serde_json::from_str(&json).unwrap();
    let sorts: Vec<&J> = parsed["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| &h["sort"])
        .collect();
    // Stock descending, missing last: `e` 7, `a` 3, `f` 1, `c` 0, `b` -2, then `d` without a stock.
    assert_eq!(
        sorts,
        [
            &json!([7, 150.25, false, "1969-12-31T00:00:00Z", "zapatería", 0.0]),
            &json!([3, 80.5, true, "2023-11-14T22:13:20Z", "nike", 0.0]),
            &json!([1, 40.0, true, "2023-11-14T22:13:20.000001Z", "nike", 0.0]),
            &json!([0, 60.0, true, "2022-04-15T05:20:00Z", "nike", 0.0]),
            &json!([-2, 120.0, false, "2020-09-13T12:26:40Z", "acme", 0.0]),
            &json!([null, null, true, "1970-01-01T00:00:00Z", "acme", 0.0]),
        ]
    );
    assert!(matches!(results.hits[5].sort[0], SortValue::Missing));
}
