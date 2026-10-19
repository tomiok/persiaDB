//! Collection schemas: strict JSON, validation, canonical encoding and hash, evolution (SPEC §6.2, ADR-0022).

// Helpers outside #[test] functions are test code too.
#![allow(clippy::unwrap_used, clippy::panic)]

use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, EvolutionError, Fallback, FieldKind, FieldType,
    FilterRelaxation, KeywordOptions, Level, Levels, MAX_FIELDS, NameProblem, RankFn,
    RankingConfig, RelaxationConfig, SchemaBuilder, SchemaError, StaticRank, TextOptions,
};
use proptest::prelude::*;
use serde_json::{Value, json};

/// SPEC §6.2's example, comments removed.
const SPEC_EXAMPLE: &str = r#"{
  "name": "products",
  "fields": {
    "title": { "type": "text", "analyzer": "es", "boost": 3.0, "positions": true, "k1": 1.2, "b": 0.75 },
    "body":  { "type": "text", "analyzer": "es", "boost": 1.0 },
    "brand": { "type": "keyword", "facet": true },
    "price": { "type": "f64" },
    "sales": { "type": "i64" }
  },
  "ranking": { "static_rank": { "field": "sales", "weight": 0.1, "fn": "log1p" } },
  "relaxation": { "enabled": true, "min_results": 1, "levels": "default", "fallback": "static_rank" },
  "synonyms": [["tv","television"]]
}"#;

fn parse(v: &Value) -> Result<CollectionSchema, SchemaError> {
    CollectionSchema::from_json(&v.to_string())
}

fn spec_value() -> Value {
    serde_json::from_str(SPEC_EXAMPLE).unwrap()
}

/// `spec_value()` with `patch` applied at the JSON pointer `path` (`None` removes the key).
fn patched(path: &str, value: Option<Value>) -> Value {
    let mut v = spec_value();
    let (parent, key) = path.rsplit_once('/').unwrap();
    let parent = v.pointer_mut(parent).unwrap().as_object_mut().unwrap();
    match value {
        Some(new) => parent.insert(key.to_owned(), new),
        None => parent.remove(key),
    };
    v
}

fn text() -> FieldKind {
    FieldKind::Text(TextOptions::default())
}

fn json_error(v: &Value) -> String {
    match parse(v) {
        Err(SchemaError::Json(message)) => message,
        other => panic!("expected a JSON error for {v}, got {other:?}"),
    }
}

#[test]
fn spec_example_parses_with_defaults_filled_in() {
    let s = CollectionSchema::from_json(SPEC_EXAMPLE).unwrap();
    assert_eq!(s.name(), "products");
    let names: Vec<_> = s
        .fields()
        .iter()
        .map(|f| (f.id().get(), f.name()))
        .collect();
    assert_eq!(
        names,
        [
            (0, "body"),
            (1, "brand"),
            (2, "price"),
            (3, "sales"),
            (4, "title")
        ]
    );
    let FieldKind::Text(title) = s.field("title").unwrap().kind() else {
        panic!()
    };
    assert_eq!(title.analyzer, AnalyzerRef::new("es", 1).unwrap());
    assert_eq!(
        (
            title.boost,
            title.positions,
            title.k1,
            title.b,
            title.stored
        ),
        (3.0, true, 1.2, 0.75, true)
    );
    let FieldKind::Text(body) = s.field("body").unwrap().kind() else {
        panic!()
    };
    assert!(!body.positions);
    assert_eq!(
        s.field("brand").unwrap().kind(),
        &FieldKind::Keyword(KeywordOptions {
            facet: true,
            stored: true
        })
    );
    assert_eq!(s.field("price").unwrap().field_type(), FieldType::F64);
    let rank = s.ranking().static_rank.as_ref().unwrap();
    assert_eq!(
        (rank.field.as_str(), rank.weight, rank.function),
        ("sales", 0.1, RankFn::Log1p)
    );
    assert_eq!(s.relaxation(), &RelaxationConfig::default());
    assert_eq!(
        s.synonyms(),
        [vec!["tv".to_owned(), "television".to_owned()]]
    );
    assert!(s.strict());
}

#[test]
fn minimal_definition_gets_every_default() {
    let s = parse(&json!({"name": "a", "fields": {"t": {"type": "text"}}})).unwrap();
    assert_eq!(s.field("t").unwrap().kind(), &text());
    assert_eq!(s.ranking(), &RankingConfig::default());
    assert_eq!(s.relaxation(), &RelaxationConfig::default());
    assert!(s.synonyms().is_empty() && s.strict());
    let empty = parse(&json!({"name": "a", "fields": {}})).unwrap();
    assert_eq!(empty.fields(), []);
}

#[test]
fn unknown_keys_are_rejected_at_every_level() {
    for path in [
        "/extra",
        "/fields/title/postions",
        "/fields/brand/boost",
        "/fields/price/facet",
        "/fields/price/analyzer",
        "/ranking/extra",
        "/ranking/static_rank/extra",
        "/relaxation/extra",
    ] {
        let message = json_error(&patched(path, Some(json!(true))));
        assert!(message.contains("unknown field"), "{path}: {message}");
    }
    // Options of no type at all on `stored`.
    let v = json!({"name": "a", "fields": {"s": {"type": "stored", "stored": true}}});
    assert!(json_error(&v).contains("unknown field"));
}

#[test]
fn duplicate_keys_are_rejected_at_every_level() {
    let cases = [
        r#"{"name":"a","name":"b","fields":{}}"#,
        r#"{"name":"a","fields":{"t":{"type":"text"},"t":{"type":"text"}}}"#,
        r#"{"name":"a","fields":{"t":{"type":"text","boost":1,"boost":2}}}"#,
        r#"{"name":"a","fields":{"t":{"type":"text","type":"keyword"}}}"#,
        r#"{"name":"a","fields":{},"relaxation":{"fill":true,"fill":false}}"#,
        r#"{"name":"a","fields":{},"strict":true,"strict":false}"#,
    ];
    for case in cases {
        match CollectionSchema::from_json(case) {
            Err(SchemaError::Json(m)) => assert!(m.contains("duplicate"), "{case}: {m}"),
            other => panic!("{case}: {other:?}"),
        }
    }
}

#[test]
fn malformed_values_are_json_errors() {
    for (path, value) in [
        ("/fields/title/boost", Some(Value::Null)),
        ("/fields/title/type", Some(json!("string"))),
        ("/fields/title/type", None),
        ("/fields/title/boost", Some(json!("3"))),
        ("/fields/title/id", Some(json!(-1))),
        ("/relaxation/min_results", Some(json!(1.5))),
        ("/relaxation/min_results", Some(json!(5_000_000_000_u64))),
        ("/relaxation/levels", Some(json!("all"))),
        ("/relaxation/levels", Some(json!(["exact"]))),
        ("/relaxation/fallback", Some(json!("recency"))),
        ("/relaxation/filters", Some(json!("always"))),
        ("/ranking/static_rank/fn", Some(json!("log"))),
        ("/ranking/static_rank/field", None),
        ("/synonyms", Some(json!([["a", 1]]))),
        ("/strict", Some(Value::Null)),
        ("/name", None),
        ("/fields", None),
        ("/fields", Some(json!([]))),
    ] {
        json_error(&patched(path, value));
    }
    for text in ["", "{", "[]", "null", r#"{"name":"a","fields":{}} x"#] {
        assert!(
            matches!(CollectionSchema::from_json(text), Err(SchemaError::Json(_))),
            "{text:?}"
        );
    }
}

#[test]
fn collection_names() {
    let ok = ["a", "products", "a-b_c9", &"a".repeat(64)];
    for name in ok {
        parse(&json!({"name": name, "fields": {}})).unwrap();
    }
    let bad = [
        ("", NameProblem::Empty),
        (&"a".repeat(65), NameProblem::TooLong { len: 65 }),
        (
            "Products",
            NameProblem::InvalidChars {
                pattern: "[a-z][a-z0-9_-]*",
            },
        ),
        (
            "9a",
            NameProblem::InvalidChars {
                pattern: "[a-z][a-z0-9_-]*",
            },
        ),
        (
            "-a",
            NameProblem::InvalidChars {
                pattern: "[a-z][a-z0-9_-]*",
            },
        ),
        (
            "a/b",
            NameProblem::InvalidChars {
                pattern: "[a-z][a-z0-9_-]*",
            },
        ),
        (
            "a.b",
            NameProblem::InvalidChars {
                pattern: "[a-z][a-z0-9_-]*",
            },
        ),
        (
            "é",
            NameProblem::InvalidChars {
                pattern: "[a-z][a-z0-9_-]*",
            },
        ),
    ];
    for (name, problem) in bad {
        assert_eq!(
            parse(&json!({"name": name, "fields": {}})),
            Err(SchemaError::CollectionName {
                name: name.to_owned(),
                problem
            }),
        );
    }
}

