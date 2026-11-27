//! The typed getters of a `persia::Hit` against the response JSON (ROADMAP 10.1.30; SPEC §7.4, §7.5.2, §7.6; ADR-0065
//! amended).
//!
//! Invariants:
//! 1. `Hit::sort_values` has one value per key of the request's sort (the score alone without one), each of its key's
//!    type or `Missing`, and the hit's `"sort"` array in `SearchResponse::to_json` is those values in order: the
//!    score and an `f64` as the same number, an `i64` as the same integer, a `datetime` as the text that reads back
//!    as the same microseconds, a `bool`, a keyword as its string, and `Missing` as `null`. Each value is the one the
//!    hit's document holds in the key's field, and `Missing` exactly when it holds none.
//! 2. `Hit::step` and `Hit::rank_score` are `Some` exactly for a relaxed search's hits, and the JSON has `"step"`,
//!    `"rank_score"` and `"level"` exactly then, with the same values (`"level"` is `Hit::level`); a plain search's
//!    hits have none of the three keys.
//!
//! Checked on a table of requests (plain and relaxed, every sort type, missing sort values, L5, an empty page) and on
//! seeded random requests (the seed is in every failure message).
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use persia::{
    Blob, Collection, CollectionSchema, Db, Document, FieldKind, Hit, KeywordOptions,
    SchemaBuilder, SortValue, TextOptions, Value,
};
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestRunner};
use serde_json::{Value as J, json};

/// `c` gets a blob, `d` has no price and no stock, `g` has no `at` and no `active`.
const DOCS: [&str; 7] = [
    r#"{"id": "a", "title": "red running shoes", "brand": "nike", "price": 80.5, "stock": 3, "active": true, "at": "2023-11-14T22:13:20Z"}"#,
    r#"{"id": "b", "title": "red trail shoes", "brand": "acme", "price": 120.0, "stock": -2, "active": false, "at": "2020-09-13T12:26:40Z"}"#,
    r#"{"id": "c", "title": "blue running shoes", "brand": "nike", "price": 60.0, "stock": 0, "active": true, "at": "2022-04-15T03:46:40Z"}"#,
    r#"{"id": "d", "title": "red hat", "brand": "acme", "active": true, "at": "1970-01-01T00:00:00Z"}"#,
    r#"{"id": "e", "title": "green boots", "brand": "zapatería", "price": 150.25, "stock": 7, "active": false, "at": "1969-12-31T00:00:00Z"}"#,
    r#"{"id": "f", "title": "red sandals", "brand": "nike", "price": 40.0, "stock": 1, "active": true, "at": "2023-11-14T22:13:20.000001Z"}"#,
    r#"{"id": "g", "title": "red socks", "price": -0.5, "stock": 9}"#,
];

/// A collection with a field of every sortable type, some documents in a segment and some in the memtable.
fn shoes() -> (tempfile::TempDir, Db, Collection) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let schema = SchemaBuilder::new("shoes")
        .field(
            "title",
            FieldKind::Text(TextOptions::default().positions(true)),
        )
        .field(
            "brand",
            FieldKind::Keyword(KeywordOptions::default().facet(true)),
        )
        .field("price", FieldKind::F64 { stored: true })
        .field("stock", FieldKind::I64 { stored: true })
        .field("active", FieldKind::Bool { stored: true })
        .field("at", FieldKind::Datetime { stored: true })
        .build()
        .unwrap();
    let shoes = db.create_collection(schema).unwrap();
    for (i, doc) in DOCS.iter().enumerate() {
        if i == 2 {
            let doc = Document::from_json(doc, &shoes.schema().unwrap()).unwrap();
            shoes
                .put_with_blob(&doc, Blob::from_reader(&b"\x89PNG"[..]))
                .unwrap();
            shoes.commit().unwrap();
        } else {
            shoes.put_json(doc).unwrap();
        }
    }
    (dir, db, shoes)
}

