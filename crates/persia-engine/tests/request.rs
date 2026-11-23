//! The whole request (ROADMAP 10.1.10; SPEC §7.1, §7.4; ADR-0062): its JSON, its typed form, the function that runs
//! it, and the cursor token.
//!
//! Invariants:
//! 1. The JSON is read strictly: an unknown or repeated key (at any depth), `null` for a key and a value of another
//!    shape are errors; a key left out has its default. Arbitrary text never panics.
//! 2. A request built in code and the JSON that says the same parse to equal values, key by key and for arbitrary
//!    requests (the test writes the JSON from its own model of the grammar, not with the engine).
//! 3. `run` is the engine's own search of the same parts: `q` compiled with the collection's analyzers, `filter`
//!    joined hard and `soft_filter` soft, the collection's relaxation settings as the request overrides them; the
//!    relaxed search when they relax, the plain one when they run L0 alone.
//! 4. `filter` is hard at every level and `soft_filter` is what `filters: "last_resort"` drops; a filter given as
//!    both is hard.
//! 5. A cursor crosses JSON as a token: pages by token are the slices of one ranking, and a token that was changed,
//!    cut, or taken from another collection is an error.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::too_many_lines,
    clippy::option_option,
    missing_docs
)] // Tests: inputs are fixed.

use std::ops::Bound;
use std::sync::Arc;
use std::time::Duration;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    CancelToken, Clock, CollectionSnapshot, Cursor, CursorOrigin, Database, DatabaseOptions,
    Execution, Located, ManualClock, RelaxedResults, SearchResults, SegmentIds, StepCursor,
    Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{
    DefaultFields, Filter, FilterValue, QueryCompiler, QueryError, QueryLimits,
};
use persia_engine::relax::{RelaxError, RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, Fallback, FieldId, FieldKind, FilterRelaxation, KeywordOptions,
    Level, Levels, RelaxationConfig, SchemaBuilder, TextOptions,
};
use persia_engine::search::request::{
    CursorTokenError, MAX_CURSOR_TOKEN_LEN, MAX_REQUEST_JSON_LEN, QueryRequest, QueryRequestError,
    QueryResults, RunOptions,
};
use persia_engine::search::{
    FacetRequest, Interrupt, Missing, Order, Projection, Pruning, RequestError, SearchPlan, Sort,
    SortKey, SortValue,
};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestRunner};
use serde_json::{Value as J, json};

const PROPTEST_SEED: u64 = 0x1011_0000_0001;

fn builder(relaxation: RelaxationConfig) -> SchemaBuilder {
    let en = TextOptions {
        analyzer: AnalyzerRef::builtin("en").unwrap(),
        ..TextOptions::default()
    };
    let facet = KeywordOptions {
        facet: true,
        ..KeywordOptions::default()
    };
    SchemaBuilder::new("shoes")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("body", FieldKind::Text(en))
        .field("brand", FieldKind::Keyword(facet))
        .field("tenant", FieldKind::Keyword(KeywordOptions::default()))
        .field("price", FieldKind::F64 { stored: true })
        .field("stock", FieldKind::I64 { stored: true })
        .field("active", FieldKind::Bool { stored: true })
        .field("at", FieldKind::Datetime { stored: true })
        .field("notes", FieldKind::Stored)
        .relaxation(relaxation)
}

fn schema() -> CollectionSchema {
    builder(RelaxationConfig::default()).build().unwrap()
}

fn id(name: &str) -> FieldId {
    schema().field(name).unwrap().id()
}

fn limits() -> QueryLimits {
    QueryLimits::default()
}

fn parse(json: &str) -> Result<QueryRequest, QueryRequestError> {
    QueryRequest::from_json(json, &schema(), &limits())
}

fn keyword(field: &str, value: &str) -> Filter {
    Filter::Term {
        field: id(field),
        value: FilterValue::Keyword(value.into()),
    }
}

/// `(id, title, brand, tenant, price)`: the first four are committed to a segment, the rest stay in the memtable.
const DOCS: [(&str, &str, &str, &str, f64); 6] = [
    ("a", "red running shoes", "nike", "t1", 80.0),
    ("b", "red trail shoes", "acme", "t1", 120.0),
    ("c", "blue running shoes", "nike", "t2", 60.0),
    ("d", "red hat", "acme", "t1", 20.0),
    ("e", "green boots", "nike", "t1", 150.0),
    ("f", "red sandals", "nike", "t2", 40.0),
];