#[test]
fn field_names() {
    for name in [
        "a",
        "Title",
        "created_at",
        "x9",
        "ids",
        "blobs",
        &"a".repeat(64),
    ] {
        SchemaBuilder::new("c").field(name, text()).build().unwrap();
    }
    let chars = NameProblem::InvalidChars {
        pattern: "[A-Za-z][A-Za-z0-9_]*",
    };
    let bad = [
        ("", NameProblem::Empty),
        (&"a".repeat(65), NameProblem::TooLong { len: 65 }),
        ("id", NameProblem::Reserved),
        ("ID", NameProblem::Reserved),
        ("Blob", NameProblem::Reserved),
        ("_score", NameProblem::Reserved),
        ("_", NameProblem::Reserved),
        ("9a", chars),
        ("a-b", chars),
        ("a.b", chars),
        ("a b", chars),
        ("título", chars),
    ];
    for (name, problem) in bad {
        assert_eq!(
            SchemaBuilder::new("c").field(name, text()).build(),
            Err(SchemaError::FieldName {
                name: name.to_owned(),
                problem
            }),
        );
    }
    assert_eq!(
        SchemaBuilder::new("c")
            .field("a", text())
            .field("a", FieldKind::Stored)
            .build(),
        Err(SchemaError::DuplicateField {
            name: "a".to_owned()
        })
    );
}

#[test]
fn field_count_limit() {
    let build = |n: usize| {
        (0..n)
            .fold(SchemaBuilder::new("c"), |b, i| {
                b.field(format!("f{i}"), FieldKind::Stored)
            })
            .build()
    };
    assert_eq!(build(MAX_FIELDS).unwrap().fields().len(), MAX_FIELDS);
    assert_eq!(
        build(MAX_FIELDS + 1),
        Err(SchemaError::TooManyFields {
            count: MAX_FIELDS + 1
        })
    );
}

#[test]
fn numeric_ranges() {
    let at = |path: &str, v: Value| parse(&patched(path, Some(v)));
    let out_of_range = |path: &str, v: Value| match at(path, v.clone()) {
        Err(SchemaError::OutOfRange { .. }) => {}
        other => panic!("{path} = {v}: {other:?}"),
    };
    for (path, ok, bad) in [
        (
            "/fields/title/boost",
            vec![json!(1e-9), json!(1000)],
            vec![json!(0), json!(-1), json!(1000.0001)],
        ),
        (
            "/fields/title/k1",
            vec![json!(0), json!(10)],
            vec![json!(-0.1), json!(10.5)],
        ),
        (
            "/fields/title/b",
            vec![json!(0), json!(1)],
            vec![json!(-0.01), json!(1.01)],
        ),
        (
            "/ranking/static_rank/weight",
            vec![json!(0), json!(1000)],
            vec![json!(-1), json!(1001)],
        ),
        (
            "/relaxation/min_results",
            vec![json!(1), json!(10_000)],
            vec![json!(0), json!(10_001)],
        ),
        (
            "/relaxation/budget_ms",
            vec![json!(1), json!(60_000)],
            vec![json!(0), json!(60_001)],
        ),
    ] {
        for v in ok {
            at(path, v.clone()).unwrap_or_else(|e| panic!("{path} = {v}: {e}"));
        }
        for v in bad {
            out_of_range(path, v);
        }
    }
    // Non-finite values cannot come from JSON, but can from code.
    for boost in [f64::NAN, f64::INFINITY] {
        let kind = FieldKind::Text(TextOptions {
            boost,
            ..TextOptions::default()
        });
        let err = SchemaBuilder::new("c")
            .field("t", kind)
            .build()
            .unwrap_err();
        assert!(
            matches!(err, SchemaError::OutOfRange { ref option, .. } if option == "fields.t.boost"),
            "{err}"
        );
    }
}

#[test]
fn negative_zero_reads_as_zero() {
    let s = parse(&patched("/fields/title/k1", Some(json!(-0.0)))).unwrap();
    let FieldKind::Text(o) = s.field("title").unwrap().kind() else {
        panic!()
    };
    assert!(o.k1 == 0.0 && o.k1.is_sign_positive());
    let s = parse(&patched("/ranking/static_rank/weight", Some(json!(-0.0)))).unwrap();
    assert!(
        s.ranking()
            .static_rank
            .as_ref()
            .unwrap()
            .weight
            .is_sign_positive()
    );
}

#[test]
fn analyzers() {
    for name in [
        "standard",
        "es",
        "es@1",
        "keyword",
        "whitespace",
        "en",
        "pt",
        "fr",
        "de",
        "it",
    ] {
        parse(&patched("/fields/title/analyzer", Some(json!(name)))).unwrap();
    }
    for name in ["", "ES", "klingon", "es@2", "es@01", "es@"] {
        assert_eq!(
            parse(&patched("/fields/title/analyzer", Some(json!(name)))),
            Err(SchemaError::UnknownAnalyzer {
                field: "title".to_owned(),
                analyzer: name.to_owned()
            })
        );
    }
}

#[test]
fn field_ids() {
    let explicit = json!({"name": "a", "fields": {
        "z": {"type": "text", "id": 0}, "a": {"type": "i64", "id": 2}, "m": {"type": "stored", "id": 1}}});
    let s = parse(&explicit).unwrap();
    let order: Vec<_> = s
        .fields()
        .iter()
        .map(persia_engine::schema::Field::name)
        .collect();
    assert_eq!(order, ["z", "m", "a"]);
    assert_eq!(
        s.field_by_id(s.field("a").unwrap().id()).unwrap().name(),
        "a"
    );
    for fields in [
        json!({"z": {"type": "text", "id": 0}, "a": {"type": "i64"}}),
        json!({"z": {"type": "text", "id": 0}, "a": {"type": "i64", "id": 2}}),
        json!({"z": {"type": "text", "id": 1}, "a": {"type": "i64", "id": 1}}),
        json!({"z": {"type": "text", "id": 1}}),
    ] {
        let err = parse(&json!({"name": "a", "fields": fields})).unwrap_err();
        assert!(
            matches!(err, SchemaError::FieldIds { .. }),
            "{fields}: {err}"
        );
    }
}

#[test]
fn static_rank_field_must_be_a_numeric_or_datetime_field() {
    for (field, ok) in [
        ("sales", true),
        ("price", true),
        ("title", false),
        ("brand", false),
        ("nope", false),
    ] {
        let r = parse(&patched("/ranking/static_rank/field", Some(json!(field))));
        assert_eq!(r.is_ok(), ok, "{field}: {r:?}");
        if !ok {
            assert!(matches!(r, Err(SchemaError::StaticRankField { .. })));
        }
    }
    let s = SchemaBuilder::new("c")
        .field("at", FieldKind::Datetime { stored: true })
        .ranking(RankingConfig {
            static_rank: Some(StaticRank::new("at")),
        })
        .build()
        .unwrap();
    assert_eq!(
        s.ranking().static_rank.as_ref().unwrap().function,
        RankFn::Log1p
    );
    let empty_ranking = parse(&patched("/ranking", Some(json!({})))).unwrap();
    assert_eq!(empty_ranking.ranking().static_rank, None);
}

#[test]
fn relaxation_options() {
    let v = patched(
        "/relaxation",
        Some(
            json!({"enabled": false, "min_results": 5, "levels": ["synonyms", "fuzzy"], "fallback": "none",
                    "fill": true, "filters": "last_resort", "budget_ms": 200}),
        ),
    );
    let r = parse(&v).unwrap().relaxation().clone();
    assert_eq!(
        r,
        RelaxationConfig {
            enabled: false,
            min_results: 5,
            levels: Levels::Custom(vec![Level::Synonyms, Level::Fuzzy]),
            fallback: Fallback::None,
            fill: true,
            filters: FilterRelaxation::LastResort,
            budget_ms: 200,
        }
    );
    let empty = parse(&patched("/relaxation/levels", Some(json!([])))).unwrap();
    assert_eq!(empty.relaxation().levels.resolve(), []);
    for levels in [json!(["fuzzy", "prefix"]), json!(["prefix", "prefix"])] {
        let r = parse(&patched("/relaxation/levels", Some(levels.clone())));
        assert!(
            matches!(r, Err(SchemaError::Levels { .. })),
            "{levels}: {r:?}"
        );
    }
}