/// The kind of value a sort key of a request's JSON gives (`"score"`, or a field of [`shoes`] by name).
fn key_kind(key: &J) -> &'static str {
    let field = match key {
        J::String(s) if s == "score" => return "score",
        J::Object(o) => o["field"].as_str().unwrap(),
        other => panic!("not a sort key: {other}"),
    };
    match field {
        "price" => "f64",
        "stock" => "i64",
        "active" => "bool",
        "at" => "datetime",
        "brand" => "keyword",
        other => panic!("no sortable field {other}"),
    }
}

fn kind(value: &SortValue) -> &'static str {
    match value {
        SortValue::Score(_) => "score",
        SortValue::F64(_) => "f64",
        SortValue::I64(_) => "i64",
        SortValue::Bool(_) => "bool",
        SortValue::Datetime(_) => "datetime",
        SortValue::Keyword(_) => "keyword",
        SortValue::Missing => "missing",
        other => panic!("a sort value this test does not know: {other:?}"),
    }
}

/// The microseconds the `datetime` text `text` reads back as, through a document of `schema`'s field `at`.
fn micros_of(text: &J, schema: &CollectionSchema) -> i64 {
    let doc = Document::from_json(&json!({"id": "x", "at": text}).to_string(), schema).unwrap();
    match doc.get(schema.field("at").unwrap().id()) {
        Some(Value::Datetime(micros)) => *micros,
        other => panic!("{text} read as {other:?}"),
    }
}

/// The sort value the hit's own document gives the field key `key` (`None` for the score key, and for a field the
/// document does not hold or its projection left out), as [`SortValue`] types it.
fn document_value(hit: &Hit, key: &J, schema: &CollectionSchema) -> Option<SortValue> {
    let name = key.get("field")?.as_str()?;
    Some(match hit.document.get(schema.field(name)?.id())? {
        Value::F64(v) => SortValue::F64(*v),
        Value::I64(v) => SortValue::I64(*v),
        Value::Bool(v) => SortValue::Bool(*v),
        Value::Datetime(v) => SortValue::Datetime(*v),
        Value::String(s) => SortValue::Keyword(s.as_bytes().to_vec()),
        other => panic!("{name} holds {other:?}"),
    })
}