fn database(schema: CollectionSchema) -> Database<SimFile> {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(10),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [10; 16], options).unwrap();
    let name = schema.name().to_owned();
    let field = |n: &str| schema.field(n).unwrap().id();
    let (title, body, brand, tenant, price) = (
        field("title"),
        field("body"),
        field("brand"),
        field("tenant"),
        field("price"),
    );
    db.create_collection(schema).unwrap();
    for (i, (doc, text, by, of, cost)) in DOCS.into_iter().enumerate() {
        let doc = Document::new(doc)
            .with(title, Value::String(text.into()))
            .with(body, Value::String(text.into()))
            .with(brand, Value::String(by.into()))
            .with(tenant, Value::String(of.into()))
            .with(price, Value::F64(cost));
        db.put(&name, &doc, Durability::Fsync).unwrap();
        if i == 3 {
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

fn exec() -> Execution {
    Execution::new(Pruning::default())
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

fn relaxed(results: QueryResults) -> RelaxedResults {
    match results {
        QueryResults::Relaxed(r) => r,
        other => panic!("not a relaxed search: {other:?}"),
    }
}

fn plain(results: QueryResults) -> SearchResults {
    match results {
        QueryResults::Plain(r) => r,
        other => panic!("not a plain search: {other:?}"),
    }
}

fn relaxed_ids(r: &RelaxedResults) -> Vec<&str> {
    r.hits.iter().map(|h| h.hit.document.id()).collect()
}

fn plain_ids(r: &SearchResults) -> Vec<&str> {
    r.hits.iter().map(|h| h.document.id()).collect()
}

// --- 1, 2: the JSON ---------------------------------------------------------------------------------------------------

#[test]
fn every_key_parses_to_its_typed_value() {
    let schema = schema();
    let cursor = Cursor {
        values: vec![SortValue::Score(1.5), SortValue::Key(7)],
        location: Located::Memtable(3),
        origin: CursorOrigin {
            seq: 2,
            version: 6,
            collection: "shoes".into(),
        },
        relaxed: None,
        proximity: false,
        relax: None,
    };
    let json = json!({
        "q": "zapatillas rojas nike",
        "fields": ["title^3", "body"],
        "filter": {"term": ["tenant", "t1"]},
        "soft_filter": {"and": [{"term": ["brand", "nike"]}, {"range": ["price", {"lte": 100}]}]},
        "sort": ["score", {"field": "price", "order": "desc", "missing": "first"}],
        "limit": 5,
        "offset": 2,
        "search_after": cursor.to_token(),
        "facets": ["brand", {"field": "brand", "size": 3}],
        "relax": {"min_results": 4, "fill": true},
        "explain": true,
        "proximity": true,
    });
    let built = QueryRequest::new("zapatillas rojas nike")
        .with_fields(DefaultFields::parse(&schema, &["title^3", "body"]).unwrap())
        .with_filter(keyword("tenant", "t1"))
        .with_soft_filter(Filter::And(vec![
            keyword("brand", "nike"),
            Filter::Range {
                field: id("price"),
                lower: Bound::Unbounded,
                upper: Bound::Included(FilterValue::F64(100.0)),
            },
        ]))
        .with_sort(
            Sort::new(
                [
                    SortKey::score(),
                    SortKey::Field {
                        field: id("price"),
                        order: Order::Desc,
                        missing: Missing::First,
                    },
                ],
                &schema,
            )
            .unwrap(),
        )
        .with_limit(5)
        .with_offset(2)
        .with_search_after(cursor.clone())
        .with_facets([
            FacetRequest::new(id("brand"), 10, &schema).unwrap(),
            FacetRequest::new(id("brand"), 3, &schema).unwrap(),
        ])
        .with_relax(RelaxOptions {
            min_results: Some(4),
            fill: Some(true),
            ..RelaxOptions::default()
        })
        .with_explain(true)
        .with_proximity(true);
    let parsed = parse(&json.to_string()).unwrap();
    assert_eq!(parsed, built);
    // The getters read what the setters wrote.
    assert_eq!(parsed.q(), "zapatillas rojas nike");
    assert_eq!(
        parsed.fields().unwrap().fields(),
        [(id("title"), 3.0), (id("body"), 1.0)]
    );
    assert_eq!(parsed.filter(), Some(&keyword("tenant", "t1")));
    assert!(matches!(parsed.soft_filter(), Some(Filter::And(all)) if all.len() == 2));
    let page = parsed.page();
    assert_eq!((page.limit, page.offset, page.explain), (5, 2, true));
    assert_eq!(page.search_after.as_ref(), Some(&cursor));
    assert_eq!(page.facets.len(), 2);
    assert_eq!(page.projection, Projection::All);
    assert_eq!(parsed.relax().min_results, Some(4));
    assert!(parsed.proximity());
}

#[test]
fn a_key_left_out_has_its_default() {
    let empty = parse("{}").unwrap();
    assert_eq!(empty, QueryRequest::default());
    assert_eq!(empty, QueryRequest::new(""));
    assert_eq!(empty.q(), "");
    assert!(empty.fields().is_none() && empty.filter().is_none() && empty.soft_filter().is_none());
    let page = empty.page();
    assert!(page.sort.is_score());
    assert_eq!((page.offset, page.limit), (0, 20));
    assert!(page.search_after.is_none() && page.facets.is_empty() && !page.explain);
    assert_eq!(empty.relax(), &RelaxOptions::default());
    assert!(!empty.proximity());
    // One key says nothing about the others.
    assert_eq!(
        parse(r#"{"limit": 3}"#).unwrap(),
        QueryRequest::default().with_limit(3)
    );
    assert_eq!(parse(r#"{"q": "red"}"#).unwrap(), QueryRequest::new("red"));
    // `q` is text, never a syntax: nothing in it fails.
    assert_eq!(
        parse(r#"{"q": "-\"unclosed AND (x:"}"#).unwrap().q(),
        "-\"unclosed AND (x:"
    );
}

#[test]
fn sort_forms() {
    let schema = schema();
    let price = id("price");
    let of = |keys: &[SortKey]| Sort::new(keys.iter().copied(), &schema).unwrap();
    let field = |order, missing| SortKey::Field {
        field: price,
        order,
        missing,
    };
    let cases: [(J, Sort); 9] = [
        (json!("score"), Sort::score()),
        (json!([]), Sort::score()),
        (json!(["score"]), Sort::score()),
        (json!([{"score": {}}]), Sort::score()),
        (json!([{"score": {"order": "desc"}}]), Sort::score()),
        (
            json!([{"score": {"order": "asc"}}]),
            of(&[SortKey::Score { order: Order::Asc }]),
        ),
        // A field ranks ascending, documents without a value last, unless it says otherwise.
        (
            json!([{"field": "price"}]),
            of(&[field(Order::Asc, Missing::Last)]),
        ),
        (
            json!([{"field": "price", "order": "desc", "missing": "last"}]),
            of(&[field(Order::Desc, Missing::Last)]),
        ),
        (
            json!([{"field": "price", "order": "asc", "missing": "first"}, "score"]),
            of(&[field(Order::Asc, Missing::First), SortKey::score()]),
        ),
    ];
    for (sort, want) in cases {
        let request = parse(&json!({ "sort": sort }).to_string()).unwrap();
        assert_eq!(request.page().sort, want, "{sort}");
    }
}

#[test]
fn facet_forms() {
    let schema = schema();
    let brand = |size| FacetRequest::new(id("brand"), size, &schema).unwrap();
    let request = parse(
        r#"{"facets": ["brand", {"field": "brand"}, {"field": "brand", "size": 1}, {"size": 1000, "field": "brand"}]}"#,
    )
    .unwrap();
    assert_eq!(
        request.page().facets,
        [brand(10), brand(10), brand(1), brand(1000)]
    );
    assert_eq!(parse(r#"{"facets": []}"#).unwrap().page().facets.len(), 0);
    // At most 16, a field may repeat.
    let sixteen = json!({ "facets": vec!["brand"; 16] }).to_string();
    assert_eq!(parse(&sixteen).unwrap().page().facets.len(), 16);
}

/// What a request that does not parse failed as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Json,
    Query,
    Relax,
    Cursor,
    Request,
}

fn kind(error: &QueryRequestError) -> Kind {
    match error {
        QueryRequestError::Json(_) => Kind::Json,
        QueryRequestError::Query(_) => Kind::Query,
        QueryRequestError::Relax(_) => Kind::Relax,
        QueryRequestError::Cursor(_) => Kind::Cursor,
        QueryRequestError::Request(_) => Kind::Request,
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_json_is_read_strictly() {
    let seventeen_keys = json!({ "sort": vec!["score"; 17] }).to_string();
    let seventeen_facets = json!({ "facets": vec!["brand"; 17] }).to_string();
    let cases: &[(&str, Kind)] = &[
        // Not a request at all.
        ("", Kind::Json),
        ("{", Kind::Json),
        ("[]", Kind::Json),
        ("\"q\"", Kind::Json),
        ("7", Kind::Json),
        ("null", Kind::Json),
        (r#"{"q": "a"} trailing"#, Kind::Json),
        // Unknown and repeated keys, at the top and below it.
        (r#"{"query": "a"}"#, Kind::Json),
        (r#"{"Q": "a"}"#, Kind::Json),
        (r#"{"projection": ["title"]}"#, Kind::Json),
        (r#"{"q": "a", "q": "b"}"#, Kind::Json),
        (r#"{"limit": 1, "q": "a", "limit": 1}"#, Kind::Json),
        (
            r#"{"filter": {"term": ["brand", "a"], "term": ["brand", "b"]}}"#,
            Kind::Json,
        ),
        (
            r#"{"filter": {"range": ["price", {"gt": 1, "gt": 2}]}}"#,
            Kind::Json,
        ),
        (
            r#"{"sort": [{"field": "price", "field": "stock"}]}"#,
            Kind::Json,
        ),
        (r#"{"relax": {"fill": true, "fill": true}}"#, Kind::Json),
        (
            r#"{"facets": [{"field": "brand", "size": 1, "size": 2}]}"#,
            Kind::Json,
        ),
        // `null` is not "left out".
        (r#"{"q": null}"#, Kind::Json),
        (r#"{"fields": null}"#, Kind::Json),
        (r#"{"filter": null}"#, Kind::Json),
        (r#"{"soft_filter": null}"#, Kind::Json),
        (r#"{"sort": null}"#, Kind::Json),
        (r#"{"limit": null}"#, Kind::Json),
        (r#"{"offset": null}"#, Kind::Json),
        (r#"{"search_after": null}"#, Kind::Json),
        (r#"{"facets": null}"#, Kind::Json),
        (r#"{"relax": null}"#, Kind::Json),
        (r#"{"explain": null}"#, Kind::Json),
        (r#"{"proximity": null}"#, Kind::Json),
        (r#"{"proximity": "on"}"#, Kind::Json),
        // A value of another shape than its key's.
        (r#"{"q": 5}"#, Kind::Json),
        (r#"{"q": ["a"]}"#, Kind::Json),
        (r#"{"fields": "title"}"#, Kind::Json),
        (r#"{"fields": ["title", 3]}"#, Kind::Json),
        (r#"{"limit": -1}"#, Kind::Json),
        (r#"{"limit": 1.0}"#, Kind::Json),
        (r#"{"limit": 1e2}"#, Kind::Json),
        (r#"{"limit": "10"}"#, Kind::Json),
        (r#"{"limit": 18446744073709551616}"#, Kind::Json),
        (r#"{"offset": 0.5}"#, Kind::Json),
        (r#"{"offset": true}"#, Kind::Json),
        (r#"{"explain": "true"}"#, Kind::Json),
        (r#"{"explain": 1}"#, Kind::Json),
        (r#"{"search_after": 5}"#, Kind::Json),
        (r#"{"search_after": {"values": []}}"#, Kind::Json),
        (r#"{"facets": "brand"}"#, Kind::Json),
        (r#"{"facets": [5]}"#, Kind::Json),
        (r#"{"facets": [{"field": "brand", "top": 3}]}"#, Kind::Json),
        (
            r#"{"facets": [{"field": "brand", "size": "3"}]}"#,
            Kind::Json,
        ),
        (
            r#"{"facets": [{"field": "brand", "size": 2.5}]}"#,
            Kind::Json,
        ),
        (
            r#"{"facets": [{"field": "brand", "size": null}]}"#,
            Kind::Json,
        ),
        (r#"{"facets": [{"size": 3}]}"#, Kind::Json),
        (r#"{"facets": [{"field": 3}]}"#, Kind::Json),
        (r#"{"sort": "price"}"#, Kind::Json),
        (r#"{"sort": {"field": "price"}}"#, Kind::Json),
        (r#"{"sort": [5]}"#, Kind::Json),
        (r#"{"sort": ["price"]}"#, Kind::Json),
        (r#"{"sort": [{"score": "desc"}]}"#, Kind::Json),
        (r#"{"sort": [{"score": {"order": "up"}}]}"#, Kind::Json),
        (r#"{"sort": [{"score": {"order": null}}]}"#, Kind::Json),
        (r#"{"sort": [{"score": {"missing": "last"}}]}"#, Kind::Json),
        (r#"{"sort": [{"score": {}, "field": "price"}]}"#, Kind::Json),
        (
            r#"{"sort": [{"field": "price", "direction": "asc"}]}"#,
            Kind::Json,
        ),
        (r#"{"sort": [{"order": "asc"}]}"#, Kind::Json),
        (r#"{"sort": [{"field": 5}]}"#, Kind::Json),
        (
            r#"{"sort": [{"field": "price", "order": "ASC"}]}"#,
            Kind::Json,
        ),
        (r#"{"sort": [{"field": "price", "order": 1}]}"#, Kind::Json),
        (
            r#"{"sort": [{"field": "price", "missing": "middle"}]}"#,
            Kind::Json,
        ),
        (
            r#"{"sort": [{"field": "price", "missing": false}]}"#,
            Kind::Json,
        ),
        // `relax` as a query's `relax` is read (SPEC §7.6).
        (r#"{"relax": 5}"#, Kind::Relax),
        (r#"{"relax": "off"}"#, Kind::Relax),
        (r#"{"relax": {"minimum": 1}}"#, Kind::Relax),
        (r#"{"relax": {"enabled": null}}"#, Kind::Relax),
        (r#"{"relax": {"min_results": 0}}"#, Kind::Relax),
        (r#"{"relax": {"budget_ms": 60001}}"#, Kind::Relax),
        (r#"{"relax": {"levels": ["fuzzy", "prefix"]}}"#, Kind::Relax),
        // Fields and filters that do not fit the schema.
        (r#"{"fields": []}"#, Kind::Query),
        (r#"{"fields": ["nope"]}"#, Kind::Query),
        (r#"{"fields": ["brand"]}"#, Kind::Query),
        (r#"{"fields": ["title", "title^2"]}"#, Kind::Query),
        (r#"{"fields": ["title^0"]}"#, Kind::Query),
        (r#"{"fields": ["title^x"]}"#, Kind::Query),
        (r#"{"filter": {"term": ["nope", "a"]}}"#, Kind::Query),
        (r#"{"filter": {"term": ["price", "a"]}}"#, Kind::Query),
        (r#"{"filter": {"is": ["brand", "a"]}}"#, Kind::Query),
        (r#"{"filter": []}"#, Kind::Query),
        (r#"{"filter": "brand:nike"}"#, Kind::Query),
        (r#"{"filter": {"range": ["price", {}]}}"#, Kind::Query),
        (r#"{"soft_filter": {"term": ["nope", "a"]}}"#, Kind::Query),
        (
            r#"{"soft_filter": {"term": ["brand", "a"], "x": 1}}"#,
            Kind::Query,
        ),
        (r#"{"soft_filter": true}"#, Kind::Query),
        (r#"{"sort": [{"field": "nope"}]}"#, Kind::Query),
        (r#"{"facets": ["nope"]}"#, Kind::Query),
        (r#"{"facets": [{"field": "nope", "size": 3}]}"#, Kind::Query),
        // A sort or a facet the schema does not allow.
        (r#"{"sort": [{"field": "title"}]}"#, Kind::Request),
        (r#"{"sort": [{"field": "notes"}]}"#, Kind::Request),
        (&seventeen_keys, Kind::Request),
        (r#"{"facets": ["tenant"]}"#, Kind::Request),
        (r#"{"facets": ["price"]}"#, Kind::Request),
        (
            r#"{"facets": [{"field": "brand", "size": 0}]}"#,
            Kind::Request,
        ),
        (
            r#"{"facets": [{"field": "brand", "size": 1001}]}"#,
            Kind::Request,
        ),
        (&seventeen_facets, Kind::Request),
        // `search_after` is a token, not a cursor written by hand.
        (r#"{"search_after": "not a token!"}"#, Kind::Cursor),
        (r#"{"search_after": ""}"#, Kind::Cursor),
        (r#"{"search_after": "AAAA"}"#, Kind::Cursor),
    ];
    for (json, want) in cases {
        match parse(json) {
            Err(e) => assert_eq!(kind(&e), *want, "{json}: {e}"),
            Ok(request) => panic!("{json} parsed: {request:?}"),
        }
    }
}

#[test]
fn errors_name_what_is_wrong() {
    let message = |json: &str| parse(json).unwrap_err().to_string();
    assert_eq!(
        message(r#"{"query": "a"}"#),
        r#"request: unknown key "query""#
    );
    assert_eq!(message(r#"{"limit": null}"#), "request: `limit` is null");
    assert_eq!(
        message(r#"{"limit": 1.5}"#),
        "request: `limit` takes an integer of at least 0, found a number with a fraction or exponent"
    );
    assert_eq!(message("[]"), "request: expected an object, found an array");
    assert_eq!(
        message(r#"{"sort": [{"field": "price", "order": "up"}]}"#),
        r#"request: `order` of a `sort` key is "asc" or "desc", found "up""#
    );
    assert_eq!(
        message(r#"{"sort": [{"field": "price", "missing": 3}]}"#),
        r#"request: `missing` of a `sort` key is "last" or "first", found an integer"#
    );
    // A long value is named by its kind, so the message stays short.
    let long = json!({"sort": [{"field": "price", "order": "x".repeat(33)}]}).to_string();
    assert_eq!(
        message(&long),
        r#"request: `order` of a `sort` key is "asc" or "desc", found a string"#
    );
    let short = json!({"sort": [{"field": "price", "order": "x".repeat(32)}]}).to_string();
    assert!(message(&short).ends_with(&format!("found {:?}", "x".repeat(32))));
    assert_eq!(
        message(r#"{"sort": [{"score": {}, "field": "price"}]}"#),
        r#"request: unknown key "field" in a `sort` score key"#
    );
    assert_eq!(
        message(r#"{"facets": [{"size": 3}]}"#),
        "request: a facet needs a `field`"
    );
    assert_eq!(
        message(r#"{"search_after": "AAAA"}"#),
        "search_after: a cursor token of version 0; this build reads version 1"
    );
    // The typed errors of each part come through as they are.
    assert!(matches!(
        parse(r#"{"fields": ["nope"]}"#),
        Err(QueryRequestError::Query(QueryError::UnknownField { field })) if field == "nope"
    ));
    assert!(matches!(
        parse(r#"{"sort": [{"field": "nope"}]}"#),
        Err(QueryRequestError::Query(QueryError::UnknownField { field })) if field == "nope"
    ));
    assert!(matches!(
        parse(r#"{"relax": {"min_results": 0}}"#),
        Err(QueryRequestError::Relax(RelaxError::OutOfRange {
            option: "min_results",
            ..
        }))
    ));
    assert!(matches!(
        parse(r#"{"sort": [{"field": "title"}]}"#),
        Err(QueryRequestError::Request(RequestError::SortField { .. }))
    ));
    assert!(matches!(
        parse(r#"{"facets": [{"field": "brand", "size": 0}]}"#),
        Err(QueryRequestError::Request(RequestError::FacetSize {
            size: 0,
            ..
        }))
    ));
    assert!(matches!(
        parse(&json!({ "facets": vec!["brand"; 17] }).to_string()),
        Err(QueryRequestError::Request(RequestError::TooManyFacets {
            count: 17,
            max: 16
        }))
    ));
    assert!(matches!(
        parse(r#"{"search_after": "not a token!"}"#),
        Err(QueryRequestError::Cursor(CursorTokenError::Encoding))
    ));
}

#[test]
fn the_request_is_bounded_in_bytes() {
    // Whitespace is JSON too: the bound is on the text.
    let at = format!("{{{}}}", " ".repeat(MAX_REQUEST_JSON_LEN - 2));
    assert_eq!(at.len(), MAX_REQUEST_JSON_LEN);
    assert_eq!(parse(&at).unwrap(), QueryRequest::default());
    let over = format!("{{{}}}", " ".repeat(MAX_REQUEST_JSON_LEN - 1));
    let err = parse(&over).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "request: {} bytes, at most {MAX_REQUEST_JSON_LEN}",
            MAX_REQUEST_JSON_LEN + 1
        )
    );
    assert_eq!(MAX_REQUEST_JSON_LEN, 1 << 20);
    assert_eq!(MAX_CURSOR_TOKEN_LEN, MAX_REQUEST_JSON_LEN);
}

#[test]
fn filters_are_held_to_the_limits_when_parsed() {
    let schema = schema();
    let small = QueryLimits {
        max_terms: 2,
        ..QueryLimits::default()
    };
    let three = r#"{"terms": ["brand", ["a", "b", "c"]]}"#;
    for key in ["filter", "soft_filter"] {
        let json = format!("{{\"{key}\": {three}}}");
        assert!(QueryRequest::from_json(&json, &schema, &limits()).is_ok());
        assert!(matches!(
            QueryRequest::from_json(&json, &schema, &small),
            Err(QueryRequestError::Query(QueryError::TooManyTerms {
                limit: 2
            }))
        ));
    }
}

// --- 3: run -----------------------------------------------------------------------------------------------------------

/// The engine's own search of the parts of `request`, written out: what `run` must return.
fn by_hand(c: &CollectionSnapshot, request: &QueryRequest) -> Result<QueryResults, String> {
    let schema = c.schema();
    let builtin = QueryCompiler::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    let fields = request
        .fields()
        .cloned()
        .unwrap_or_else(|| DefaultFields::all(schema));
    let compiled = builtin
        .compile(request.q(), &fields, &limits())
        .with_filter(request.filter().cloned())
        .with_soft_filter(request.soft_filter().cloned());
    let relax = RelaxPlan::new(schema.relaxation(), request.relax())
        .unwrap()
        .with_limits(limits());
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    if relax.relaxes() {
        views
            .search_relaxed(&compiled, request.page(), &relax, &exec())
            .map(QueryResults::Relaxed)
            .map_err(|e| e.to_string())
    } else {
        let stats = c.search_statistics(&memtable);
        let plan = SearchPlan::with_terms(compiled.query(), schema, &stats, &exec().terms(&views))
            .unwrap();
        views
            .search_with(&plan, request.page(), &exec())
            .map(QueryResults::Plain)
            .map_err(|e| e.to_string())
    }
}

#[test]
fn run_is_the_engines_search_of_the_same_parts() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let requests = [
        json!({}),
        json!({"q": "red"}),
        json!({"q": "red shoes"}),
        json!({"q": "red shoes", "relax": false}),
        json!({"q": "red -hat", "limit": 2, "offset": 1}),
        json!({"q": "shoez"}),
        json!({"q": "purple"}),
        json!({"q": "purple", "relax": false}),
        json!({"q": "runs", "fields": ["body"]}),
        json!({"q": "red", "fields": ["title^5", "body^0.5"], "explain": true}),
        json!({"q": "red", "filter": {"term": ["tenant", "t1"]}}),
        json!({"q": "red", "filter": {"term": ["tenant", "t1"]}, "relax": false, "explain": true}),
        json!({"q": "red", "soft_filter": {"term": ["brand", "nike"]}}),
        json!({"q": "red", "filter": {"term": ["tenant", "t2"]}, "soft_filter": {"term": ["brand", "acme"]},
               "relax": {"filters": "last_resort"}}),
        json!({"q": "shoes", "sort": [{"field": "price", "order": "desc"}], "facets": ["brand"]}),
        json!({"q": "shoes", "sort": [{"field": "price"}], "facets": [{"field": "brand", "size": 1}],
               "relax": false}),
        json!({"q": "red boots", "relax": {"min_results": 4, "fill": true, "levels": ["drop_terms"]}}),
        json!({"q": "title:red brand:nike"}),
    ];
    for json in requests {
        let request = parse(&json.to_string()).unwrap();
        let got = request.run(c, &options()).unwrap();
        assert!(!got.query_truncated, "{json}");
        assert_eq!(got.results, by_hand(c, &request).unwrap(), "{json}");
    }
}

#[test]
fn the_search_is_relaxed_unless_the_settings_run_l0_alone() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let run = |json: J| {
        parse(&json.to_string())
            .unwrap()
            .run(c, &options())
            .unwrap()
            .results
    };

    // By default the collection relaxes: no word matches `purple`, and the page is not empty (SPEC §7.6).
    let found = relaxed(run(json!({"q": "purple"})));
    assert_eq!(found.relaxation.level, RelaxLevel::Fallback);
    assert_eq!(found.hits.len(), DOCS.len());
    assert_eq!(found.relaxation.original.as_deref(), Some("purple"));
    // A typo is corrected, and the response says so: the request went in compiled, with its text.
    let found = relaxed(run(json!({"q": "shoez", "fields": ["title"]})));
    assert_eq!(found.relaxation.level, RelaxLevel::Fuzzy);
    assert_eq!(found.relaxation.effective.as_deref(), Some("shoes"));
    assert_eq!(relaxed_ids(&found).len(), 3);
    // `true` and an object relax too.
    assert!(matches!(
        run(json!({"q": "red", "relax": true})),
        QueryResults::Relaxed(_)
    ));
    assert!(matches!(
        run(json!({"q": "red", "relax": {"min_results": 2}})),
        QueryResults::Relaxed(_)
    ));

    // `relax: false` is the plain search: L0's hits, and maybe none.
    assert_eq!(
        plain(run(json!({"q": "purple", "relax": false})))
            .hits
            .len(),
        0
    );
    let found = plain(run(json!({"q": "red", "relax": {"enabled": false}})));
    assert_eq!(plain_ids(&found).len(), 4);
    // So is a plan that names no level, no fallback and no filter step: it runs L0 alone.
    let alone = json!({"q": "purple", "relax": {"levels": [], "fallback": "none"}});
    assert_eq!(plain(run(alone)).hits.len(), 0);
    // One level is a cascade.
    let one = json!({"q": "purple", "relax": {"levels": ["fuzzy"], "fallback": "none"}});
    assert_eq!(relaxed(run(one)).hits.len(), 0);
}

#[test]
fn the_collections_settings_apply_as_the_request_overrides_them() {
    let disabled = RelaxationConfig {
        enabled: false,
        min_results: 3,
        ..RelaxationConfig::default()
    };
    let db = database(builder(disabled).build().unwrap());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let run = |json: J| {
        QueryRequest::from_json(&json.to_string(), c.schema(), &limits())
            .unwrap()
            .run(c, &options())
            .unwrap()
            .results
    };
    // The collection disables the cascade: a request that says nothing gets the plain search.
    assert_eq!(plain(run(json!({"q": "purple"}))).hits.len(), 0);
    // A request turns it on, and keeps the collection's other settings: `min_results` 3.
    let found = relaxed(run(json!({"q": "red boots", "relax": true})));
    assert!(found.relaxation.relaxed());
    assert!(found.hits.len() >= 3, "{:?}", relaxed_ids(&found));
    // The same query needs no cascade for one hit.
    let found = relaxed(run(
        json!({"q": "boots", "relax": {"enabled": true, "min_results": 1}}),
    ));
    assert_eq!(found.relaxation.level, RelaxLevel::Exact);
    assert_eq!(relaxed_ids(&found), ["e"]);
}

#[test]
fn q_is_compiled_with_the_collections_analyzers() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let run = |json: J| {
        let found = parse(&json.to_string())
            .unwrap()
            .run(c, &options())
            .unwrap();
        plain(found.results)
    };
    // `body` is analyzed with `en`, `title` with `standard`: `runs` is `run` in `body` alone.
    let stemmed = run(json!({"q": "runs", "fields": ["body"], "relax": false}));
    assert_eq!(plain_ids(&stemmed), ["a", "c"]);
    let exact = run(json!({"q": "runs", "fields": ["title"], "relax": false}));
    assert_eq!(exact.hits.len(), 0);
    // Without `fields`, every `text` field is searched.
    assert_eq!(
        plain_ids(&run(json!({"q": "runs", "relax": false}))),
        ["a", "c"]
    );
    // `field:` reaches a keyword field, unanalyzed.
    assert_eq!(
        plain_ids(&run(json!({"q": "brand:acme", "relax": false}))),
        ["b", "d"]
    );
}

#[test]
fn the_compiler_is_the_collections_until_its_schema_changes() {
    let mut db = database(schema());
    let first = db.snapshot();
    let before = std::ptr::from_ref(first.collection("shoes").unwrap().compiler());
    // A write and a commit publish new snapshots: the compiler, and what it analyzed, is shared.
    let doc = Document::new("g").with(id("title"), Value::String("red clogs".into()));
    db.put("shoes", &doc, Durability::Fsync).unwrap();
    db.commit().unwrap();
    let second = db.snapshot();
    assert!(std::ptr::eq(
        before,
        second.collection("shoes").unwrap().compiler()
    ));
    // An evolved schema has a compiler of its own, which knows the new field.
    let evolved = builder(RelaxationConfig::default())
        .field("subtitle", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    db.evolve_collection(&evolved).unwrap();
    let third = db.snapshot();
    let c = third.collection("shoes").unwrap();
    assert!(!std::ptr::eq(before, c.compiler()));
    let subtitle = c.schema().field("subtitle").unwrap().id();
    db.put(
        "shoes",
        &Document::new("h").with(subtitle, Value::String("crimson".into())),
        Durability::Fsync,
    )
    .unwrap();
    let request =
        QueryRequest::from_json(r#"{"q": "crimson", "relax": false}"#, c.schema(), &limits());
    let found = request.unwrap().run(c, &options()).unwrap();
    assert_eq!(plain_ids(&plain(found.results)), ["h"]);
    // The snapshot taken first still compiles with its own schema's.
    assert!(std::ptr::eq(
        before,
        first.collection("shoes").unwrap().compiler()
    ));
}

#[test]
fn the_page_parts_reach_the_search() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let schema = schema();
    let by_price = Sort::new([SortKey::field(id("price"), Order::Asc)], &schema).unwrap();
    let request = QueryRequest::new("red")
        .with_relax(RelaxOptions::disabled())
        .with_sort(by_price)
        .with_offset(1)
        .with_limit(2)
        .with_facets([FacetRequest::new(id("brand"), 5, &schema).unwrap()])
        .with_explain(true)
        .with_projection(Projection::fields(["brand"], &schema).unwrap());
    let found = plain(request.run(c, &options()).unwrap().results);
    // `red` by price: d 20, f 40, a 80, b 120; the page skips one and takes two.
    assert_eq!(plain_ids(&found), ["f", "a"]);
    for hit in &found.hits {
        assert!(hit.explanation.is_some());
        let fields: Vec<FieldId> = hit.document.fields().map(|(f, _)| f).collect();
        assert_eq!(fields, [id("brand")]);
    }
    // Facets count every match, whatever the page.
    let counts: Vec<(&[u8], u64)> = found.facets[0]
        .values
        .iter()
        .map(|v| (&v.value[..], v.count))
        .collect();
    assert_eq!(counts, [(&b"acme"[..], 2), (&b"nike"[..], 2)]);
    // No projection, no explanation, by default.
    let found = plain(
        QueryRequest::new("red")
            .with_relax(RelaxOptions::disabled())
            .run(c, &options())
            .unwrap()
            .results,
    );
    assert!(found.hits.iter().all(|h| h.explanation.is_none()));
    assert!(
        found
            .hits
            .iter()
            .all(|h| h.document.get(id("title")).is_some())
    );
    // A page past the window is the search's error.
    let deep = QueryRequest::new("red").with_offset(9_990).with_limit(11);
    assert!(matches!(
        deep.run(c, &options()),
        Err(QueryRequestError::Request(RequestError::Window {
            offset: 9_990,
            limit: 11,
            ..
        }))
    ));
}

#[test]
fn the_query_and_its_filters_are_held_to_the_limits_together() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let terms = |n: usize| Filter::Terms {
        field: id("brand"),
        values: (0..n)
            .map(|i| FilterValue::Keyword(format!("b{i}")))
            .collect(),
    };
    let small = |max_terms| {
        options().limits(QueryLimits {
            max_terms,
            ..QueryLimits::default()
        })
    };
    // Two words over two `text` fields are 4 terms, the filter 3 more.
    let request = QueryRequest::new("red shoes").with_filter(terms(3));
    assert!(request.run(c, &small(7)).is_ok());
    assert!(matches!(
        request.run(c, &small(6)),
        Err(QueryRequestError::Query(QueryError::TooManyTerms {
            limit: 6
        }))
    ));
    // The soft filter counts as the hard one does.
    let request = QueryRequest::new("red shoes").with_soft_filter(terms(3));
    assert!(request.run(c, &small(7)).is_ok());
    assert!(matches!(
        request.run(c, &small(6)),
        Err(QueryRequestError::Query(QueryError::TooManyTerms {
            limit: 6
        }))
    ));
    // Text past `max_text_len` is cut, and the response says so (SPEC §7.1.1).
    let cut = options().limits(QueryLimits {
        max_text_len: 9,
        ..QueryLimits::default()
    });
    let request = QueryRequest::new("red shoes boots").with_relax(RelaxOptions::disabled());
    let found = request.run(c, &cut).unwrap();
    assert!(found.query_truncated);
    assert_eq!(plain_ids(&plain(found.results)), ["a", "b"]);
    let whole = request.run(c, &options()).unwrap();
    assert!(!whole.query_truncated);
    assert_eq!(plain(whole.results).hits.len(), 0);
}

#[test]
fn a_deadline_or_a_cancellation_is_not_an_invalid_request() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let token = CancelToken::new();
    token.cancel();
    let clock = Arc::new(ManualClock::new(0));
    let past = clock.now();
    for relax in [json!(false), json!(true)] {
        let request = parse(&json!({"q": "red", "relax": relax}).to_string()).unwrap();
        let cancelled = request
            .run(c, &options().cancel(token.clone()))
            .unwrap_err();
        assert_eq!(
            cancelled.interrupt(),
            Some(Interrupt::Cancelled),
            "{cancelled}"
        );
        let late = request
            .run(c, &options().deadline(Arc::clone(&clock) as _, past))
            .unwrap_err();
        assert_eq!(
            late.interrupt(),
            Some(Interrupt::DeadlineExceeded),
            "{late}"
        );
        // A deadline still ahead stops nothing.
        let ahead = past + Duration::from_secs(1);
        assert!(
            request
                .run(c, &options().deadline(Arc::clone(&clock) as _, ahead))
                .is_ok()
        );
    }
    // Any other failure is not an interrupt.
    let deep = QueryRequest::new("red").with_offset(10_000).with_limit(1);
    assert_eq!(deep.run(c, &options()).unwrap_err().interrupt(), None);
    assert_eq!(parse("[]").unwrap_err().interrupt(), None);
}

#[test]
fn a_request_built_for_another_schema_is_refused() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    // Field ids are positions in a schema's fields, in name order (SPEC §6.2.1): here `a`, `b` and `c` are 0, 1 and
    // 2, where this collection has `active` (bool), `at` (datetime) and `body` (text).
    let other = SchemaBuilder::new("other")
        .field("a", FieldKind::I64 { stored: true })
        .field(
            "b",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .field("c", FieldKind::I64 { stored: true })
        .build()
        .unwrap();
    let field = |name: &str| other.field(name).unwrap().id();
    let fields = QueryRequest::new("red").with_fields(DefaultFields::all(&other));
    let err = fields.run(c, &options()).unwrap_err();
    assert!(matches!(&err, QueryRequestError::Schema { collection } if collection == "shoes"));
    assert_eq!(
        err.to_string(),
        "the request was built for another schema than collection \"shoes\" has"
    );
    // A sort key and a facet are checked against this schema again: `c` is here the text field `body`, which has no
    // column, and `b` the datetime `at`, which is no keyword.
    let sort = Sort::new([SortKey::field(field("c"), Order::Asc)], &other).unwrap();
    assert!(matches!(
        QueryRequest::new("red").with_sort(sort).run(c, &options()),
        Err(QueryRequestError::Request(RequestError::SortField { .. }))
    ));
    let facet = FacetRequest::new(field("b"), 3, &other).unwrap();
    assert!(matches!(
        QueryRequest::new("red")
            .with_facets([facet])
            .run(c, &options()),
        Err(QueryRequestError::Request(RequestError::FacetField { .. }))
    ));
    // A filter is checked with the query it joins: `a` is here the bool `active`, which takes no integer.
    let filter = Filter::Term {
        field: field("a"),
        value: FilterValue::I64(1),
    };
    assert!(matches!(
        QueryRequest::new("red")
            .with_filter(filter)
            .run(c, &options()),
        Err(QueryRequestError::Query(_))
    ));
    // A `relax` that breaks its rule, set in code, is found when the request runs.
    let relax = RelaxOptions {
        min_results: Some(0),
        ..RelaxOptions::default()
    };
    assert!(matches!(
        QueryRequest::new("red")
            .with_relax(relax)
            .run(c, &options()),
        Err(QueryRequestError::Relax(RelaxError::OutOfRange { .. }))
    ));
}

// --- 4: hard and soft filters -----------------------------------------------------------------------------------------

#[test]
fn filter_is_hard_and_soft_filter_is_what_last_resort_drops() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let tenant = id("tenant");
    let run = |json: J| {
        relaxed(
            parse(&json.to_string())
                .unwrap()
                .run(c, &options())
                .unwrap()
                .results,
        )
    };
    let tenants = |found: &RelaxedResults| -> Vec<String> {
        found
            .hits
            .iter()
            .map(|h| match h.hit.document.get(tenant) {
                Some(Value::String(t)) => t.clone(),
                other => panic!("{other:?}"),
            })
            .collect()
    };
    // Tenant t2 holds `c` (blue, nike) and `f` (red, nike); no acme. `red` under both filters matches nothing at any
    // level, the fallback included, so the filter step runs: it drops `brand: acme` and keeps `tenant: t2`.
    let both = json!({
        "q": "red",
        "filter": {"term": ["tenant", "t2"]},
        "soft_filter": {"term": ["brand", "acme"]},
        "relax": {"filters": "last_resort"},
    });
    let found = run(both.clone());
    assert_eq!(found.relaxation.level, RelaxLevel::Filters);
    assert!(found.relaxation.filters_dropped);
    assert_eq!(relaxed_ids(&found), ["f"]);
    assert_eq!(found.hits[0].level, RelaxLevel::Filters);
    assert_eq!(tenants(&found), ["t2"]);
    // The request went in compiled from `q` (`RelaxedQuery::from(&Compiled)`): the response has its text.
    assert_eq!(found.relaxation.original.as_deref(), Some("red"));

    // Without the opt-in nothing drops a filter: the soft one filters as the hard one does.
    let mut never = both.clone();
    never["relax"] = json!({});
    let found = run(never);
    assert_eq!(found.hits.len(), 0);
    assert!(!found.relaxation.filters_dropped);

    // The other way round, the tenant is soft: the step drops it, and the hits leave the tenant. That is why
    // tenancy goes in `filter`.
    let swapped = json!({
        "q": "red",
        "filter": {"term": ["brand", "acme"]},
        "soft_filter": {"term": ["tenant", "t2"]},
        "relax": {"filters": "last_resort"},
    });
    let found = run(swapped);
    assert!(found.relaxation.filters_dropped);
    assert_eq!(tenants(&found), ["t1", "t1"]);

    // A filter given as both is hard: there is nothing to drop, so no filter step, and no hit outside it.
    let twice = json!({
        "q": "red",
        "filter": {"and": [{"term": ["tenant", "t2"]}, {"term": ["brand", "acme"]}]},
        "soft_filter": {"and": [{"term": ["tenant", "t2"]}, {"term": ["brand", "acme"]}]},
        "relax": {"filters": "last_resort"},
    });
    let found = run(twice);
    assert_eq!(found.hits.len(), 0);
    assert!(!found.relaxation.filters_dropped);
    assert_ne!(found.relaxation.level, RelaxLevel::Filters);

    // A hard filter alone under `last_resort`: every level keeps it, the fallback too.
    let hard = json!({
        "q": "purple",
        "filter": {"term": ["tenant", "t2"]},
        "relax": {"filters": "last_resort", "fill": true},
    });
    let found = run(hard);
    assert_eq!(found.relaxation.level, RelaxLevel::Fallback);
    assert_eq!(tenants(&found), ["t2", "t2"]);
}

#[test]
fn the_setters_join_filters_as_the_json_does() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let built = QueryRequest::new("red")
        .with_filter(keyword("tenant", "t2"))
        .with_soft_filter(keyword("brand", "acme"))
        .with_relax(RelaxOptions {
            filters: Some(FilterRelaxation::LastResort),
            ..RelaxOptions::default()
        });
    let json = r#"{"q": "red", "filter": {"term": ["tenant", "t2"]}, "soft_filter": {"term": ["brand", "acme"]},
                   "relax": {"filters": "last_resort"}}"#;
    assert_eq!(parse(json).unwrap(), built);
    let found = relaxed(built.run(c, &options()).unwrap().results);
    assert_eq!(relaxed_ids(&found), ["f"]);
    // A later filter replaces the earlier one.
    let replaced = built.clone().with_filter(keyword("tenant", "t1"));
    assert_eq!(replaced.filter(), Some(&keyword("tenant", "t1")));
    let replaced = built.with_soft_filter(keyword("brand", "nike"));
    assert_eq!(replaced.soft_filter(), Some(&keyword("brand", "nike")));
    // Under a plain search both filter alike.
    let plainly = QueryRequest::new("red")
        .with_filter(keyword("tenant", "t1"))
        .with_soft_filter(keyword("brand", "acme"))
        .with_relax(RelaxOptions::disabled());
    assert_eq!(
        plain_ids(&plain(plainly.run(c, &options()).unwrap().results)),
        ["d", "b"]
    );
}

// --- 5: cursor tokens -------------------------------------------------------------------------------------------------

/// Every hit of `request`, one page of `limit` at a time, each page asked for with the token of the last hit.
fn paged(c: &CollectionSnapshot, mut request: J, limit: usize) -> Vec<String> {
    request["limit"] = json!(limit);
    let mut ids = Vec::new();
    loop {
        let parsed = QueryRequest::from_json(&request.to_string(), c.schema(), &limits()).unwrap();
        let (page, last): (Vec<String>, Option<Cursor>) =
            match parsed.run(c, &options()).unwrap().results {
                QueryResults::Relaxed(r) => (
                    r.hits
                        .iter()
                        .map(|h| h.hit.document.id().to_owned())
                        .collect(),
                    r.hits.last().map(persia_engine::db::RelaxedHit::cursor),
                ),
                QueryResults::Plain(r) => (
                    r.hits.iter().map(|h| h.document.id().to_owned()).collect(),
                    r.hits.last().map(persia_engine::db::SearchHit::cursor),
                ),
                other => panic!("{other:?}"),
            };
        let Some(last) = last else {
            return ids;
        };
        assert!(page.len() <= limit);
        ids.extend(page);
        request["search_after"] = json!(last.to_token());
    }
}

#[test]
fn pages_by_token_are_slices_of_one_ranking() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let requests = [
        json!({"q": "red", "relax": false}),
        json!({"q": "red", "relax": false, "sort": [{"field": "price", "order": "desc"}]}),
        json!({"q": "", "relax": false, "sort": [{"field": "brand"}, "score"]}),
        // Relaxed: the token carries the hit's step and its search's settings.
        json!({"q": "red shoes"}),
        json!({"q": "red boots", "relax": {"min_results": 5, "fill": true}}),
        json!({"q": "purple", "sort": [{"field": "price"}]}),
        json!({"q": "red", "filter": {"term": ["tenant", "t2"]}, "soft_filter": {"term": ["brand", "acme"]},
               "relax": {"filters": "last_resort"}}),
    ];
    for request in requests {
        let whole = paged(c, request.clone(), 100);
        assert!(!whole.is_empty(), "{request}");
        for limit in [1, 2, 3] {
            assert_eq!(
                paged(c, request.clone(), limit),
                whole,
                "{request} by {limit}"
            );
        }
    }
}

#[test]
fn a_token_pages_only_the_search_that_made_it() {
    let schema = schema();
    let mut db = database(schema.clone());
    let other = SchemaBuilder::new("hats")
        .field("title", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    db.create_collection(other).unwrap();
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let hats = snapshot.collection("hats").unwrap();
    let first = parse(r#"{"q": "red", "relax": false, "limit": 1}"#).unwrap();
    let found = plain(first.run(c, &options()).unwrap().results);
    let token = found.hits[0].cursor().to_token();
    let after = |extra: J| {
        let mut request = json!({"q": "red", "relax": false, "limit": 1, "search_after": token});
        for (key, value) in extra.as_object().unwrap() {
            request[key] = value.clone();
        }
        request.to_string()
    };
    // The same search pages on.
    let next = parse(&after(json!({}))).unwrap();
    let second = plain(next.run(c, &options()).unwrap().results);
    assert_eq!(second.hits.len(), 1);
    assert_ne!(second.hits[0].document.id(), found.hits[0].document.id());
    // Another collection of the same snapshot: the token is foreign there.
    let foreign = QueryRequest::from_json(&after(json!({})), hats.schema(), &limits()).unwrap();
    assert!(matches!(
        foreign.run(hats, &options()),
        Err(QueryRequestError::Request(RequestError::CursorView { .. }))
    ));
    // Another sort than the token's values are of.
    let sorted = parse(&after(json!({"sort": [{"field": "price"}]}))).unwrap();
    assert!(matches!(
        sorted.run(c, &options()),
        Err(QueryRequestError::Request(RequestError::Cursor { .. }))
    ));
    // Other relaxation settings: the plain search's token under a cascade.
    let relaxing = parse(&after(json!({"relax": true}))).unwrap();
    assert!(matches!(
        relaxing.run(c, &options()),
        Err(QueryRequestError::Request(RequestError::CursorRelax { .. }))
    ));
    // Another snapshot: a write moves the memtable view on.
    db.put(
        "shoes",
        &Document::new("z").with(id("title"), Value::String("red".into())),
        Durability::Fsync,
    )
    .unwrap();
    let later = db.snapshot();
    assert!(matches!(
        next.run(later.collection("shoes").unwrap(), &options()),
        Err(QueryRequestError::Request(RequestError::CursorView { .. }))
    ));
    // A token that was changed or cut never parses to another cursor.
    let mut changed = token.clone().into_bytes();
    let middle = changed.len() / 2;
    changed[middle] = if changed[middle] == b'A' { b'B' } else { b'A' };
    let changed = String::from_utf8(changed).unwrap();
    for bad in [
        changed.as_str(),
        token.get(..token.len() - 1).unwrap(),
        token.get(..token.len() - 4).unwrap(),
    ] {
        let json = json!({"q": "red", "search_after": bad}).to_string();
        assert!(
            matches!(parse(&json), Err(QueryRequestError::Cursor(_))),
            "{bad}"
        );
    }
    drop(schema);
}

// --- properties -------------------------------------------------------------------------------------------------------

fn runner(cases: u32) -> TestRunner {
    let config = ProptestConfig {
        cases,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    };
    TestRunner::new(config)
}

/// A filter as the test's model holds it: it writes its own JSON and builds its own [`Filter`].
#[derive(Debug, Clone)]
enum F {
    Brand(String),
    Brands(Vec<String>),
    Stock(i64),
    Active(bool),
    /// `price` within bounds: `(inclusive, value)` per side, the lower at most the upper.
    Price(Option<(bool, i32)>, Option<(bool, i32)>),
    At(i64),
    Exists(&'static str),
    And(Vec<F>),
    Or(Vec<F>),
    Not(Box<F>),
}

impl F {
    fn json(&self) -> J {
        match self {
            Self::Brand(b) => json!({"term": ["brand", b]}),
            Self::Brands(all) => json!({"terms": ["brand", all]}),
            Self::Stock(n) => json!({"term": ["stock", n]}),
            Self::Active(b) => json!({"term": ["active", b]}),
            Self::Price(lower, upper) => {
                let mut bounds = serde_json::Map::new();
                if let Some((inclusive, v)) = lower {
                    bounds.insert(if *inclusive { "gte" } else { "gt" }.into(), json!(v));
                }
                if let Some((inclusive, v)) = upper {
                    bounds.insert(if *inclusive { "lte" } else { "lt" }.into(), json!(v));
                }
                json!({"range": ["price", bounds]})
            }
            Self::At(micros) => json!({"range": ["at", {"gte": micros}]}),
            Self::Exists(field) => json!({"exists": field}),
            Self::And(all) => json!({"and": all.iter().map(Self::json).collect::<Vec<_>>()}),
            Self::Or(all) => json!({"or": all.iter().map(Self::json).collect::<Vec<_>>()}),
            Self::Not(inner) => json!({"not": inner.json()}),
        }
    }

    fn typed(&self) -> Filter {
        let bound = |side: &Option<(bool, i32)>| match side {
            None => Bound::Unbounded,
            Some((true, v)) => Bound::Included(FilterValue::F64(f64::from(*v))),
            Some((false, v)) => Bound::Excluded(FilterValue::F64(f64::from(*v))),
        };
        match self {
            Self::Brand(b) => keyword("brand", b),
            Self::Brands(all) => Filter::Terms {
                field: id("brand"),
                values: all.iter().cloned().map(FilterValue::Keyword).collect(),
            },
            Self::Stock(n) => Filter::Term {
                field: id("stock"),
                value: FilterValue::I64(*n),
            },
            Self::Active(b) => Filter::Term {
                field: id("active"),
                value: FilterValue::Bool(*b),
            },
            Self::Price(lower, upper) => Filter::Range {
                field: id("price"),
                lower: bound(lower),
                upper: bound(upper),
            },
            Self::At(micros) => Filter::Range {
                field: id("at"),
                lower: Bound::Included(FilterValue::Datetime(*micros)),
                upper: Bound::Unbounded,
            },
            Self::Exists(field) => Filter::Exists { field: id(field) },
            Self::And(all) => Filter::And(all.iter().map(Self::typed).collect()),
            Self::Or(all) => Filter::Or(all.iter().map(Self::typed).collect()),
            Self::Not(inner) => Filter::Not(Box::new(inner.typed())),
        }
    }
}

fn any_filter() -> impl Strategy<Value = F> {
    let side = || proptest::option::of((any::<bool>(), 0..50_i32));
    let leaf = prop_oneof![
        "[a-z]{0,6}".prop_map(F::Brand),
        proptest::collection::vec("[a-zé\"\\\\ ]{0,5}", 0..4).prop_map(F::Brands),
        any::<i64>().prop_map(F::Stock),
        any::<bool>().prop_map(F::Active),
        // A range needs a bound and must not be empty: one side, or both inclusive and in order.
        (side(), side()).prop_filter_map("a range needs a bound", |(a, b)| match (a, b) {
            (None, None) => None,
            (Some((_, x)), Some((_, y))) =>
                Some(F::Price(Some((true, x.min(y))), Some((true, x.max(y))))),
            (a, b) => Some(F::Price(a, b)),
        }),
        (0..4_000_000_000_000_000_i64).prop_map(F::At),
        prop_oneof![Just("title"), Just("brand"), Just("price"), Just("at")].prop_map(F::Exists),
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..3).prop_map(F::And),
            proptest::collection::vec(inner.clone(), 0..3).prop_map(F::Or),
            inner.prop_map(|f| F::Not(Box::new(f))),
        ]
    })
}

/// A sort key as the model holds it: `(field, order, missing)`, each optional part written or left out.
#[derive(Debug, Clone)]
enum Key {
    /// `"score"`.
    Score,
    /// `{"score": {"order"}}`.
    ScoreOrder(Option<bool>),
    Field(&'static str, Option<bool>, Option<bool>),
}

impl Key {
    fn json(&self) -> J {
        let order = |asc: bool| if asc { "asc" } else { "desc" };
        match self {
            Self::Score => json!("score"),
            Self::ScoreOrder(None) => json!({"score": {}}),
            Self::ScoreOrder(Some(asc)) => json!({"score": {"order": order(*asc)}}),
            Self::Field(field, asc, first) => {
                let mut key = serde_json::Map::new();
                key.insert("field".into(), json!(field));
                if let Some(asc) = asc {
                    key.insert("order".into(), json!(order(*asc)));
                }
                if let Some(first) = first {
                    key.insert(
                        "missing".into(),
                        json!(if *first { "first" } else { "last" }),
                    );
                }
                J::Object(key)
            }
        }
    }

    fn typed(&self) -> SortKey {
        let order = |asc: bool| if asc { Order::Asc } else { Order::Desc };
        match self {
            Self::Score => SortKey::score(),
            Self::ScoreOrder(asc) => SortKey::Score {
                order: asc.map_or(Order::Desc, order),
            },
            Self::Field(field, asc, first) => SortKey::Field {
                field: id(field),
                order: asc.map_or(Order::Asc, order),
                missing: match first {
                    Some(true) => Missing::First,
                    _ => Missing::Last,
                },
            },
        }
    }
}

fn any_key() -> impl Strategy<Value = Key> {
    let flag = || proptest::option::of(any::<bool>());
    prop_oneof![
        Just(Key::Score),
        flag().prop_map(Key::ScoreOrder),
        (
            prop_oneof![
                Just("brand"),
                Just("tenant"),
                Just("price"),
                Just("stock"),
                Just("active"),
                Just("at")
            ],
            flag(),
            flag()
        )
            .prop_map(|(f, o, m)| Key::Field(f, o, m)),
    ]
}

/// `relax` as the model holds it.
#[derive(Debug, Clone)]
enum Relax {
    Bool(bool),
    Object {
        enabled: Option<bool>,
        min_results: Option<u32>,
        /// `None`: left out; `Some(None)`: `"default"`; else the levels kept, by index into [`Level::ALL`].
        levels: Option<Option<Vec<bool>>>,
        fallback: Option<bool>,
        fill: Option<bool>,
        last_resort: Option<bool>,
        budget_ms: Option<u32>,
    },
}

impl Relax {
    fn json(&self) -> J {
        match self {
            Self::Bool(b) => json!(b),
            Self::Object {
                enabled,
                min_results,
                levels,
                fallback,
                fill,
                last_resort,
                budget_ms,
            } => {
                let mut o = serde_json::Map::new();
                let names = ["synonyms", "drop_terms", "prefix", "fuzzy"];
                if let Some(v) = enabled {
                    o.insert("enabled".into(), json!(v));
                }
                if let Some(v) = min_results {
                    o.insert("min_results".into(), json!(v));
                }
                match levels {
                    None => {}
                    Some(None) => {
                        o.insert("levels".into(), json!("default"));
                    }
                    Some(Some(kept)) => {
                        let kept: Vec<&str> = names
                            .iter()
                            .zip(kept)
                            .filter_map(|(n, k)| k.then_some(*n))
                            .collect();
                        o.insert("levels".into(), json!(kept));
                    }
                }
                if let Some(v) = fallback {
                    o.insert(
                        "fallback".into(),
                        json!(if *v { "static_rank" } else { "none" }),
                    );
                }
                if let Some(v) = fill {
                    o.insert("fill".into(), json!(v));
                }
                if let Some(v) = last_resort {
                    o.insert(
                        "filters".into(),
                        json!(if *v { "last_resort" } else { "never" }),
                    );
                }
                if let Some(v) = budget_ms {
                    o.insert("budget_ms".into(), json!(v));
                }
                J::Object(o)
            }
        }
    }

    fn typed(&self) -> RelaxOptions {
        match self {
            Self::Bool(enabled) => RelaxOptions {
                enabled: Some(*enabled),
                ..RelaxOptions::default()
            },
            Self::Object {
                enabled,
                min_results,
                levels,
                fallback,
                fill,
                last_resort,
                budget_ms,
            } => RelaxOptions {
                enabled: *enabled,
                min_results: *min_results,
                levels: levels.as_ref().map(|levels| match levels {
                    None => Levels::Default,
                    Some(kept) => Levels::Custom(
                        Level::ALL
                            .into_iter()
                            .zip(kept)
                            .filter_map(|(l, k)| k.then_some(l))
                            .collect(),
                    ),
                }),
                fallback: fallback.map(|v| {
                    if v {
                        Fallback::StaticRank
                    } else {
                        Fallback::None
                    }
                }),
                fill: *fill,
                filters: last_resort.map(|v| {
                    if v {
                        FilterRelaxation::LastResort
                    } else {
                        FilterRelaxation::Never
                    }
                }),
                budget_ms: *budget_ms,
            },
        }
    }
}

fn any_relax() -> impl Strategy<Value = Relax> {
    let flag = || proptest::option::of(any::<bool>());
    prop_oneof![
        any::<bool>().prop_map(Relax::Bool),
        (
            flag(),
            proptest::option::of(1..=10_000_u32),
            proptest::option::of(proptest::option::of(proptest::collection::vec(
                any::<bool>(),
                4
            ))),
            flag(),
            flag(),
            flag(),
            proptest::option::of(1..=60_000_u32),
        )
            .prop_map(
                |(enabled, min_results, levels, fallback, fill, last_resort, budget_ms)| {
                    Relax::Object {
                        enabled,
                        min_results,
                        levels,
                        fallback,
                        fill,
                        last_resort,
                        budget_ms,
                    }
                }
            ),
    ]
}

fn any_score() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(0.0),
        Just(-0.0),
        Just(f64::MAX),
        Just(f64::MIN_POSITIVE),
        0.0..1e6_f64,
        any::<u64>()
            .prop_map(f64::from_bits)
            .prop_filter("a score is not NaN", |s| !s.is_nan()),
    ]
}

fn any_cursor() -> impl Strategy<Value = Cursor> {
    let value = prop_oneof![
        Just(SortValue::Missing),
        any_score().prop_map(SortValue::Score),
        any::<u64>().prop_map(SortValue::Key),
        proptest::collection::vec(any::<u8>(), 0..40).prop_map(SortValue::Keyword),
    ];
    let location = prop_oneof![
        any::<u32>().prop_map(Located::Memtable),
        (any::<usize>(), any::<u32>()).prop_map(|(index, docid)| Located::Segment { index, docid }),
    ];
    let step =
        proptest::option::of(
            (any::<usize>(), any::<usize>(), any::<usize>())
                .prop_map(|(step, last, hit)| StepCursor { step, last, hit }),
        );
    // The settings of any plan that relaxes; `None` is a plan of L0 alone.
    let relax = proptest::option::of(any_relax()).prop_map(|relax| {
        let options = relax.map_or_else(RelaxOptions::default, |r| r.typed());
        RelaxPlan::new(&RelaxationConfig::default(), &options)
            .unwrap()
            .settings()
    });
    (
        proptest::collection::vec(value, 0..6),
        location,
        (any::<u64>(), any::<u64>(), ".{0,24}"),
        step,
        any::<bool>(),
        relax,
    )
        .prop_map(
            |(values, location, (seq, version, collection), relaxed, proximity, relax)| Cursor {
                values,
                location,
                origin: CursorOrigin {
                    seq,
                    version,
                    collection,
                },
                relaxed,
                proximity,
                relax,
            },
        )
}

/// A request as the model holds it: each key written or left out.
#[derive(Debug, Clone)]
struct Model {
    q: Option<String>,
    /// Distinct `text` fields, each with a weight or none.
    fields: Option<Vec<(&'static str, Option<u16>)>>,
    filter: Option<F>,
    soft_filter: Option<F>,
    /// `None`: left out; `Some(None)`: `"score"`; else a list of keys.
    sort: Option<Option<Vec<Key>>>,
    limit: Option<usize>,
    offset: Option<usize>,
    search_after: Option<Cursor>,
    /// Each a name, or an object with a size or without.
    facets: Option<Vec<Option<Option<usize>>>>,
    relax: Option<Relax>,
    explain: Option<bool>,
    proximity: Option<bool>,
}

impl Model {
    fn json(&self) -> String {
        let mut o = serde_json::Map::new();
        if let Some(q) = &self.q {
            o.insert("q".into(), json!(q));
        }
        if let Some(fields) = &self.fields {
            let specs: Vec<String> = fields.iter().map(|(f, w)| spec(f, *w)).collect();
            o.insert("fields".into(), json!(specs));
        }
        if let Some(f) = &self.filter {
            o.insert("filter".into(), f.json());
        }
        if let Some(f) = &self.soft_filter {
            o.insert("soft_filter".into(), f.json());
        }
        match &self.sort {
            None => {}
            Some(None) => {
                o.insert("sort".into(), json!("score"));
            }
            Some(Some(keys)) => {
                o.insert(
                    "sort".into(),
                    json!(keys.iter().map(Key::json).collect::<Vec<_>>()),
                );
            }
        }
        if let Some(n) = self.limit {
            o.insert("limit".into(), json!(n));
        }
        if let Some(n) = self.offset {
            o.insert("offset".into(), json!(n));
        }
        if let Some(cursor) = &self.search_after {
            o.insert("search_after".into(), json!(cursor.to_token()));
        }
        if let Some(facets) = &self.facets {
            let facets: Vec<J> = facets
                .iter()
                .map(|f| match f {
                    None => json!("brand"),
                    Some(None) => json!({"field": "brand"}),
                    Some(Some(size)) => json!({"field": "brand", "size": size}),
                })
                .collect();
            o.insert("facets".into(), json!(facets));
        }
        if let Some(relax) = &self.relax {
            o.insert("relax".into(), relax.json());
        }
        if let Some(explain) = self.explain {
            o.insert("explain".into(), json!(explain));
        }
        if let Some(proximity) = self.proximity {
            o.insert("proximity".into(), json!(proximity));
        }
        J::Object(o).to_string()
    }

    fn typed(&self, schema: &CollectionSchema) -> QueryRequest {
        let mut r = QueryRequest::new(self.q.clone().unwrap_or_default());
        if let Some(fields) = &self.fields {
            let specs: Vec<String> = fields.iter().map(|(f, w)| spec(f, *w)).collect();
            r = r.with_fields(DefaultFields::parse(schema, &specs).unwrap());
        }
        if let Some(f) = &self.filter {
            r = r.with_filter(f.typed());
        }
        if let Some(f) = &self.soft_filter {
            r = r.with_soft_filter(f.typed());
        }
        if let Some(Some(keys)) = &self.sort {
            r = r.with_sort(Sort::new(keys.iter().map(Key::typed), schema).unwrap());
        }
        if let Some(n) = self.limit {
            r = r.with_limit(n);
        }
        if let Some(n) = self.offset {
            r = r.with_offset(n);
        }
        if let Some(cursor) = &self.search_after {
            r = r.with_search_after(cursor.clone());
        }
        if let Some(facets) = &self.facets {
            r = r.with_facets(facets.iter().map(|f| {
                FacetRequest::new(id("brand"), f.flatten().unwrap_or(10), schema).unwrap()
            }));
        }
        if let Some(relax) = &self.relax {
            r = r.with_relax(relax.typed());
        }
        if let Some(explain) = self.explain {
            r = r.with_explain(explain);
        }
        if let Some(proximity) = self.proximity {
            r = r.with_proximity(proximity);
        }
        r
    }
}

/// A `fields` entry: `name` or `name^weight`.
fn spec(field: &str, weight: Option<u16>) -> String {
    match weight {
        None => field.to_owned(),
        // 0.25 to 1000, in quarters: exact in binary, as written.
        Some(w) => format!("{field}^{}", f64::from(w) / 4.0),
    }
}

fn any_model() -> impl Strategy<Value = Model> {
    let weight = || proptest::option::of(1..=4_000_u16);
    let fields = prop_oneof![
        weight().prop_map(|w| vec![("title", w)]),
        weight().prop_map(|w| vec![("body", w)]),
        (weight(), weight()).prop_map(|(a, b)| vec![("body", a), ("title", b)]),
    ];
    let sort = proptest::option::of(proptest::option::of(proptest::collection::vec(
        any_key(),
        0..5,
    )));
    let facets = proptest::collection::vec(
        proptest::option::of(proptest::option::of(1..=1_000_usize)),
        0..4,
    );
    (
        (
            proptest::option::of(".{0,24}"),
            proptest::option::of(fields),
            proptest::option::of(any_filter()),
            proptest::option::of(any_filter()),
            sort,
        ),
        (
            proptest::option::of(0..20_000_usize),
            proptest::option::of(0..20_000_usize),
            proptest::option::of(any_cursor()),
            proptest::option::of(facets),
            proptest::option::of(any_relax()),
            proptest::option::of(any::<bool>()),
            proptest::option::of(any::<bool>()),
        ),
    )
        .prop_map(
            |(
                (q, fields, filter, soft_filter, sort),
                (limit, offset, search_after, facets, relax, explain, proximity),
            )| {
                Model {
                    q,
                    fields,
                    filter,
                    soft_filter,
                    sort,
                    limit,
                    offset,
                    search_after,
                    facets,
                    relax,
                    explain,
                    proximity,
                }
            },
        )
}

/// Invariant 2: whatever a caller builds in code, the JSON that says the same parses to the same request.
#[test]
fn a_built_request_parses_back_from_its_json() {
    let schema = schema();
    // Filters of the model nest up to 4 deep and hold few values: within the default limits.
    runner(512)
        .run(&any_model(), |model| {
            let json = model.json();
            let built = model.typed(&schema);
            match QueryRequest::from_json(&json, &schema, &limits()) {
                Ok(parsed) => {
                    prop_assert_eq!(&parsed, &built, "{}", json);
                    // A cursor's `-0.0` equals `0.0`: the token tells them apart.
                    let token =
                        |r: &QueryRequest| r.page().search_after.as_ref().map(Cursor::to_token);
                    prop_assert_eq!(token(&parsed), token(&built));
                }
                Err(e) => prop_assert!(false, "{json}: {e}"),
            }
            Ok(())
        })
        .unwrap();
}

/// Invariant 1: no text panics the parser, and what parses runs or fails as a value, never by panicking.
#[test]
fn arbitrary_text_never_panics() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let schema = schema();
    // JSON-shaped text over the request's own words, so parsing gets past the first byte.
    let fragment = prop_oneof![
        Just("{"),
        Just("}"),
        Just("["),
        Just("]"),
        Just(":"),
        Just(","),
        Just("\""),
        Just(" "),
        Just("\"q\""),
        Just("\"fields\""),
        Just("\"filter\""),
        Just("\"soft_filter\""),
        Just("\"sort\""),
        Just("\"limit\""),
        Just("\"offset\""),
        Just("\"search_after\""),
        Just("\"facets\""),
        Just("\"relax\""),
        Just("\"explain\""),
        Just("\"proximity\""),
        Just("\"score\""),
        Just("\"field\""),
        Just("\"order\""),
        Just("\"missing\""),
        Just("\"size\""),
        Just("\"term\""),
        Just("\"terms\""),
        Just("\"range\""),
        Just("\"and\""),
        Just("\"not\""),
        Just("\"exists\""),
        Just("\"brand\""),
        Just("\"price\""),
        Just("\"title\""),
        Just("\"asc\""),
        Just("\"first\""),
        Just("\"gte\""),
        Just("\"min_results\""),
        Just("true"),
        Just("false"),
        Just("null"),
        Just("0"),
        Just("7"),
        Just("-1"),
        Just("1.5"),
        Just("1e400"),
        Just("\"red shoes\""),
        Just("\"AQ\""),
    ];
    let shaped = proptest::collection::vec(fragment, 0..24).prop_map(|parts| parts.concat());
    let text = prop_oneof![shaped, ".{0,64}", any_model().prop_map(|m| m.json())];
    runner(2_048)
        .run(&text, |json| {
            if let Ok(request) = QueryRequest::from_json(&json, &schema, &limits()) {
                // A parsed request is a value like one built in code: running it returns, with hits or an error.
                let _ = request.run(c, &options());
            }
            Ok(())
        })
        .unwrap();
}

/// Base64url without padding, written again here: the test does not read the engine's tokens with the engine's codec.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .chain(std::iter::repeat(&0))
            .take(3)
            .fold(0_u32, |n, &b| (n << 8) | u32::from(b));
        for i in 0..=chunk.len() {
            let sextet = usize::try_from((n >> (18 - 6 * i)) & 63).unwrap();
            out.push(char::from(ALPHABET[sextet]));
        }
    }
    out
}

#[test]
fn a_cursor_round_trips_through_its_token() {
    runner(1_024)
        .run(&any_cursor(), |cursor| {
            let token = cursor.to_token();
            prop_assert!(
                token
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            );
            let decoded = Cursor::from_token(&token).unwrap();
            prop_assert_eq!(&decoded, &cursor);
            // Bit for bit: `-0.0` and `0.0` are equal, their tokens are not.
            prop_assert_eq!(decoded.to_token(), token);
            Ok(())
        })
        .unwrap();
}

#[test]
fn arbitrary_tokens_never_panic_and_only_the_canonical_one_decodes() {
    // Arbitrary text, arbitrary bytes as base64url, and arbitrary bodies under the version byte and a matching
    // checksum, which reach every field's reader.
    let summed = proptest::collection::vec(any::<u8>(), 0..80).prop_map(|mut body| {
        body.insert(0, 1);
        let sum = persia_format::checksum::crc32c(&body);
        body.extend(sum.to_le_bytes());
        base64url(&body)
    });
    let damaged = (any_cursor(), any::<usize>(), any::<u8>()).prop_map(|(cursor, at, c)| {
        let mut token = cursor.to_token().into_bytes();
        let at = at % token.len();
        token[at] = b"ABCxyz019-_=+ "[usize::from(c) % 14];
        String::from_utf8(token).unwrap()
    });
    let text = prop_oneof![
        ".{0,48}",
        proptest::collection::vec(any::<u8>(), 0..64).prop_map(|b| base64url(&b)),
        summed,
        damaged,
    ];
    runner(4_096)
        .run(&text, |token| {
            if let Ok(cursor) = Cursor::from_token(&token) {
                prop_assert_eq!(cursor.to_token(), token);
            }
            Ok(())
        })
        .unwrap();
}

/// A token is not authenticated (SPEC §7.4): a client may write any cursor into one. A cursor of this snapshot's own
/// origin, so that it passes the origin check, with anything else in it, makes the search return hits or an error.
#[test]
fn a_forged_cursor_never_panics() {
    let db = database(schema());
    let snapshot = db.snapshot();
    let c = snapshot.collection("shoes").unwrap();
    let origin = CursorOrigin {
        seq: snapshot.seq(),
        version: c.memtable().version(),
        collection: "shoes".into(),
    };
    let requests = [
        json!({"q": "red"}),
        json!({"q": "red", "relax": false}),
        json!({"q": "purple", "relax": {"fill": true, "filters": "last_resort"}, "facets": ["brand"]}),
        json!({"q": "red shoes", "sort": [{"field": "price"}, "score", {"field": "brand", "missing": "first"}]}),
        json!({"q": "", "relax": false, "sort": [{"field": "brand", "order": "desc"}], "explain": true}),
    ];
    // Small steps, segment indexes and docids, so that they land on, just past, and far past the real ones.
    let small = || prop_oneof![0..4_usize, any::<usize>()];
    let strategy = (
        any_cursor(),
        0..requests.len(),
        proptest::option::of((small(), small(), small())),
        prop_oneof![
            (0..8_u32).prop_map(Located::Memtable),
            (small(), 0..8_u32).prop_map(|(index, docid)| Located::Segment { index, docid }),
        ],
    );
    runner(2_048)
        .run(&strategy, |(cursor, request, steps, location)| {
            let forged = Cursor {
                origin: origin.clone(),
                relaxed: steps.map(|(step, last, hit)| StepCursor { step, last, hit }),
                location,
                proximity: false,
                ..cursor
            };
            let mut json = requests[request].clone();
            json["search_after"] = json!(forged.to_token());
            let parsed = QueryRequest::from_json(&json.to_string(), c.schema(), &limits()).unwrap();
            // The cursor's values may not be of the sort's keys, its steps not of the cascade's: each is an error or
            // a page, by the checks every cursor gets.
            let _ = parsed.run(c, &options());
            // And with the settings the search itself records, so the settings check passes too.
            let relax = RelaxPlan::new(c.schema().relaxation(), parsed.relax()).unwrap();
            let matching = Cursor {
                relax: relax.settings(),
                ..forged
            };
            let _ = parsed.with_search_after(matching).run(c, &options());
            Ok(())
        })
        .unwrap();
}