#[test]
fn synonym_limits() {
    let at = |s: Value| parse(&patched("/synonyms", Some(s)));
    at(json!([])).unwrap();
    at(json!([["smart tv", "television", "tele"]])).unwrap();
    at(json!([vec!["x".repeat(256), "y".to_owned()]])).unwrap();
    let max_group: Vec<String> = (0..64).map(|i| format!("t{i}")).collect();
    at(json!([max_group])).unwrap();
    let too_big: Vec<String> = (0..65).map(|i| format!("t{i}")).collect();
    for bad in [
        json!([["tv"]]),
        json!([[]]),
        json!([too_big]),
        json!([["tv", "tv"]]),
        json!([["tv", ""]]),
        json!([["tv", " \t"]]),
        json!([vec!["x".repeat(257), "y".to_owned()]]),
    ] {
        assert!(
            matches!(at(bad.clone()), Err(SchemaError::Synonyms { group: 0, .. })),
            "{bad}"
        );
    }
    let groups = vec![json!(["a", "b"]); 10_001];
    assert!(matches!(
        at(Value::Array(groups)),
        Err(SchemaError::Synonyms { group: 10_001, .. })
    ));
}

#[test]
fn to_json_writes_every_option() {
    let s = parse(
        &json!({"name": "a", "fields": {"t": {"type": "text"}, "k": {"type": "keyword"},
        "n": {"type": "i64"}, "s": {"type": "stored"}}}),
    )
    .unwrap();
    let out: Value = serde_json::from_str(&s.to_json().unwrap()).unwrap();
    assert_eq!(
        out,
        json!({
            "name": "a",
            "fields": {
                "k": {"type": "keyword", "id": 0, "facet": false, "stored": true},
                "n": {"type": "i64", "id": 1, "stored": true},
                "s": {"type": "stored", "id": 2},
                "t": {"type": "text", "id": 3, "analyzer": "standard@1", "boost": 1.0, "positions": false,
                      "k1": 1.2, "b": 0.75, "stored": true}
            },
            "relaxation": {"enabled": true, "min_results": 1, "levels": "default", "fallback": "static_rank",
                           "fill": false, "filters": "never", "budget_ms": 50},
            "synonyms": [],
            "strict": true
        })
    );
    assert_eq!(
        CollectionSchema::from_json(&s.to_json().unwrap()).unwrap(),
        s
    );
}

/// Golden vectors (SPEC §6.2.5): these bytes and hashes are persisted in segments and must never change.
#[test]
fn canonical_encoding_golden() {
    let s = SchemaBuilder::new("c")
        .field(
            "t",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field(
            "k",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                stored: false,
            }),
        )
        .field("s", FieldKind::Stored)
        .field("n", FieldKind::I64 { stored: true })
        .build()
        .unwrap();
    #[rustfmt::skip]
    let expected: &[u8] = &[
        1,                                   // encoding_version
        1, b'c',                             // name
        4,                                   // field_count
        1, b'k', 2, 2, 3, 1, 4, 0,           // k: keyword, facet, !stored
        1, b'n', 3, 1, 4, 1,                 // n: i64, stored
        1, b's', 7, 0,                       // s: stored
        1, b't', 1, 3,                       // t: text, 3 options
        1, 8, b's', b't', b'a', b'n', b'd', b'a', b'r', b'd', 1, // analyzer standard@1
        2, 1,                                // positions
        4, 1,                                // stored
    ];
    assert_eq!(s.canonical_index_definition(), expected);
    assert_eq!(s.hash().to_string(), GOLDEN_SMALL);
    assert_eq!(
        CollectionSchema::from_json(SPEC_EXAMPLE)
            .unwrap()
            .hash()
            .to_string(),
        GOLDEN_SPEC_EXAMPLE
    );
}

const GOLDEN_SMALL: &str = "0706f435496649b303ccb69d6a3bb1a8";
const GOLDEN_SPEC_EXAMPLE: &str = "29425d3be8aa36bd85f8f15cdf734469";

#[test]
fn hash_ignores_query_time_settings() {
    let base = CollectionSchema::from_json(SPEC_EXAMPLE).unwrap();
    for (path, value) in [
        ("/fields/title/boost", json!(9)),
        ("/fields/title/k1", json!(2)),
        ("/fields/title/b", json!(0)),
        ("/ranking", json!({})),
        ("/relaxation", json!({"enabled": false})),
        ("/synonyms", json!([])),
        ("/strict", json!(false)),
    ] {
        let other = parse(&patched(path, Some(value))).unwrap();
        assert_ne!(other, base, "{path}");
        assert_eq!(other.hash(), base.hash(), "{path}");
    }
}

#[test]
fn hash_covers_the_index_definition() {
    let base = CollectionSchema::from_json(SPEC_EXAMPLE).unwrap();
    for (path, value) in [
        ("/name", Some(json!("product"))),
        ("/fields/title/analyzer", Some(json!("en"))),
        ("/fields/title/positions", Some(json!(false))),
        ("/fields/title/stored", Some(json!(false))),
        ("/fields/brand/facet", Some(json!(false))),
        ("/fields/brand/stored", Some(json!(false))),
        ("/fields/price/stored", Some(json!(false))),
        ("/fields/price/type", Some(json!("datetime"))),
        ("/fields/body", None),
        ("/fields/extra", Some(json!({"type": "stored"}))),
        ("/fields/title", Some(json!({"type": "keyword"}))),
    ] {
        let other = parse(&patched(path, value)).unwrap();
        assert_ne!(other.hash(), base.hash(), "{path}");
    }
    // Same fields, different ids.
    let ids = |a: u32, b: u32| {
        parse(&json!({"name": "c", "fields": {"x": {"type": "i64", "id": a}, "y": {"type": "i64", "id": b}}}))
            .unwrap()
            .hash()
    };
    assert_ne!(ids(0, 1), ids(1, 0));
}

fn evolve(current: &Value, proposed: &Value) -> Result<CollectionSchema, EvolutionError> {
    parse(current).unwrap().evolve(&parse(proposed).unwrap())
}

#[test]
fn evolution_rules() {
    let current = spec_value();
    let changed = |field: &str, property: &'static str, current: &str, proposed: &str| {
        Err(EvolutionError::FieldChanged {
            field: field.to_owned(),
            property,
            current: current.to_owned(),
            proposed: proposed.to_owned(),
        })
    };
    let cases: Vec<(&str, Option<Value>, Result<(), EvolutionError>)> = vec![
        ("/fields/new", Some(json!({"type": "text"})), Ok(())),
        ("/fields/title/boost", Some(json!(5)), Ok(())),
        ("/fields/title/k1", Some(json!(0.5)), Ok(())),
        ("/fields/title/b", Some(json!(0.1)), Ok(())),
        ("/fields/title/analyzer", Some(json!("es@1")), Ok(())),
        ("/ranking", None, Ok(())),
        ("/relaxation", Some(json!({"min_results": 10})), Ok(())),
        ("/synonyms", Some(json!([["a", "b"]])), Ok(())),
        ("/strict", Some(json!(false)), Ok(())),
        (
            "/name",
            Some(json!("other")),
            Err(EvolutionError::CollectionRenamed {
                current: "products".into(),
                proposed: "other".into(),
            }),
        ),
        (
            "/fields/body",
            None,
            Err(EvolutionError::FieldRemoved {
                field: "body".into(),
            }),
        ),
        (
            "/fields/price/type",
            Some(json!("i64")),
            changed("price", "type", "f64", "i64"),
        ),
        (
            "/fields/title",
            Some(json!({"type": "keyword"})),
            changed("title", "type", "text", "keyword"),
        ),
        (
            "/fields/title/analyzer",
            Some(json!("en")),
            changed("title", "analyzer", "es@1", "en@1"),
        ),
        (
            "/fields/title/positions",
            Some(json!(false)),
            changed("title", "positions", "true", "false"),
        ),
        (
            "/fields/brand/facet",
            Some(json!(false)),
            changed("brand", "facet", "true", "false"),
        ),
        (
            "/fields/brand/stored",
            Some(json!(false)),
            changed("brand", "stored", "true", "false"),
        ),
        (
            "/fields/body/stored",
            Some(json!(false)),
            changed("body", "stored", "true", "false"),
        ),
        (
            "/fields/sales/stored",
            Some(json!(false)),
            changed("sales", "stored", "true", "false"),
        ),
    ];
    for (path, value, expected) in cases {
        let proposed = patched(path, value);
        let result = evolve(&current, &proposed);
        assert_eq!(
            result.as_ref().map(|_| ()).map_err(Clone::clone),
            expected,
            "{path}"
        );
        if let Ok(next) = result {
            let proposed = parse(&proposed).unwrap();
            assert_eq!(next.ranking(), proposed.ranking(), "{path}");
            assert_eq!(next.relaxation(), proposed.relaxation(), "{path}");
            assert_eq!(next.synonyms(), proposed.synonyms(), "{path}");
            assert_eq!(next.strict(), proposed.strict(), "{path}");
            for f in proposed.fields() {
                assert_eq!(next.field(f.name()).unwrap().kind(), f.kind(), "{path}");
            }
        }
    }
}

