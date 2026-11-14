//! L4's step (SPEC §7.6, ROADMAP 8.2.4), as built from a query: which terms it matches fuzzily, and at what distance.
//!
//! Invariants:
//! 1. The distance follows the analyzed term's length in characters (Unicode scalar values, not bytes): none below
//!    [`MIN_LEN`], 1 up to 7, 2 from [`TWO_EDITS_LEN`] (ROADMAP 8.2.4.4).
//! 2. Excluded clauses, phrases, `field:` clauses, filters and the terms of fields `expands` rejects (not `text`, or
//!    stacking tokens) stay exact; a step that rewrites nothing is not built. Unlike L3, the last term has no rule of
//!    its own.
//! 3. A rewritten `combined` matches its exact fields or a `fuzzy` per rewritten field, boosted by the ratio of the
//!    field's weight to its schema boost; every `fuzzy` keeps [`DEFAULT_MAX_EXPANSIONS`].
//! 4. A compiled query's step states L0's text; a programmatic one's none. Without the views' terms, the level reads
//!    no statistics.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)] // Tests: inputs are fixed.

use persia_analysis::AnalyzerRegistry;

use super::*;
use crate::query::{
    BoolQuery, BoostQuery, CombinedQuery, DefaultFields, Filter, FilterValue, QueryCompiler,
    QueryLimits, TermQuery, WeightedTerm,
};
use crate::schema::{
    AnalyzerRef, CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use crate::stats::StatisticsProvider;

/// `title` (standard, weight 2), `body` (`en`: `running` is `run`), `brand` (keyword), `n` (i64).
fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                boost: 2.0,
                ..TextOptions::default()
            }),
        )
        .field(
            "body",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("en").unwrap(),
                ..TextOptions::default()
            }),
        )
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .field("n", FieldKind::I64 { stored: false })
        .build()
        .unwrap()
}

fn id(schema: &CollectionSchema, name: &str) -> FieldId {
    schema.field(name).unwrap().id()
}

fn compile(schema: &CollectionSchema, text: &str) -> Compiled {
    QueryCompiler::new(schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(text, &DefaultFields::all(schema), &QueryLimits::default())
}

/// A provider L4 never reads without the views' terms: planning expands its `fuzzy` queries.
struct NoStats;

impl StatisticsProvider for NoStats {
    fn doc_count(&self) -> u64 {
        panic!("L4 reads no statistics without the views' terms")
    }
    fn field(&self, _: FieldId) -> crate::stats::FieldTotals {
        panic!("L4 reads no statistics without the views' terms")
    }
    fn df(&self, _: FieldId, _: &[u8]) -> Result<u64, ReadError> {
        panic!("L4 reads no statistics without the views' terms")
    }
}

/// `text` fields and their schema boost: the ones the search lets L4 rewrite when no analyzer stacks tokens.
fn text_fields(schema: &CollectionSchema) -> impl Fn(FieldId) -> Option<f64> + '_ {
    |f| match schema.field_by_id(f).map(crate::schema::Field::kind) {
        Some(FieldKind::Text(o)) => Some(o.boost),
        _ => None,
    }
}

fn step_of(
    query: &Query,
    compiled: Option<&Compiled>,
    expands: &dyn Fn(FieldId) -> Option<f64>,
) -> Option<Step> {
    let input = LevelInput::new(query, compiled, &NoStats).expanding(expands);
    let mut steps = Fuzzy.steps(&input).unwrap();
    assert!(steps.len() <= 1, "one step at most: {steps:?}");
    steps.pop()
}

fn step(schema: &CollectionSchema, text: &str) -> Option<Step> {
    let compiled = compile(schema, text);
    step_of(&compiled.query, Some(&compiled), &text_fields(schema))
}

fn fuzzy_of(field: FieldId, term: &str, distance: u8) -> Query {
    Query::Fuzzy(FuzzyQuery {
        field,
        term: term.into(),
        distance,
        max_expansions: DEFAULT_MAX_EXPANSIONS,
    })
}

