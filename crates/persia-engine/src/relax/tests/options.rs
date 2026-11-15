//! A query's `relax` (ROADMAP 8.3.1, ADR-0045): its JSON form, its rules, how it merges over the collection's settings,
//! and the settings a cursor records.

use super::*;
use crate::schema::{Fallback, Level, Levels};

fn parse(json: &str) -> Result<RelaxOptions, RelaxError> {
    RelaxOptions::from_json(json)
}

fn json_error(json: &str) -> String {
    match parse(json) {
        Err(RelaxError::Json(e)) => e,
        other => panic!("{json}: {other:?}"),
    }
}

#[test]
fn false_true_and_an_object_are_the_forms() {
    assert_eq!(parse("false").unwrap(), RelaxOptions::disabled());
    assert_eq!(
        parse("true").unwrap(),
        RelaxOptions {
            enabled: Some(true),
            ..RelaxOptions::default()
        }
    );
    assert_eq!(parse("{}").unwrap(), RelaxOptions::default());
    let every = parse(
        r#"{"enabled": true, "min_results": 7, "levels": ["drop_terms", "fuzzy"], "fallback": "none",
            "fill": true, "filters": "last_resort", "budget_ms": 900}"#,
    )
    .unwrap();
    assert_eq!(
        every,
        RelaxOptions {
            enabled: Some(true),
            min_results: Some(7),
            levels: Some(Levels::Custom(vec![Level::DropTerms, Level::Fuzzy])),
            fallback: Some(Fallback::None),
            fill: Some(true),
            filters: Some(FilterRelaxation::LastResort),
            budget_ms: Some(900),
        }
    );
    assert_eq!(
        parse(r#"{"levels": "default"}"#).unwrap().levels,
        Some(Levels::Default)
    );
    assert_eq!(
        parse(r#"{"levels": []}"#).unwrap().levels,
        Some(Levels::Custom(Vec::new()))
    );
}

#[test]
fn other_shapes_unknown_repeated_and_null_keys_are_json_errors() {
    for json in [
        "",
        "null",
        "1",
        r#""false""#,
        "[]",
        "[true, 5]",
        "{",
        "false false",
    ] {
        json_error(json);
    }
    assert!(json_error(r#"{"minimum": 3}"#).contains("minimum"));
    assert!(json_error(r#"{"fill": true, "fill": false}"#).contains("duplicate"));
    json_error(r#"{"fill": null}"#);
    json_error(r#"{"min_results": -1}"#);
    json_error(r#"{"min_results": 1.5}"#);
    json_error(r#"{"budget_ms": 4294967296}"#);
    json_error(r#"{"levels": ["nope"]}"#);
    json_error(r#"{"levels": "all"}"#);
    json_error(r#"{"fallback": "recency"}"#);
    json_error(r#"{"filters": "always"}"#);
    // L0 and L5 are not listed: L0 always runs and L5 is `fallback`.
    json_error(r#"{"levels": ["exact"]}"#);
    json_error(r#"{"levels": ["fallback"]}"#);
    let long = format!(
        r#"{{"fill": true{}}}"#,
        " ".repeat(RelaxOptions::MAX_JSON_LEN)
    );
    assert!(json_error(&long).contains("at most"));
}

#[test]
fn values_are_held_to_their_rules_also_when_the_cascade_is_off() {
    for (json, option) in [
        (r#"{"min_results": 0}"#, "min_results"),
        (r#"{"min_results": 10001}"#, "min_results"),
        (r#"{"enabled": false, "min_results": 0}"#, "min_results"),
        (r#"{"budget_ms": 0}"#, "budget_ms"),
        (r#"{"enabled": false, "budget_ms": 60001}"#, "budget_ms"),
    ] {
        assert!(
            matches!(parse(json), Err(RelaxError::OutOfRange { option: o, .. }) if o == option),
            "{json}"
        );
    }
    for json in [
        r#"{"levels": ["fuzzy", "prefix"]}"#,
        r#"{"enabled": false, "levels": ["prefix", "prefix"]}"#,
    ] {
        assert!(
            matches!(parse(json), Err(RelaxError::Levels { .. })),
            "{json}"
        );
    }
    // The edges are allowed.
    for json in [
        r#"{"min_results": 1, "budget_ms": 1}"#,
        r#"{"min_results": 10000, "budget_ms": 60000}"#,
    ] {
        parse(json).unwrap();
    }
    // `validate` and `RelaxPlan::new` agree on a value set in code.
    let bad = RelaxOptions {
        budget_ms: Some(0),
        ..RelaxOptions::disabled()
    };
    assert_eq!(
        bad.validate(),
        RelaxPlan::new(&RelaxationConfig::default(), &bad).map(|_| ())
    );
}

#[test]
fn a_parsed_relax_merges_one_setting_at_a_time() {
    let collection = RelaxationConfig {
        min_results: 4,
        fill: true,
        budget_ms: 300,
        levels: Levels::Custom(vec![Level::Synonyms, Level::Prefix]),
        ..RelaxationConfig::default()
    };
    let plan = RelaxPlan::new(&collection, &parse(r#"{"min_results": 9}"#).unwrap()).unwrap();
    assert_eq!(plan.min_results(), 9);
    assert!(plan.fill());
    assert_eq!(plan.budget(), Duration::from_millis(300));
    assert_eq!(
        levels(&plan),
        [
            RelaxLevel::Exact,
            RelaxLevel::Synonyms,
            RelaxLevel::Prefix,
            RelaxLevel::Fallback
        ]
    );
    // `levels` replaces the collection's list whole: it is one setting.
    let plan = RelaxPlan::new(&collection, &parse(r#"{"levels": ["fuzzy"]}"#).unwrap()).unwrap();
    assert_eq!(
        levels(&plan),
        [RelaxLevel::Exact, RelaxLevel::Fuzzy, RelaxLevel::Fallback]
    );
    // `true` turns on a cascade the collection turned off, with the collection's other settings.
    let off = RelaxationConfig {
        enabled: false,
        ..collection.clone()
    };
    assert!(
        !RelaxPlan::new(&off, &RelaxOptions::default())
            .unwrap()
            .relaxes()
    );
    let on = RelaxPlan::new(&off, &parse("true").unwrap()).unwrap();
    assert_eq!(levels(&on).len(), 4);
    assert_eq!(on.min_results(), 4);
}

fn settings(collection: &RelaxationConfig, options: &RelaxOptions) -> Option<RelaxSettings> {
    RelaxPlan::new(collection, options).unwrap().settings()
}

#[test]
fn the_settings_a_cursor_records_are_those_that_decide_the_ranking() {
    let collection = RelaxationConfig::default();
    let base = settings(&collection, &RelaxOptions::default()).unwrap();
    assert_eq!(base.levels().collect::<Vec<_>>(), ALL);
    assert_eq!(
        (base.min_results(), base.fill(), base.filters()),
        (1, false, FilterRelaxation::Never)
    );
    assert_eq!(
        base.to_string(),
        "levels [exact, synonyms, drop_terms, prefix, fuzzy, fallback], min_results 1, fill false, filters never"
    );
    // Each of these changes the steps or where the cascade stops.
    for json in [
        r#"{"min_results": 2}"#,
        r#"{"fill": true}"#,
        r#"{"filters": "last_resort"}"#,
        r#"{"fallback": "none"}"#,
        r#"{"levels": ["synonyms", "drop_terms", "prefix"]}"#,
    ] {
        assert_ne!(
            settings(&collection, &parse(json).unwrap()),
            Some(base),
            "{json}"
        );
    }
    // The budget only truncates a page; the default levels spelled out are the same levels.
    for json in [
        r#"{"budget_ms": 2000}"#,
        r#"{"levels": ["synonyms", "drop_terms", "prefix", "fuzzy"]}"#,
        r#"{"enabled": true}"#,
    ] {
        assert_eq!(
            settings(&collection, &parse(json).unwrap()),
            Some(base),
            "{json}"
        );
    }
    // A plan of L0 alone has none, however it got there, as a plain search has none.
    for json in [
        "false",
        r#"{"enabled": false, "min_results": 50, "fill": true}"#,
        r#"{"levels": [], "fallback": "none"}"#,
    ] {
        assert_eq!(settings(&collection, &parse(json).unwrap()), None, "{json}");
    }
    // A query that restates the collection's settings has the collection's.
    let restated = RelaxationConfig {
        min_results: 3,
        fill: true,
        ..RelaxationConfig::default()
    };
    assert_eq!(
        settings(
            &restated,
            &parse(r#"{"min_results": 3, "fill": true}"#).unwrap()
        ),
        settings(&restated, &RelaxOptions::default())
    );
}