#[test]
fn evolution_keeps_ids_and_appends_added_fields_by_name() {
    let current = json!({"name": "c", "fields": {"m": {"type": "text"}, "x": {"type": "i64"}}});
    let proposed = json!({"name": "c", "fields": {"a": {"type": "stored"}, "m": {"type": "text", "boost": 2},
        "z": {"type": "bool"}, "b": {"type": "keyword"}, "x": {"type": "i64"}}});
    let next = evolve(&current, &proposed).unwrap();
    let ids: Vec<_> = next
        .fields()
        .iter()
        .map(|f| (f.id().get(), f.name()))
        .collect();
    assert_eq!(ids, [(0, "m"), (1, "x"), (2, "a"), (3, "b"), (4, "z")]);
    // The result is a valid schema: it round-trips and evolves to itself.
    assert_eq!(
        CollectionSchema::from_json(&next.to_json().unwrap()).unwrap(),
        next
    );
    assert_eq!(next.evolve(&next).unwrap(), next);
    // Ids in the proposed definition are ignored.
    let renumbered = json!({"name": "c", "fields": {"m": {"type": "text", "id": 1}, "x": {"type": "i64", "id": 0}}});
    assert_eq!(
        evolve(&current, &renumbered).unwrap(),
        parse(&current).unwrap()
    );
}

#[test]
fn serde_impls_validate() {
    let s: CollectionSchema = serde_json::from_str(SPEC_EXAMPLE).unwrap();
    assert_eq!(s, CollectionSchema::from_json(SPEC_EXAMPLE).unwrap());
    assert_eq!(serde_json::to_string(&s).unwrap(), s.to_json().unwrap());
    let bad = patched("/fields/title/boost", Some(json!(0)));
    let err = serde_json::from_value::<CollectionSchema>(bad).unwrap_err();
    assert!(err.to_string().contains("boost"), "{err}");
}

fn field_kind() -> impl Strategy<Value = FieldKind> {
    let unit = || prop_oneof![Just(0.0), Just(1.0), 0.0..=1.0f64];
    let text = (
        prop::sample::select(persia_engine::schema::BUILTIN_ANALYZERS),
        prop_oneof![Just(1000.0), 1e-6..=1000.0f64],
        any::<bool>(),
        prop_oneof![Just(0.0), Just(10.0), 0.0..=10.0f64],
        unit(),
        any::<bool>(),
    )
        .prop_map(|(analyzer, boost, positions, k1, b, stored)| {
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin(analyzer).unwrap(),
                boost,
                positions,
                k1,
                b,
                stored,
            })
        });
    prop_oneof![
        text,
        (any::<bool>(), any::<bool>())
            .prop_map(|(facet, stored)| FieldKind::Keyword(KeywordOptions { facet, stored })),
        any::<bool>().prop_map(|stored| FieldKind::I64 { stored }),
        any::<bool>().prop_map(|stored| FieldKind::F64 { stored }),
        any::<bool>().prop_map(|stored| FieldKind::Bool { stored }),
        any::<bool>().prop_map(|stored| FieldKind::Datetime { stored }),
        Just(FieldKind::Stored),
    ]
}

fn relaxation() -> impl Strategy<Value = RelaxationConfig> {
    let levels = prop_oneof![
        Just(Levels::Default),
        prop::sample::subsequence(Level::ALL.to_vec(), 0..=4).prop_map(Levels::Custom),
    ];
    (
        any::<bool>(),
        1..=10_000u32,
        levels,
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        1..=60_000u32,
    )
        .prop_map(
            |(enabled, min_results, levels, none, fill, last_resort, budget_ms)| RelaxationConfig {
                enabled,
                min_results,
                levels,
                fallback: if none {
                    Fallback::None
                } else {
                    Fallback::StaticRank
                },
                fill,
                filters: if last_resort {
                    FilterRelaxation::LastResort
                } else {
                    FilterRelaxation::Never
                },
                budget_ms,
            },
        )
}

/// Valid definitions: unique names, a static rank on a numeric field when there is one.
fn schema() -> impl Strategy<Value = (Vec<(String, FieldKind)>, SchemaBuilder)> {
    (
        "[a-z][a-z0-9_-]{0,20}",
        prop::collection::btree_map("[A-Za-z][A-Za-z0-9_]{0,12}", field_kind(), 0..12),
        relaxation(),
        prop::collection::vec(prop::collection::btree_set("[a-z ]{0,6}[a-z]", 2..5), 0..4),
        any::<bool>(),
        any::<bool>(),
        prop_oneof![Just(0.0), Just(1000.0), 0.0..=1000.0f64],
    )
        .prop_filter("reserved names", |(_, fields, ..)| {
            fields
                .keys()
                .all(|n| !n.eq_ignore_ascii_case("id") && !n.eq_ignore_ascii_case("blob"))
        })
        .prop_map(|(name, fields, relax, synonyms, strict, linear, weight)| {
            let fields: Vec<_> = fields.into_iter().collect();
            let rank_field = fields.iter().find(|(_, k)| {
                matches!(
                    k,
                    FieldKind::I64 { .. } | FieldKind::F64 { .. } | FieldKind::Datetime { .. }
                )
            });
            let ranking = RankingConfig {
                static_rank: rank_field.map(|(n, _)| StaticRank {
                    field: n.clone(),
                    weight,
                    function: if linear {
                        RankFn::Linear
                    } else {
                        RankFn::Log1p
                    },
                }),
            };
            let builder = fields
                .iter()
                .fold(SchemaBuilder::new(name), |b, (n, k)| {
                    b.field(n.clone(), k.clone())
                })
                .ranking(ranking)
                .relaxation(relax)
                .synonyms(
                    synonyms
                        .into_iter()
                        .map(|g| g.into_iter().collect())
                        .collect(),
                )
                .strict(strict);
            (fields, builder)
        })
}

