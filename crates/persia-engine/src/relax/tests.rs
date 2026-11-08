#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::query::TermQuery;
use crate::schema::{FieldKind, SchemaBuilder, TextOptions};

const ALL: [RelaxLevel; 6] = [
    RelaxLevel::Exact,
    RelaxLevel::Synonyms,
    RelaxLevel::DropTerms,
    RelaxLevel::Prefix,
    RelaxLevel::Fuzzy,
    RelaxLevel::Fallback,
];

fn levels(plan: &RelaxPlan) -> Vec<RelaxLevel> {
    plan.levels().collect()
}

#[test]
fn collection_defaults_plan_every_level_then_the_fallback() {
    let plan = RelaxPlan::new(&RelaxationConfig::default(), &RelaxOptions::default()).unwrap();
    assert_eq!(levels(&plan), ALL);
    assert!(plan.relaxes());
    assert_eq!(plan.min_results(), 1);
    assert!(!plan.fill());
    assert_eq!(plan.filters(), FilterRelaxation::Never);
    assert_eq!(plan.budget(), Duration::from_millis(50));
}

#[test]
fn levels_and_names_are_in_cascade_order() {
    assert!(ALL.windows(2).all(|w| w[0] < w[1]));
    let names: Vec<&str> = ALL.iter().map(|l| l.name()).collect();
    assert_eq!(
        names,
        [
            "exact",
            "synonyms",
            "drop_terms",
            "prefix",
            "fuzzy",
            "fallback"
        ]
    );
    assert_eq!(RelaxLevel::Fuzzy.to_string(), "fuzzy");
    // A schema level maps to the cascade level of the same name.
    for level in Level::ALL {
        assert_eq!(RelaxLevel::from(level).name(), level.name());
    }
}

