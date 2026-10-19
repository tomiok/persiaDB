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