proptest! {
    /// JSON round trip is exact and keeps the hash.
    #[test]
    fn json_round_trip((_, builder) in schema()) {
        let s = builder.build().unwrap();
        let back = CollectionSchema::from_json(&s.to_json().unwrap()).unwrap();
        prop_assert_eq!(back.hash(), s.hash());
        prop_assert_eq!(back, s);
    }

    /// Declaration order never matters: same ids, same hash.
    #[test]
    fn declaration_order_is_irrelevant((fields, builder) in schema(), seed in any::<u64>()) {
        let s = builder.clone().build().unwrap();
        let mut shuffled = fields.clone();
        // A seeded rotation plus reversal covers distinct orders without an RNG dependency.
        let len = shuffled.len().max(1);
        shuffled.rotate_left(usize::try_from(seed).unwrap_or(0) % len);
        if seed % 2 == 1 { shuffled.reverse(); }
        let reordered = shuffled
            .into_iter()
            .fold(SchemaBuilder::new(s.name()), |b, (n, k)| b.field(n, k))
            .ranking(s.ranking().clone())
            .relaxation(s.relaxation().clone())
            .synonyms(s.synonyms().to_vec())
            .strict(s.strict())
            .build()
            .unwrap();
        prop_assert_eq!(reordered.hash(), s.hash());
        prop_assert_eq!(reordered, s);
    }

    /// A schema evolves to itself, and to itself plus a new field, which takes the next id.
    #[test]
    fn evolution_is_reflexive_and_appends((fields, builder) in schema(), kind in field_kind()) {
        let s = builder.clone().build().unwrap();
        prop_assert_eq!(&s.evolve(&s).unwrap(), &s);
        prop_assume!(fields.len() < 12);
        let grown = builder.field("Zz_added", kind).build().unwrap();
        let next = s.evolve(&grown).unwrap();
        prop_assert_eq!(next.field("Zz_added").unwrap().id().get(), u32::try_from(fields.len()).unwrap());
        for f in s.fields() {
            prop_assert_eq!(next.field(f.name()).unwrap().id(), f.id());
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Hardening of the strict JSON form (ROADMAP 3.1.2.1) and of the canonical encoding / schema hash (ROADMAP 3.1.2.2).
//
// Invariants (SPEC §6.2.1, §6.2.5, ADR-0022):
//  S1 every key is named: a definition (and each nested object) is a JSON object, never a positional array;
//  S2 unknown keys, duplicate keys (also when spelled with escapes), `null` for an optional key and options foreign
//     to the field's type are errors at every level, wherever the `type` key sits in a field object;
//  S3 values have exactly their JSON type: no strings for numbers/bools, no floats for integers, no out-of-range ints;
//  S4 escapes are decoded before any rule: `"id"` is the reserved `id`;
//  R1 an accepted schema round-trips exactly through `to_json`/`from_json` and through `serde_json` values;
//  H1 the hash ignores JSON key order at every level and declaration order;
//  H2 the hash is XXH3-128 of the canonical bytes, changes with any index-definition property and ignores
//     query-time settings;
//  V1 validation never panics on any input, and `from_json` agrees with the serde impl.
// ---------------------------------------------------------------------------------------------------------------

fn from_str_kind(text: &str) -> Result<CollectionSchema, String> {
    match CollectionSchema::from_json(text) {
        Ok(s) => Ok(s),
        Err(SchemaError::Json(m)) => Err(m),
        Err(other) => panic!("expected a JSON error for {text}, got {other:?}"),
    }
}

/// S1. serde's derived structs (and internally tagged enums) also accept a JSON array as a positional form; a
/// strict, key-based definition must not (SPEC §6.2.1: "an unknown key ... is an error, never ignored" presumes keys).
#[test]
fn positional_array_forms_are_rejected_at_every_level() {
    let cases = [
        // The whole definition.
        r#"["a",{}]"#,
        // A field: the first element is taken as the `type` tag, the rest as options in declaration order.
        r#"{"name":"a","fields":{"t":["text"]}}"#,
        r#"{"name":"a","fields":{"t":["text",0,"es",3.0,true]}}"#,
        r#"{"name":"a","fields":{"t":["stored"]}}"#,
        // Nested settings.
        r#"{"name":"a","fields":{},"ranking":[]}"#,
        r#"{"name":"a","fields":{"t":{"type":"i64"}},"ranking":{"static_rank":["t",2.0,"linear"]}}"#,
        r#"{"name":"a","fields":{},"relaxation":[true,5]}"#,
        r#"{"name":"a","fields":{},"relaxation":[]}"#,
    ];
    for case in cases {
        assert!(
            matches!(CollectionSchema::from_json(case), Err(SchemaError::Json(_))),
            "accepted a positional form: {case} => {:?}",
            CollectionSchema::from_json(case).map(|s| s.to_json().unwrap())
        );
        let value: Value = serde_json::from_str(case).unwrap();
        assert!(
            serde_json::from_value::<CollectionSchema>(value).is_err(),
            "serde impl accepted a positional form: {case}"
        );
    }
}

/// S2. The `type` key may come anywhere in a field object (serde buffers internally tagged content); the rules are
/// the same wherever it sits.
#[test]
fn type_key_position_does_not_change_strictness() {
    let first = from_str_kind(
        r#"{"name":"a","fields":{"t":{"type":"text","boost":2,"positions":true,"analyzer":"es"}}}"#,
    )
    .unwrap();
    for text in [
        r#"{"name":"a","fields":{"t":{"boost":2,"type":"text","positions":true,"analyzer":"es"}}}"#,
        r#"{"name":"a","fields":{"t":{"boost":2,"positions":true,"analyzer":"es","type":"text"}}}"#,
    ] {
        let s = from_str_kind(text).unwrap();
        assert_eq!(s, first, "{text}");
        assert_eq!(s.hash(), first.hash(), "{text}");
    }
    for (text, needle) in [
        // A foreign option before the tag.
        (
            r#"{"name":"a","fields":{"t":{"boost":2,"type":"keyword"}}}"#,
            "unknown field",
        ),
        (
            r#"{"name":"a","fields":{"t":{"facet":true,"type":"i64"}}}"#,
            "unknown field",
        ),
        (
            r#"{"name":"a","fields":{"t":{"id":0,"type":"stored","stored":true}}}"#,
            "unknown field",
        ),
        // An unknown key before the tag.
        (
            r#"{"name":"a","fields":{"t":{"postions":true,"type":"text"}}}"#,
            "unknown field",
        ),
        // Duplicates split by the tag.
        (
            r#"{"name":"a","fields":{"t":{"stored":true,"type":"i64","stored":false}}}"#,
            "duplicate",
        ),
        (
            r#"{"name":"a","fields":{"t":{"id":0,"type":"datetime","id":0}}}"#,
            "duplicate",
        ),
        (
            r#"{"name":"a","fields":{"t":{"type":"i64","stored":true,"type":"i64"}}}"#,
            "duplicate",
        ),
        // A null before the tag.
        (
            r#"{"name":"a","fields":{"t":{"analyzer":null,"type":"text"}}}"#,
            "null",
        ),
        // The tag's value is case-sensitive and must be a string.
        (r#"{"name":"a","fields":{"t":{"type":"Text"}}}"#, "variant"),
        (r#"{"name":"a","fields":{"t":{"type":1}}}"#, ""),
        (r#"{"name":"a","fields":{"t":{"type":null}}}"#, ""),
        (r#"{"name":"a","fields":{"t":{"Type":"text"}}}"#, ""),
        (r#"{"name":"a","fields":{"t":{}}}"#, "type"),
        (r#"{"name":"a","fields":{"t":"text"}}"#, ""),
        (r#"{"name":"a","fields":{"t":null}}"#, ""),
    ] {
        let message = from_str_kind(text).unwrap_err();
        assert!(message.contains(needle), "{text}: {message}");
    }
}

/// S2. `null` is never a stand-in for "absent", for every optional key at every level.
#[test]
fn null_is_rejected_for_every_optional_key() {
    let full = json!({
        "name": "a",
        "fields": {
            "t": {"type": "text", "id": 0, "analyzer": "es", "boost": 1, "positions": true, "k1": 1, "b": 0.5,
                  "stored": true},
            "k": {"type": "keyword", "id": 1, "facet": true, "stored": true},
            "n": {"type": "i64", "id": 2, "stored": true},
            "f": {"type": "f64", "id": 3, "stored": true},
            "o": {"type": "bool", "id": 4, "stored": true},
            "d": {"type": "datetime", "id": 5, "stored": true},
            "s": {"type": "stored", "id": 6}
        },
        "ranking": {"static_rank": {"field": "n", "weight": 1, "fn": "linear"}},
        "relaxation": {"enabled": true, "min_results": 1, "levels": "default", "fallback": "none", "fill": true,
                       "filters": "never", "budget_ms": 5},
        "synonyms": [["a", "b"]],
        "strict": false
    });
    parse(&full).unwrap();
    let mut paths = Vec::new();
    collect_pointers(&full, "", &mut paths);
    let mut checked = 0;
    for path in paths {
        let Some((parent, key)) = path.rsplit_once('/') else {
            continue;
        };
        // Only object members (every key, and every field definition); array elements are not keys.
        if !full.pointer(parent).is_some_and(Value::is_object) {
            continue;
        }
        let mut v = full.clone();
        v.pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.to_owned(), Value::Null);
        let message = json_error(&v);
        // Enums report serde_json's "expected value" for a null; everything else names the null.
        assert!(
            message.contains("null") || message.starts_with("expected value"),
            "{path}: {message}"
        );
        checked += 1;
    }
    // 43 keys at every level plus the 7 field definitions.
    assert_eq!(checked, 50);
}

/// S2. Each option is accepted exactly on the types SPEC §6.2.2 lists it for.
#[test]
fn option_type_matrix() {
    let options = [
        ("analyzer", json!("es"), &["text"][..]),
        ("boost", json!(2), &["text"][..]),
        ("positions", json!(true), &["text"][..]),
        ("k1", json!(1), &["text"][..]),
        ("b", json!(0.5), &["text"][..]),
        ("facet", json!(true), &["keyword"][..]),
        (
            "stored",
            json!(false),
            &["text", "keyword", "i64", "f64", "bool", "datetime"][..],
        ),
        (
            "id",
            json!(0),
            &[
                "text", "keyword", "i64", "f64", "bool", "datetime", "stored",
            ][..],
        ),
    ];
    for ty in [
        "text", "keyword", "i64", "f64", "bool", "datetime", "stored",
    ] {
        for (option, value, applies) in &options {
            let v = json!({"name": "a", "fields": {"t": {"type": ty, *option: value}}});
            if applies.contains(&ty) {
                parse(&v).unwrap_or_else(|e| panic!("{ty}.{option}: {e}"));
            } else {
                let message = json_error(&v);
                assert!(
                    message.contains("unknown field"),
                    "{ty}.{option}: {message}"
                );
            }
        }
    }
}

/// S3. Values must have their JSON type; numbers must fit.
#[test]
fn values_must_have_their_exact_json_type() {
    for (path, value) in [
        // Numbers as strings, bools as numbers or strings.
        ("/fields/title/k1", json!("1.2")),
        ("/fields/title/positions", json!(1)),
        ("/fields/title/positions", json!("true")),
        ("/fields/title/id", json!("0")),
        ("/relaxation/min_results", json!("1")),
        ("/relaxation/enabled", json!(0)),
        ("/strict", json!("false")),
        // Floats for integers, even when integral.
        ("/fields/title/id", json!(0.0)),
        ("/fields/title/id", json!(-0.0)),
        ("/relaxation/budget_ms", json!(50.0)),
        ("/relaxation/min_results", json!(1e2)),
        // Integers outside u32.
        ("/fields/title/id", json!(4_294_967_296_u64)),
        ("/relaxation/budget_ms", json!(u64::MAX)),
        ("/relaxation/budget_ms", json!(i64::MIN)),
        // Wrong containers.
        ("/synonyms", json!(["a", "b"])),
        ("/synonyms", json!({"a": "b"})),
        ("/relaxation/levels", json!({"synonyms": true})),
        ("/relaxation/levels", json!("Default")),
        ("/relaxation/levels", json!(["Synonyms"])),
        ("/ranking/static_rank/fn", json!("LOG1P")),
        ("/name", json!(1)),
        ("/fields/title/analyzer", json!(["es"])),
    ] {
        json_error(&patched(path, Some(value)));
    }
    // Integers are fine for float options and read exactly.
    let s = parse(&patched("/fields/title/boost", Some(json!(7)))).unwrap();
    let FieldKind::Text(o) = s.field("title").unwrap().kind() else {
        panic!()
    };
    assert_eq!(o.boost.to_bits(), 7.0f64.to_bits());
    // Numbers JSON cannot hold as f64 are parse errors, not infinities.
    for text in [
        r#"{"name":"a","fields":{"t":{"type":"text","boost":1e400}}}"#,
        r#"{"name":"a","fields":{"t":{"type":"text","k1":-1e400}}}"#,
    ] {
        from_str_kind(text).unwrap_err();
    }
    // Integers beyond u64 become floats in serde_json; still not a u32.
    from_str_kind(r#"{"name":"a","fields":{"t":{"type":"text","id":18446744073709551616}}}"#)
        .unwrap_err();
    // Huge floats for float options are range errors (SPEC §6.2.2), not parse errors.
    assert!(matches!(
        parse(&patched("/fields/title/boost", Some(json!(f64::MAX)))),
        Err(SchemaError::OutOfRange { .. })
    ));
}

/// S4. Escapes are decoded before names are checked or compared.
#[test]
fn escaped_keys_and_names_are_the_decoded_strings() {
    // Reserved names.
    for (text, name) in [
        (r#"{"name":"a","fields":{"id":{"type":"text"}}}"#, "id"),
        (r#"{"name":"a","fields":{"ID":{"type":"text"}}}"#, "ID"),
        (r#"{"name":"a","fields":{"blob":{"type":"text"}}}"#, "blob"),
        (r#"{"name":"a","fields":{"_x":{"type":"text"}}}"#, "_x"),
    ] {
        assert_eq!(
            CollectionSchema::from_json(text),
            Err(SchemaError::FieldName {
                name: name.to_owned(),
                problem: NameProblem::Reserved
            }),
            "{text}"
        );
    }
    // Duplicates spelled differently.
    for text in [
        r#"{"name":"a","name":"b","fields":{}}"#,
        r#"{"name":"a","fields":{"t":{"type":"text"},"t":{"type":"text"}}}"#,
        r#"{"name":"a","fields":{"t":{"boost":2,"type":"text","boost":3}}}"#,
        r#"{"name":"a","fields":{"t":{"type":"text","type":"text"}}}"#,
        r#"{"name":"a","fields":{},"relaxation":{"fill":true,"fill":true}}"#,
    ] {
        assert!(
            from_str_kind(text).unwrap_err().contains("duplicate"),
            "{text}"
        );
    }
    // Escaped spellings of valid keys and values are those keys and values.
    let escaped =
        from_str_kind(r#"{"name":"a","fields":{"t":{"type":"text","analyzer":"es"}}}"#).unwrap();
    let plain =
        from_str_kind(r#"{"name":"a","fields":{"t":{"type":"text","analyzer":"es"}}}"#).unwrap();
    assert_eq!(escaped, plain);
    assert_eq!(escaped.hash(), plain.hash());
    // A non-ASCII letter that looks like an ASCII one is not that letter.
    assert!(matches!(
        CollectionSchema::from_json(r#"{"name":"a","fields":{"іd":{"type":"text"}}}"#),
        Err(SchemaError::FieldName {
            problem: NameProblem::InvalidChars { .. },
            ..
        })
    ));
    // Lone surrogates are malformed JSON.
    from_str_kind(r#"{"name":"a","fields":{"\ud800":{"type":"text"}}}"#).unwrap_err();
}

/// V1. Deep nesting anywhere (including inside buffered field objects) is an error, not a stack overflow.
#[test]
fn deep_nesting_is_an_error_not_a_crash() {
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    let deep_obj = format!("{}1{}", r#"{"a":"#.repeat(100_000), "}".repeat(100_000));
    for value in [&deep, &deep_obj] {
        for template in [
            r#"{"name":"a","fields":{},"x":V}"#,
            r#"{"name":"a","fields":{"t":{"type":"text","x":V}}}"#,
            r#"{"name":"a","fields":{"t":{"x":V,"type":"text"}}}"#,
            r#"{"name":"a","fields":{},"synonyms":V}"#,
            r#"{"name":"a","fields":{},"relaxation":{"levels":V}}"#,
            r#"{"name":"a","fields":{"t":V}}"#,
            "V",
        ] {
            let text = template.replace('V', value);
            from_str_kind(&text).unwrap_err();
        }
    }
}

/// R1. Extreme but valid floats survive both round trips bit for bit.
#[test]
fn boundary_floats_round_trip_bit_exactly() {
    let floats = [
        5e-324,
        f64::MIN_POSITIVE,
        0.1 + 0.2,
        1.0 - f64::EPSILON / 2.0,
        1000.0,
        999.999_999_999_999_9,
        std::f64::consts::PI,
    ];
    for boost in floats {
        let b = (boost / 1000.0).min(1.0);
        let k1 = boost / 100.0;
        let s = SchemaBuilder::new("c")
            .field(
                "t",
                FieldKind::Text(TextOptions {
                    boost,
                    k1,
                    b,
                    ..TextOptions::default()
                }),
            )
            .field("n", FieldKind::F64 { stored: true })
            .ranking(RankingConfig {
                static_rank: Some(StaticRank {
                    weight: boost,
                    ..StaticRank::new("n")
                }),
            })
            .build()
            .unwrap();
        let via_text = CollectionSchema::from_json(&s.to_json().unwrap()).unwrap();
        let via_value: CollectionSchema =
            serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        for back in [&via_text, &via_value] {
            let FieldKind::Text(o) = back.field("t").unwrap().kind() else {
                panic!()
            };
            assert_eq!(
                (o.boost.to_bits(), o.k1.to_bits(), o.b.to_bits()),
                (boost.to_bits(), k1.to_bits(), b.to_bits()),
                "{boost}"
            );
            let w = back.ranking().static_rank.as_ref().unwrap().weight;
            assert_eq!(w.to_bits(), boost.to_bits(), "{boost}");
            assert_eq!(back.to_json().unwrap(), s.to_json().unwrap());
        }
    }
}

/// H2. Golden bytes for the types and options the first golden vector does not reach, derived by hand from the
/// SPEC §6.2.5 table (not from the encoder's output).
#[test]
fn canonical_encoding_golden_remaining_types() {
    let s = SchemaBuilder::new("g")
        .field(
            "x",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("es").unwrap(),
                boost: 2.0, // query-time: not encoded
                positions: false,
                stored: false,
                ..TextOptions::default()
            }),
        )
        .field("f", FieldKind::F64 { stored: false })
        .field("d", FieldKind::Datetime { stored: true })
        .field("a", FieldKind::Bool { stored: false })
        .build()
        .unwrap();
    #[rustfmt::skip]
    let expected: &[u8] = &[
        1,                         // encoding_version
        1, b'g',                   // name
        4,                         // field_count
        1, b'a', 5, 1, 4, 0,       // a: bool, !stored
        1, b'd', 6, 1, 4, 1,       // d: datetime, stored
        1, b'f', 4, 1, 4, 0,       // f: f64, !stored
        1, b'x', 1, 3,             // x: text, 3 options
        1, 2, b'e', b's', 1,       // analyzer es@1
        2, 0,                      // !positions
        4, 0,                      // !stored
    ];
    assert_eq!(s.canonical_index_definition(), expected);
    assert_eq!(
        s.hash().get(),
        persia_format::hash::xxh3_128(expected),
        "the hash is XXH3-128 of the canonical bytes"
    );
    assert_eq!(s.hash().to_string(), GOLDEN_REMAINING_TYPES);
    // The empty schema.
    let empty = SchemaBuilder::new("e").build().unwrap();
    assert_eq!(empty.canonical_index_definition(), [1, 1, b'e', 0]);
    assert_eq!(empty.hash().to_string(), GOLDEN_EMPTY);
    // `Display` is 32 lowercase hex digits, zero-padded.
    assert_eq!(
        persia_engine::schema::SchemaHash::from_u128(0xab).to_string(),
        "000000000000000000000000000000ab"
    );
}

const GOLDEN_REMAINING_TYPES: &str = "8fa45c7272bc18a279e24b038b781c8e";
const GOLDEN_EMPTY: &str = "4236aa2d3c043329e3d87ea985a0c010";

/// H2. `field_count` and string lengths are LEB128 varints (§4.1): check the one-to-two-byte boundary.
#[test]
fn canonical_encoding_varint_boundaries() {
    for (count, varint) in [
        (127, &[0x7f][..]),
        (128, &[0x80, 0x01][..]),
        (300, &[0xac, 0x02][..]),
    ] {
        let s = (0..count)
            .fold(SchemaBuilder::new("c"), |b, i| {
                b.field(format!("f{i:03}"), FieldKind::Stored)
            })
            .build()
            .unwrap();
        let mut expected = vec![1, 1, b'c'];
        expected.extend_from_slice(varint);
        for i in 0..count {
            expected.push(4);
            expected.extend_from_slice(format!("f{i:03}").as_bytes());
            expected.extend_from_slice(&[7, 0]);
        }
        assert_eq!(s.canonical_index_definition(), expected, "{count}");
    }
}

/// Writes `v` as JSON with the members of every object in the order `order` picks (a permutation of `0..len`).
fn write_ordered(v: &Value, order: &mut dyn FnMut(usize) -> Vec<usize>, out: &mut String) {
    match v {
        Value::Object(map) => {
            let entries: Vec<_> = map.iter().collect();
            out.push('{');
            for (i, index) in order(entries.len()).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let (key, value) = *entries.get(index).unwrap();
                out.push_str(&serde_json::to_string(key).unwrap());
                out.push(':');
                write_ordered(value, order, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_ordered(item, order, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// A seeded permutation (splitmix64 + Fisher-Yates); no RNG dependency.
fn permutation(state: &mut u64, len: usize) -> Vec<usize> {
    let mut p: Vec<usize> = (0..len).collect();
    for i in (1..len).rev() {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let j = usize::try_from(z % (i as u64 + 1)).unwrap();
        p.swap(i, j);
    }
    p
}

fn collect_pointers(v: &Value, at: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                let p = format!("{at}/{}", k.replace('~', "~0").replace('/', "~1"));
                out.push(p.clone());
                collect_pointers(child, &p, out);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                let p = format!("{at}/{i}");
                out.push(p.clone());
                collect_pointers(child, &p, out);
            }
        }
        _ => {}
    }
}

/// H1. Every object of the SPEC example written in ascending key order (which puts `type` last in every field)
/// and in descending key order (`type` first, every other level reversed).
#[test]
fn sorted_and_reversed_key_order_keep_schema_and_hash() {
    let base = CollectionSchema::from_json(SPEC_EXAMPLE).unwrap();
    for descending in [false, true] {
        let mut text = String::new();
        write_ordered(
            &spec_value(),
            &mut |len| {
                let mut order: Vec<usize> = (0..len).collect();
                if descending {
                    order.reverse();
                }
                order
            },
            &mut text,
        );
        let type_last = text.contains(r#""positions":true,"type":"text"}"#);
        assert_eq!(type_last, !descending, "{text}");
        let other = CollectionSchema::from_json(&text).unwrap();
        assert_eq!(other, base);
        assert_eq!(other.hash().to_string(), GOLDEN_SPEC_EXAMPLE);
    }
}

/// One change to what is indexed (SPEC §6.2.4's list, plus names).
#[derive(Debug, Clone, Copy)]
enum IndexChange {
    Stored,
    Type,
    PositionsOrFacet,
    Analyzer,
    RenameField,
    RenameCollection,
}

fn apply_index_change(
    name: &str,
    mut fields: Vec<(String, FieldKind)>,
    target: usize,
    change: IndexChange,
) -> (String, Vec<(String, FieldKind)>) {
    let mut name = name.to_owned();
    let i = target % fields.len();
    let kind = &mut fields.get_mut(i).unwrap().1;
    match change {
        IndexChange::Stored => match kind {
            FieldKind::Text(o) => o.stored = !o.stored,
            FieldKind::Keyword(o) => o.stored = !o.stored,
            FieldKind::I64 { stored }
            | FieldKind::F64 { stored }
            | FieldKind::Bool { stored }
            | FieldKind::Datetime { stored } => *stored = !*stored,
            FieldKind::Stored => *kind = FieldKind::I64 { stored: true },
        },
        IndexChange::Type => {
            *kind = match kind {
                FieldKind::Text(_) | FieldKind::Stored => {
                    FieldKind::Keyword(KeywordOptions::default())
                }
                FieldKind::Keyword(o) => FieldKind::Text(TextOptions {
                    stored: o.stored,
                    ..TextOptions::default()
                }),
                FieldKind::I64 { stored } => FieldKind::F64 { stored: *stored },
                FieldKind::F64 { stored } => FieldKind::Bool { stored: *stored },
                FieldKind::Bool { stored } => FieldKind::Datetime { stored: *stored },
                FieldKind::Datetime { stored } => FieldKind::I64 { stored: *stored },
            };
        }
        IndexChange::PositionsOrFacet => match kind {
            FieldKind::Text(o) => o.positions = !o.positions,
            FieldKind::Keyword(o) => o.facet = !o.facet,
            FieldKind::Stored => *kind = FieldKind::Keyword(KeywordOptions::default()),
            other => *other = FieldKind::Stored,
        },
        IndexChange::Analyzer => match kind {
            FieldKind::Text(o) => {
                let names = persia_engine::schema::BUILTIN_ANALYZERS;
                let at = names.iter().position(|n| *n == o.analyzer.name()).unwrap();
                o.analyzer =
                    AnalyzerRef::builtin(names.get((at + 1) % names.len()).unwrap()).unwrap();
            }
            other => *other = FieldKind::Text(TextOptions::default()),
        },
        IndexChange::RenameField => {
            let old = fields.get(i).unwrap().0.clone();
            let mut new: String = old.chars().take(63).collect();
            new.push('Q');
            while fields.iter().any(|(n, _)| *n == new) {
                new.insert(0, 'Q');
                new.truncate(64);
            }
            fields.get_mut(i).unwrap().0 = new;
        }
        IndexChange::RenameCollection => {
            name.truncate(63);
            name.push('q');
        }
    }
    (name, fields)
}

fn index_change() -> impl Strategy<Value = IndexChange> {
    prop::sample::select(vec![
        IndexChange::Stored,
        IndexChange::Type,
        IndexChange::PositionsOrFacet,
        IndexChange::Analyzer,
        IndexChange::RenameField,
        IndexChange::RenameCollection,
    ])
}

/// Replaces, removes or inserts at a seeded JSON pointer of `v`; keeps the result schema-shaped but arbitrary.
fn mutate_json(v: &mut Value, seed: u64, op: u8, replacement: Value, key: &str) {
    let mut pointers = vec![String::new()];
    collect_pointers(v, "", &mut pointers);
    let at = pointers
        .get(usize::try_from(seed % pointers.len() as u64).unwrap())
        .unwrap();
    let target = v.pointer_mut(at).unwrap();
    match (op % 3, target) {
        (0, t) => *t = replacement,
        (1, Value::Object(map)) => {
            map.insert(key.to_owned(), replacement);
        }
        (1, Value::Array(items)) => items.push(replacement),
        (_, Value::Object(map)) => {
            if let Some(first) = map.keys().nth(usize::try_from(seed % 7).unwrap()).cloned() {
                map.remove(&first);
            }
        }
        (_, Value::Array(items)) => {
            items.pop();
        }
        (_, t) => *t = replacement,
    }
}

fn json_key() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::sample::select(vec![
            "name",
            "fields",
            "ranking",
            "relaxation",
            "synonyms",
            "strict",
            "type",
            "id",
            "analyzer",
            "boost",
            "positions",
            "k1",
            "b",
            "stored",
            "facet",
            "static_rank",
            "field",
            "weight",
            "fn",
            "enabled",
            "min_results",
            "levels",
            "fallback",
            "fill",
            "filters",
            "budget_ms",
            "title",
            "ID",
            "blob",
            "_x",
            "Extra",
            "",
        ])
        .prop_map(str::to_owned),
        "[A-Za-z_]{1,8}",
    ]
}

fn json_scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        prop_oneof![
            Just(0_i64),
            Just(1),
            Just(-1),
            Just(1000),
            Just(1024),
            Just(10_000),
            Just(10_001),
            Just(60_000),
            Just(i64::from(u32::MAX)),
            Just(i64::from(u32::MAX) + 1),
            Just(i64::MIN),
            any::<i64>(),
        ]
        .prop_map(|n| json!(n)),
        Just(json!(u64::MAX)),
        prop_oneof![
            Just(-0.0),
            Just(0.75),
            Just(1e-300),
            Just(5e-324),
            Just(1000.0),
            Just(1_000.000_000_000_1),
            Just(f64::MAX),
            Just(-f64::MAX),
            any::<f64>().prop_filter("JSON has no NaN or inf", |f| f.is_finite()),
        ]
        .prop_map(|f| json!(f)),
        prop::sample::select(vec![
            "text",
            "keyword",
            "i64",
            "f64",
            "bool",
            "datetime",
            "stored",
            "es",
            "es@1",
            "es@2",
            "standard@0",
            "default",
            "synonyms",
            "drop_terms",
            "prefix",
            "fuzzy",
            "log1p",
            "linear",
            "none",
            "static_rank",
            "never",
            "last_resort",
            "id",
            "a",
            "",
            " ",
        ])
        .prop_map(|s| json!(s)),
        "\\PC{0,8}".prop_map(Value::String),
    ]
}

fn json_value() -> impl Strategy<Value = Value> {
    json_scalar().prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            prop::collection::btree_map(json_key(), inner, 0..6)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// V1 + R1: whatever `from_json` accepts is a valid schema with every invariant; the serde impl agrees with it.
fn check_accepted_or_rejected(text: &str) -> Result<(), TestCaseError> {
    let parsed = CollectionSchema::from_json(text);
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        let via_serde = serde_json::from_value::<CollectionSchema>(value);
        prop_assert_eq!(via_serde.as_ref().ok(), parsed.as_ref().ok(), "{}", text);
    }
    let Ok(s) = parsed else {
        return Ok(());
    };
    for (position, f) in s.fields().iter().enumerate() {
        prop_assert_eq!(usize::try_from(f.id().get()).unwrap(), position);
        prop_assert_eq!(
            s.field(f.name()).map(persia_engine::schema::Field::id),
            Some(f.id())
        );
    }
    prop_assert_eq!(
        s.hash().get(),
        persia_format::hash::xxh3_128(&s.canonical_index_definition())
    );
    let json = s.to_json().unwrap();
    let back = CollectionSchema::from_json(&json).unwrap();
    prop_assert_eq!(back.to_json().unwrap(), json);
    prop_assert_eq!(back.hash(), s.hash());
    prop_assert_eq!(&back, &s);
    let via_value: CollectionSchema =
        serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
    prop_assert_eq!(&via_value, &s);
    let evolved = s.evolve(&s);
    prop_assert_eq!(evolved.as_ref(), Ok(&s));
    Ok(())
}

proptest! {
    /// R1. The serde impls round-trip exactly too (a `Value` reorders keys; floats keep their bits).
    #[test]
    fn serde_value_round_trip((_, builder) in schema()) {
        let s = builder.build().unwrap();
        let value = serde_json::to_value(&s).unwrap();
        let back: CollectionSchema = serde_json::from_value(value.clone()).unwrap();
        prop_assert_eq!(back.to_json().unwrap(), s.to_json().unwrap());
        prop_assert_eq!(back.hash(), s.hash());
        prop_assert_eq!(&back, &s);
        prop_assert_eq!(CollectionSchema::from_json(&value.to_string()).unwrap(), s);
    }

    /// H1. Any key order at every level (so `type` anywhere in a field), with or without the written ids, gives
    /// the same schema and hash.
    #[test]
    fn json_key_order_is_irrelevant((_, builder) in schema(), seed in any::<u64>(), strip_ids in any::<bool>()) {
        let s = builder.build().unwrap();
        let mut value: Value = serde_json::from_str(&s.to_json().unwrap()).unwrap();
        if strip_ids {
            // Builder schemas have name-ordered ids, which is what assignment gives back.
            for field in value["fields"].as_object_mut().unwrap().values_mut() {
                field.as_object_mut().unwrap().remove("id");
            }
        }
        let mut state = seed;
        let mut text = String::new();
        write_ordered(&value, &mut |len| permutation(&mut state, len), &mut text);
        let back = CollectionSchema::from_json(&text).unwrap();
        prop_assert_eq!(back.hash(), s.hash());
        prop_assert_eq!(&back, &s);
    }

    /// H2. Any one change to what is indexed changes the canonical bytes and the hash.
    #[test]
    fn index_definition_changes_change_the_hash(
        (fields, builder) in schema(),
        target in any::<usize>(),
        change in index_change(),
    ) {
        prop_assume!(!fields.is_empty());
        let s = builder.build().unwrap();
        let (name, changed) = apply_index_change(s.name(), fields, target, change);
        // Ranking is query-time; dropping it keeps a type change of the static-rank field valid.
        let other = changed
            .into_iter()
            .fold(SchemaBuilder::new(name), |b, (n, k)| b.field(n, k))
            .relaxation(s.relaxation().clone())
            .synonyms(s.synonyms().to_vec())
            .strict(s.strict())
            .build()
            .unwrap();
        prop_assert_ne!(other.canonical_index_definition(), s.canonical_index_definition(), "{:?}", change);
        prop_assert_ne!(other.hash(), s.hash(), "{:?}", change);
    }

    /// H2. Query-time settings (SPEC §6.2.5's list) never reach the canonical bytes.
    #[test]
    fn query_time_changes_keep_the_hash(
        (fields, builder) in schema(),
        boosts in prop::collection::vec((1e-6..=1000.0f64, 0.0..=10.0f64, 0.0..=1.0f64), 12),
        relax in relaxation(),
        strict in any::<bool>(),
        synonyms in prop::collection::vec(prop::collection::btree_set("[a-z]{1,4}", 2..4), 0..3),
    ) {
        let s = builder.build().unwrap();
        let retuned = fields
            .into_iter()
            .zip(boosts)
            .map(|((n, k), (boost, k1, b))| match k {
                FieldKind::Text(o) => (n, FieldKind::Text(TextOptions { boost, k1, b, ..o })),
                other => (n, other),
            })
            .fold(SchemaBuilder::new(s.name()), |b, (n, k)| b.field(n, k))
            .relaxation(relax)
            .synonyms(synonyms.into_iter().map(|g| g.into_iter().collect()).collect())
            .strict(strict)
            .build()
            .unwrap();
        prop_assert_eq!(retuned.canonical_index_definition(), s.canonical_index_definition());
        prop_assert_eq!(retuned.hash(), s.hash());
    }

    /// V1. Near-valid definitions (a valid one with a few random replacements, insertions and removals at random
    /// paths) never panic, and whatever is accepted satisfies every invariant.
    #[test]
    fn mutated_definitions_never_panic(
        (_, builder) in schema(),
        edits in prop::collection::vec((any::<u64>(), any::<u8>(), json_value(), json_key()), 1..4),
        seed in any::<u64>(),
    ) {
        let s = builder.build().unwrap();
        let mut value: Value = serde_json::from_str(&s.to_json().unwrap()).unwrap();
        for (at, op, replacement, key) in edits {
            mutate_json(&mut value, at, op, replacement, &key);
        }
        let mut state = seed;
        let mut text = String::new();
        write_ordered(&value, &mut |len| permutation(&mut state, len), &mut text);
        check_accepted_or_rejected(&text)?;
    }

    /// V1. Arbitrary JSON built from the schema's vocabulary never panics.
    #[test]
    fn arbitrary_json_never_panics(value in json_value()) {
        check_accepted_or_rejected(&value.to_string())?;
    }
}