/// What the getters of each hit of `request`'s response say against its JSON (invariants 1 and 2); returns the hits
/// and their JSON objects for the caller's own checks.
fn check(shoes: &Collection, request: &J, context: &str) -> Vec<(Hit, J)> {
    let schema = shoes.schema().unwrap();
    let response = shoes.search_json(&request.to_string()).unwrap();
    let json: J = serde_json::from_str(&response.to_json().unwrap()).unwrap();
    let objects = json["hits"].as_array().unwrap();
    assert_eq!(objects.len(), response.hits.len(), "{context}");
    let default_sort = [json!("score")];
    let keys = request
        .get("sort")
        .map_or(&default_sort[..], |s| s.as_array().unwrap().as_slice());
    for (i, (hit, object)) in response.hits.iter().zip(objects).enumerate() {
        let context = format!("{context}, hit {i} ({}): {object}", hit.document.id());
        // Invariant 1.
        let values = hit.sort_values();
        let sort = object["sort"].as_array().unwrap();
        assert_eq!(values.len(), keys.len(), "{context}");
        assert_eq!(sort.len(), values.len(), "{context}");
        for ((value, written), key) in values.iter().zip(sort).zip(keys) {
            let kind = kind(value);
            assert!(
                kind == key_kind(key) || kind == "missing",
                "{context}: {value:?} for the key {key}"
            );
            // The value is the document's own, and `Missing` only when the document has none.
            match document_value(hit, key, &schema) {
                Some(own) => assert_eq!(value, &own, "{context}: the document's value"),
                None if kind != "score" => assert!(
                    request.get("projection").is_some() || kind == "missing",
                    "{context}: {value:?} for a field the document does not hold"
                ),
                None => {}
            }
            match value {
                SortValue::Score(n) => {
                    assert_eq!(written.as_f64(), Some(*n), "{context}");
                    assert_eq!(
                        *n, hit.score,
                        "{context}: the score key's value is the score"
                    );
                }
                SortValue::F64(n) => assert_eq!(written.as_f64(), Some(*n), "{context}"),
                SortValue::I64(n) => {
                    assert!(written.is_i64(), "{context}: an integer");
                    assert_eq!(written.as_i64(), Some(*n), "{context}");
                }
                SortValue::Bool(b) => assert_eq!(written.as_bool(), Some(*b), "{context}"),
                SortValue::Datetime(micros) => {
                    assert!(written.is_string(), "{context}");
                    assert_eq!(micros_of(written, &schema), *micros, "{context}");
                }
                SortValue::Keyword(bytes) => match std::str::from_utf8(bytes) {
                    Ok(text) => assert_eq!(written.as_str(), Some(text), "{context}"),
                    Err(_) => assert!(written["base64url"].is_string(), "{context}"),
                },
                SortValue::Missing => assert!(written.is_null(), "{context}"),
                other => panic!("{context}: {other:?}"),
            }
        }
        // Invariant 2.
        let relaxed = hit.step().is_some();
        assert_eq!(hit.rank_score().is_some(), relaxed, "{context}");
        for key in ["step", "rank_score", "level"] {
            assert_eq!(object.get(key).is_some(), relaxed, "{context}: {key}");
        }
        if relaxed {
            assert_eq!(
                object["step"].as_u64(),
                hit.step().map(|s| u64::try_from(s).unwrap()),
                "{context}"
            );
            assert_eq!(object["rank_score"].as_f64(), hit.rank_score(), "{context}");
            assert_eq!(
                object["level"].as_str(),
                Some(hit.level.name()),
                "{context}"
            );
        }
    }
    response
        .hits
        .into_iter()
        .zip(objects.iter().cloned())
        .collect()
}