fn exact(field: FieldId, term: &str, weight: f64) -> Query {
    Query::Combined(CombinedQuery {
        fields: vec![WeightedTerm {
            field,
            term: term.into(),
            weight,
        }],
    })
}

fn should(clauses: Vec<Query>) -> Query {
    Query::Bool(BoolQuery {
        should: clauses,
        ..BoolQuery::default()
    })
}

fn must(query: &Query) -> &[Query] {
    match query {
        Query::Bool(b) => &b.must,
        other => panic!("not a bool: {other:?}"),
    }
}

#[test]
fn the_distance_follows_the_length_in_characters() {
    for (term, want) in [
        ("", None),
        ("abc", None),
        ("abcd", Some(1)),
        ("abcdefg", Some(1)),
        ("abcdefgh", Some(2)),
        (&"x".repeat(32 * 1024), Some(2)),
        // Characters, not bytes: 3 characters in 5 bytes, 4 in 6, 7 in 11.
        ("ñuñ", None),
        ("ñuño", Some(1)),
        ("ñuñoñuñ", Some(1)),
        ("ñuñoñuño", Some(2)),
        ("😀😀😀", None),
        ("😀😀😀😀", Some(1)),
    ] {
        assert_eq!(distance(term), want, "{term:?}");
    }
}

#[test]
fn each_field_rewrites_its_own_analyzed_term() {
    let schema = schema();
    let (title, body) = (id(&schema, "title"), id(&schema, "body"));
    // `running` is 7 characters in `title` (distance 1); `en` stems it to `run` (3), which `body` keeps exact.
    // `connections` is 11 in `title` (2) and `connect` (7) in `body` (1). `red` (3) stays exact in both.
    let s = step(&schema, "running connections red").unwrap();
    let compiled = compile(&schema, "running connections red");
    let parts = must(&s.query);
    assert_eq!(parts.len(), 3);
    assert_eq!(
        parts[0],
        should(vec![exact(body, "run", 1.0), fuzzy_of(title, "running", 1)])
    );
    assert_eq!(
        parts[1],
        should(vec![
            fuzzy_of(body, "connect", 1),
            fuzzy_of(title, "connections", 2)
        ])
    );
    assert_eq!(parts[2], compiled.parts(2).unwrap()[0]);
    assert_eq!(s.applied, "fuzzy");
    assert_eq!(s.effective.as_deref(), Some("running connections red"));
    assert!(s.alternatives.is_none());
}

#[test]
fn short_words_alone_build_no_step() {
    let schema = schema();
    assert_eq!(step(&schema, "red tv"), None);
    // L3 expands a short last word; L4 does not.
    assert_eq!(step(&schema, "w"), None);
    assert_eq!(step(&schema, ""), None);
}

#[test]
fn excluded_clauses_phrases_and_field_clauses_stay_exact() {
    let schema = schema();
    let text = "-shoes \"leather boots\" title:sandals \"slippers\" sneakers";
    let compiled = compile(&schema, text);
    let s = step(&schema, text).unwrap();
    let root = match &s.query {
        Query::Bool(b) => b,
        other => panic!("not a bool: {other:?}"),
    };
    // Every part but the last word's is as compiled, the exclusion with them.
    let original = compiled.root().unwrap();
    assert_eq!(root.must_not, original.must_not);
    let n = root.must.len();
    assert_eq!(root.must[..n - 1], original.must[..n - 1]);
    assert_ne!(root.must[n - 1], original.must[n - 1]);
    // Without the word, nothing is rewritten.
    assert_eq!(
        step(
            &schema,
            "-shoes \"leather boots\" title:sandals \"slippers\""
        ),
        None
    );
}