#[test]
fn a_query_overrides_each_setting_it_gives_and_keeps_the_others() {
    let collection = RelaxationConfig {
        min_results: 3,
        fill: true,
        budget_ms: 20,
        filters: FilterRelaxation::LastResort,
        ..RelaxationConfig::default()
    };
    // Nothing given: the collection's.
    let plan = RelaxPlan::new(&collection, &RelaxOptions::default()).unwrap();
    assert_eq!(
        (
            plan.min_results(),
            plan.fill(),
            plan.budget(),
            plan.filters()
        ),
        (
            3,
            true,
            Duration::from_millis(20),
            FilterRelaxation::LastResort
        )
    );
    // Each given: the query's.
    let options = RelaxOptions {
        min_results: Some(7),
        fill: Some(false),
        budget_ms: Some(5),
        filters: Some(FilterRelaxation::Never),
        ..RelaxOptions::default()
    };
    let plan = RelaxPlan::new(&collection, &options).unwrap();
    assert_eq!(
        (
            plan.min_results(),
            plan.fill(),
            plan.budget(),
            plan.filters()
        ),
        (7, false, Duration::from_millis(5), FilterRelaxation::Never)
    );
    // One given: only that one moves.
    let plan = RelaxPlan::new(
        &collection,
        &RelaxOptions {
            budget_ms: Some(9),
            ..RelaxOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        (plan.min_results(), plan.fill(), plan.budget()),
        (3, true, Duration::from_millis(9))
    );
}

#[test]
fn levels_and_fallback_come_from_the_query_before_the_collection() {
    let collection = RelaxationConfig {
        levels: Levels::Custom(vec![Level::Synonyms, Level::Prefix]),
        fallback: Fallback::None,
        ..RelaxationConfig::default()
    };
    let plan = RelaxPlan::new(&collection, &RelaxOptions::default()).unwrap();
    assert_eq!(
        levels(&plan),
        [RelaxLevel::Exact, RelaxLevel::Synonyms, RelaxLevel::Prefix]
    );
    let options = RelaxOptions {
        levels: Some(Levels::Default),
        fallback: Some(Fallback::StaticRank),
        ..RelaxOptions::default()
    };
    assert_eq!(levels(&RelaxPlan::new(&collection, &options).unwrap()), ALL);
    // No level after L0: L0, then the fallback.
    let options = RelaxOptions {
        levels: Some(Levels::Custom(Vec::new())),
        fallback: Some(Fallback::StaticRank),
        ..RelaxOptions::default()
    };
    assert_eq!(
        levels(&RelaxPlan::new(&collection, &options).unwrap()),
        [RelaxLevel::Exact, RelaxLevel::Fallback]
    );
    // Neither: L0 alone, and the plan does not relax.
    let options = RelaxOptions {
        levels: Some(Levels::Custom(Vec::new())),
        ..RelaxOptions::default()
    };
    let plan = RelaxPlan::new(&collection, &options).unwrap();
    assert_eq!(levels(&plan), [RelaxLevel::Exact]);
    assert!(!plan.relaxes());
}

#[test]
fn relax_false_runs_l0_alone_and_the_query_wins_over_the_collection() {
    let on = RelaxationConfig::default();
    let off = RelaxationConfig {
        enabled: false,
        ..RelaxationConfig::default()
    };
    let disabled = RelaxPlan::new(&on, &RelaxOptions::disabled()).unwrap();
    assert_eq!(levels(&disabled), [RelaxLevel::Exact]);
    assert!(!disabled.relaxes());
    assert_eq!(
        levels(&RelaxPlan::new(&off, &RelaxOptions::default()).unwrap()),
        [RelaxLevel::Exact]
    );
    let enabled = RelaxOptions {
        enabled: Some(true),
        ..RelaxOptions::default()
    };
    assert_eq!(levels(&RelaxPlan::new(&off, &enabled).unwrap()), ALL);
}

#[test]
fn a_query_setting_out_of_range_is_an_error_naming_it() {
    let collection = RelaxationConfig::default();
    let min = |n| RelaxOptions {
        min_results: Some(n),
        ..RelaxOptions::default()
    };
    let budget = |n| RelaxOptions {
        budget_ms: Some(n),
        ..RelaxOptions::default()
    };
    for (options, option, value) in [
        (min(0), "min_results", 0),
        (min(10_001), "min_results", 10_001),
        (budget(0), "budget_ms", 0),
        (budget(60_001), "budget_ms", 60_001),
    ] {
        let err = RelaxPlan::new(&collection, &options).unwrap_err();
        assert!(
            matches!(err, RelaxError::OutOfRange { option: o, value: v, .. } if o == option && v == value),
            "{err}"
        );
    }
    // The bounds themselves are allowed.
    for options in [min(1), min(10_000), budget(1), budget(60_000)] {
        RelaxPlan::new(&collection, &options).unwrap();
    }
    // Even when the query disables the cascade: the settings are still the query's to get right.
    let off = RelaxOptions {
        min_results: Some(0),
        ..RelaxOptions::disabled()
    };
    assert!(RelaxPlan::new(&collection, &off).is_err());
}

#[test]
fn custom_levels_must_be_distinct_and_in_cascade_order() {
    let collection = RelaxationConfig::default();
    let custom = |l: Vec<Level>| RelaxOptions {
        levels: Some(Levels::Custom(l)),
        ..RelaxOptions::default()
    };
    let twice = RelaxPlan::new(&collection, &custom(vec![Level::Prefix, Level::Prefix]));
    assert!(matches!(twice, Err(RelaxError::Levels { problem }) if problem.contains("twice")));
    let reversed = RelaxPlan::new(&collection, &custom(vec![Level::Fuzzy, Level::Prefix]));
    assert!(matches!(reversed, Err(RelaxError::Levels { problem }) if problem.contains("order")));
    let ok = RelaxPlan::new(&collection, &custom(vec![Level::DropTerms, Level::Fuzzy])).unwrap();
    assert_eq!(
        levels(&ok),
        [
            RelaxLevel::Exact,
            RelaxLevel::DropTerms,
            RelaxLevel::Fuzzy,
            RelaxLevel::Fallback
        ]
    );
}

fn input_query() -> (CollectionSchema, Query) {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let query = Query::Term(TermQuery {
        field: body,
        term: "red".into(),
    });
    (schema, query)
}

#[test]
fn l0_is_the_query_as_compiled_and_later_levels_build_nothing_yet() {
    let (schema, query) = input_query();
    // A previous step that differs: L0 is the original, whatever came before.
    let previous = Query::MatchAll;
    let input = LevelInput {
        original: &query,
        previous: &previous,
        schema: &schema,
    };
    let plan = RelaxPlan::new(&RelaxationConfig::default(), &RelaxOptions::default()).unwrap();
    let steps: Vec<Vec<Step>> = plan
        .planned()
        .iter()
        .map(|l| l.transform.steps(&input))
        .collect();
    assert_eq!(
        steps[0],
        [Step {
            query: query.clone(),
            applied: String::new()
        }]
    );
    // L1 to L5 wait on ROADMAP 8.2: no step, so the cascade skips them.
    assert!(steps[1..].iter().all(Vec::is_empty));
}

#[derive(Debug)]
struct Fixed(Vec<Step>);

impl LevelTransform for Fixed {
    fn steps(&self, _input: &LevelInput<'_>) -> Vec<Step> {
        self.0.clone()
    }
}

#[test]
fn a_replaced_transform_keeps_its_place_and_a_new_level_goes_in_cascade_order() {
    let (schema, query) = input_query();
    let input = LevelInput {
        original: &query,
        previous: &query,
        schema: &schema,
    };
    let step = Step {
        query: Query::MatchAll,
        applied: "prefix".into(),
    };
    let none = RelaxOptions {
        levels: Some(Levels::Custom(vec![Level::Synonyms, Level::Fuzzy])),
        fallback: Some(Fallback::None),
        ..RelaxOptions::default()
    };
    let plan = RelaxPlan::new(&RelaxationConfig::default(), &none)
        .unwrap()
        .with_level(RelaxLevel::Prefix, Arc::new(Fixed(vec![step.clone()])))
        .with_level(RelaxLevel::Synonyms, Arc::new(Fixed(Vec::new())));
    assert_eq!(
        levels(&plan),
        [
            RelaxLevel::Exact,
            RelaxLevel::Synonyms,
            RelaxLevel::Prefix,
            RelaxLevel::Fuzzy
        ]
    );
    assert_eq!(plan.planned()[2].transform.steps(&input), [step]);
}