#[test]
fn the_getters_state_what_the_json_writes_for_a_table_of_requests() {
    let (_dir, _db, shoes) = shoes();
    let every_type = json!([
        "score",
        {"field": "price", "order": "desc", "missing": "first"},
        {"field": "stock"},
        {"field": "active"},
        {"field": "at", "order": "desc"},
        {"field": "brand"}
    ]);
    // (request, relaxed, a hit with a missing sort value)
    let table = [
        (
            json!({"q": "red shoes", "relax": false, "sort": every_type}),
            false,
            false,
        ),
        (
            json!({"q": "red", "relax": false, "sort": [{"field": "price"}]}),
            false,
            true,
        ),
        (
            json!({"q": "red", "relax": false, "sort": [{"field": "at", "missing": "first"}, {"field": "brand"}]}),
            false,
            true,
        ),
        (json!({"q": "shoes", "relax": false}), false, false),
        (
            json!({"q": "red", "relax": false, "limit": 0}),
            false,
            false,
        ),
        (json!({"q": "red"}), true, false),
        (
            json!({"q": "red snadals", "relax": {"levels": ["fuzzy"]}}),
            true,
            false,
        ),
        (
            json!({"q": "red zebra", "relax": {"levels": ["drop_terms"]}, "sort": [{"field": "stock", "order": "desc"}]}),
            true,
            true,
        ),
        (
            json!({"q": "red", "sort": [{"field": "active", "order": "desc"}, {"field": "at", "missing": "first"}]}),
            true,
            true,
        ),
        (
            json!({"q": "purple", "limit": 7, "projection": "none"}),
            true,
            false,
        ),
        (
            json!({"q": "purple", "limit": 7, "sort": [{"field": "price"}, "score"]}),
            true,
            true,
        ),
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (request, relaxed, missing) in table {
        let context = format!("request {request}");
        let hits = check(&shoes, &request, &context);
        let limit = request.get("limit").and_then(J::as_u64);
        assert_eq!(hits.is_empty(), limit == Some(0), "{context}: hits");
        for (hit, _) in &hits {
            assert_eq!(
                hit.step().is_some(),
                relaxed,
                "{context}: {}",
                hit.document.id()
            );
            seen.extend(hit.sort_values().iter().map(kind));
        }
        let has_missing = hits
            .iter()
            .any(|(hit, _)| hit.sort_values().contains(&SortValue::Missing));
        assert_eq!(has_missing, missing, "{context}: a missing sort value");
    }
    let every_kind = [
        "bool", "datetime", "f64", "i64", "keyword", "missing", "score",
    ];
    assert_eq!(seen.into_iter().collect::<Vec<_>>(), every_kind);
}

/// The values of the table's first request, written out: the getters type each key's value as its field holds it.
#[test]
fn the_sort_values_are_the_documents_values_typed_by_their_keys() {
    let (_dir, _db, shoes) = shoes();
    let request = json!({"q": "trail", "relax": false, "sort": [
        "score", {"field": "price"}, {"field": "stock"}, {"field": "active"}, {"field": "at"}, {"field": "brand"}
    ]});
    let hits = check(&shoes, &request, "trail");
    let [(hit, _)] = hits.as_slice() else {
        panic!("one hit: {hits:?}")
    };
    assert_eq!(
        hit.sort_values(),
        [
            SortValue::Score(hit.score),
            SortValue::F64(120.0),
            SortValue::I64(-2),
            SortValue::Bool(false),
            SortValue::Datetime(1_600_000_000_000_000),
            SortValue::Keyword(b"acme".to_vec()),
        ]
    );
}

/// Requests of the grammar's keys that change what a hit holds: sorts of every type, relaxed or not, a page.
fn any_request() -> impl Strategy<Value = J> {
    let words = prop_oneof![
        Just("red"),
        Just("shoes"),
        Just("snadals"),
        Just("runing"),
        Just("purple"),
        Just("\"running shoes\""),
    ];
    let key = prop_oneof![
        Just(json!("score")),
        Just(json!({"field": "price", "order": "desc"})),
        Just(json!({"field": "stock", "missing": "first"})),
        Just(json!({"field": "active"})),
        Just(json!({"field": "at", "order": "desc", "missing": "first"})),
        Just(json!({"field": "brand"})),
    ];
    let relax = prop_oneof![
        Just(json!(null)),
        Just(json!(false)),
        Just(json!({"levels": ["fuzzy"]})),
        Just(json!({"levels": ["prefix", "fuzzy"], "fill": true})),
    ];
    (
        prop::collection::vec(words, 0..3),
        prop::option::of(prop::collection::vec(key, 1..4)),
        relax,
        0_usize..8,
        0_usize..3,
    )
        .prop_map(|(words, sort, relax, limit, offset)| {
            let mut request = json!({"q": words.join(" "), "limit": limit, "offset": offset});
            let object = request.as_object_mut().unwrap();
            if let Some(sort) = sort {
                object.insert("sort".into(), J::Array(sort));
            }
            if !relax.is_null() {
                object.insert("relax".into(), relax);
            }
            request
        })
}

/// Invariants 1 and 2 for any request of [`any_request`].
#[test]
fn the_getters_state_what_the_json_writes_for_any_request() {
    const SEED: u64 = 0x1013_0000_0001;
    let (_dir, _db, shoes) = shoes();
    let config = ProptestConfig {
        cases: 64,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(SEED),
        ..ProptestConfig::default()
    };
    TestRunner::new(config)
        .run(&any_request(), |request| {
            // A sort key given twice is refused: such a request has no response to compare.
            if let Some(J::Array(keys)) = request.get("sort") {
                let mut names: Vec<_> = keys.iter().map(key_kind).collect();
                names.sort_unstable();
                names.dedup();
                prop_assume!(names.len() == keys.len());
            }
            check(
                &shoes,
                &request,
                &format!("seed {SEED:#x}, request {request}"),
            );
            Ok(())
        })
        .unwrap();
}