#[test]
fn fields_it_may_not_rewrite_stay_exact() {
    let schema = schema();
    let (title, body) = (id(&schema, "title"), id(&schema, "body"));
    // Only `title` may be rewritten (as if `body` stacked tokens): `body` stays one exact `combined`.
    let only_title = |f: FieldId| (f == title).then_some(2.0);
    let compiled = compile(&schema, "sneakers");
    let s = step_of(&compiled.query, Some(&compiled), &only_title).unwrap();
    assert_eq!(
        s.query,
        should(vec![
            exact(body, "sneaker", 1.0),
            fuzzy_of(title, "sneakers", 2)
        ])
    );
    // No field may: no step.
    let none = |_: FieldId| None;
    assert_eq!(step_of(&compiled.query, Some(&compiled), &none), None);
    // A `keyword` term of a programmatic query is an identifier: exact.
    let brand = Query::Term(TermQuery {
        field: id(&schema, "brand"),
        term: "acme corp".into(),
    });
    assert_eq!(step_of(&brand, None, &text_fields(&schema)), None);
}

#[test]
fn a_filter_stays_on_the_step() {
    let schema = schema();
    let filter = Filter::Range {
        field: id(&schema, "n"),
        lower: Bound::Included(FilterValue::I64(1)),
        upper: Bound::Unbounded,
    };
    let compiled = compile(&schema, "sneakers").with_filter([filter.clone()]);
    let s = step_of(&compiled.query, Some(&compiled), &text_fields(&schema)).unwrap();
    match &s.query {
        Query::Bool(b) => {
            assert_eq!(b.filter, [filter]);
            assert_eq!(b.must.len(), 1);
        }
        other => panic!("not a bool: {other:?}"),
    }
}

#[test]
fn a_programmatic_query_rewrites_its_root_must_terms() {
    let schema = schema();
    let title = id(&schema, "title");
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: title,
            term: t.into(),
        })
    };
    let expands = text_fields(&schema);
    // A term alone.
    let s = step_of(&term("sandals"), None, &expands).unwrap();
    assert_eq!(s.query, fuzzy_of(title, "sandals", 1));
    assert_eq!(s.effective, None);
    // A root `bool`'s `must`, its `must_not` as it is; the short term stays exact.
    let root = Query::Bool(BoolQuery {
        must: vec![term("sandals"), term("red")],
        must_not: vec![term("boots")],
        ..BoolQuery::default()
    });
    let s = step_of(&root, None, &expands).unwrap();
    assert_eq!(
        s.query,
        Query::Bool(BoolQuery {
            must: vec![fuzzy_of(title, "sandals", 1), term("red")],
            must_not: vec![term("boots")],
            ..BoolQuery::default()
        })
    );
    // A root with `should` clauses is not relaxed, as at L2 and L3.
    let optional = Query::Bool(BoolQuery {
        should: vec![term("sandals")],
        ..BoolQuery::default()
    });
    assert_eq!(step_of(&optional, None, &expands), None);
    // Under a `boost`, the boost stays.
    let boosted = Query::Boost(BoostQuery {
        query: Box::new(term("sandals")),
        boost: 3.0,
    });
    let s = step_of(&boosted, None, &expands).unwrap();
    assert_eq!(
        s.query,
        Query::Boost(BoostQuery {
            query: Box::new(fuzzy_of(title, "sandals", 1)),
            boost: 3.0,
        })
    );
}

#[test]
fn a_fields_weight_scales_a_fuzzy_by_its_ratio_to_the_schema_boost() {
    let schema = schema();
    let title = id(&schema, "title");
    let fields = DefaultFields::parse(&schema, &["title^6"]).unwrap();
    let compiled = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile("sandals", &fields, &QueryLimits::default());
    let s = step_of(&compiled.query, Some(&compiled), &text_fields(&schema)).unwrap();
    // Weight 6 over the schema boost 2.
    assert_eq!(
        s.query,
        Query::Boost(BoostQuery {
            query: Box::new(fuzzy_of(title, "sandals", 1)),
            boost: 3.0,
        })
    );
}
