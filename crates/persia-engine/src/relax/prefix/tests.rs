//! L3's step (SPEC §7.6, ROADMAP 8.2.3), as built from a query: which terms it expands and how.
//!
//! Invariants:
//! 1. The last term is expanded whatever its length; every other term only from [`MIN_LEN`] characters, counted on
//!    the analyzed term.
//! 2. Excluded clauses, phrases, `field:` clauses, filters and the terms of fields `expands` rejects (not `text`, or
//!    stacking tokens) stay exact; a step that expands nothing is not built.
//! 3. An expanded `combined` matches its exact fields or a prefix per expanded field, boosted by the field's weight;
//!    every prefix keeps [`DEFAULT_MAX_EXPANSIONS`].
//! 4. A compiled query's step states L0's text; a programmatic one's none. The level reads no statistics.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)] // Tests: inputs are fixed.

use std::ops::Bound;

use persia_analysis::AnalyzerRegistry;

use super::*;
use crate::query::{
    DefaultFields, FilterValue, QueryCompiler, QueryLimits, TermQuery, WeightedTerm,
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

/// A provider L3 never reads: planning expands its prefixes.
struct NoStats;

impl StatisticsProvider for NoStats {
    fn doc_count(&self) -> u64 {
        panic!("L3 reads no statistics")
    }
    fn field(&self, _: FieldId) -> crate::stats::FieldTotals {
        panic!("L3 reads no statistics")
    }
    fn df(&self, _: FieldId, _: &[u8]) -> Result<u64, ReadError> {
        panic!("L3 reads no statistics")
    }
}

/// `text` fields and their schema boost: the ones the search lets L3 expand when no analyzer stacks tokens.
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
    let mut steps = Prefix.steps(&input).unwrap();
    assert!(steps.len() <= 1, "one step at most: {steps:?}");
    steps.pop()
}

fn step(schema: &CollectionSchema, text: &str) -> Option<Step> {
    let compiled = compile(schema, text);
    step_of(&compiled.query, Some(&compiled), &text_fields(schema))
}

fn prefix_of(field: FieldId, p: &str) -> Query {
    Query::Prefix(PrefixQuery {
        field,
        prefix: p.into(),
        max_expansions: DEFAULT_MAX_EXPANSIONS,
    })
}

fn boosted(query: Query, boost: f64) -> Query {
    Query::Boost(BoostQuery {
        query: Box::new(query),
        boost,
    })
}

/// A word over `title` and `body`, both expanded. A `prefix` scores with its field's schema boost (`title`: 2), so
/// the default fields' weights, which are those boosts, add none.
fn both(schema: &CollectionSchema, title: &str, body: &str) -> Query {
    Query::Bool(BoolQuery {
        should: vec![
            prefix_of(id(schema, "body"), body),
            prefix_of(id(schema, "title"), title),
        ],
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
fn the_last_term_is_expanded_whatever_its_length_and_the_others_from_three_characters() {
    let schema = schema();
    // `ab` is last: expanded at 2 characters. `tv` is not last: kept at 2. `red` is 3: expanded.
    let s = step(&schema, "tv red ab").unwrap();
    assert_eq!(s.applied, "prefix");
    assert_eq!(s.effective.as_deref(), Some("tv red ab"));
    let compiled = compile(&schema, "tv red ab");
    let parts = must(&s.query);
    assert_eq!(parts[0], compiled.parts(0).unwrap()[0]);
    assert_eq!(parts[1], both(&schema, "red", "red"));
    assert_eq!(parts[2], both(&schema, "ab", "ab"));
    // Only short terms, none last: nothing is expanded, so no step.
    assert_eq!(step(&schema, "tv \"ab cd\"").map(|s| s.query), None);
}

#[test]
fn the_length_is_the_analyzed_terms_in_characters() {
    let schema = schema();
    // `Ñus` analyzes to `ñus` in `title`: 3 characters (4 bytes); `en` folds it to `nus`. Under `en`, `going` stems to
    // `go` (2): `body` keeps it exact while `title`, which keeps `going`, is expanded.
    let s = step(&schema, "Ñus going x").unwrap();
    let parts = must(&s.query);
    assert_eq!(parts[0], both(&schema, "ñus", "nus"));
    // `ñu` is 2 characters in 3 bytes: kept exact when it is not last.
    let compiled = compile(&schema, "ñu tv");
    let s = step(&schema, "ñu tv").unwrap();
    assert_eq!(must(&s.query)[0], compiled.parts(0).unwrap()[0]);
    assert_eq!(must(&s.query)[1], both(&schema, "tv", "tv"));
    let body = id(&schema, "body");
    let exact_body = Query::Combined(CombinedQuery {
        fields: vec![WeightedTerm {
            field: body,
            term: "go".into(),
            weight: 1.0,
        }],
    });
    assert_eq!(
        parts[1],
        Query::Bool(BoolQuery {
            should: vec![exact_body, prefix_of(id(&schema, "title"), "going")],
            ..BoolQuery::default()
        })
    );
}

#[test]
fn excluded_clauses_phrases_and_field_clauses_stay_exact() {
    let schema = schema();
    let text = "running -shoes \"red hat\" title:boots brand:Acme";
    let compiled = compile(&schema, text);
    let s = step(&schema, text).unwrap();
    let Query::Bool(b) = &s.query else { panic!() };
    // `running` is expanded (`run` in `body`), and nothing else.
    assert_eq!(b.must[0], both(&schema, "running", "run"));
    assert_eq!(&b.must[1..], &must(&compiled.query)[1..]);
    assert_eq!(b.must_not, compiled.root().unwrap().must_not);
    // A quoted single word compiles as a word, and stays exact as a phrase.
    assert_eq!(step(&schema, "tv \"sho\"").map(|s| s.query), None);
    // The last clause is a `field:` clause: no word is the last term, so a short word before it stays exact.
    assert_eq!(step(&schema, "ab title:boots").map(|s| s.query), None);
    // An excluded last clause is no term: the last required one is.
    let s = step(&schema, "ab -cd").unwrap();
    assert_eq!(must(&s.query)[0], both(&schema, "ab", "ab"));
}

#[test]
fn fields_it_may_not_expand_stay_exact() {
    let schema = schema();
    let title = id(&schema, "title");
    let compiled = compile(&schema, "red shoes");
    // As for a field whose analyzer stacks tokens: `title` stays exact, `body` is expanded.
    let not_title = |f: FieldId| text_fields(&schema)(f).filter(|_| f != title);
    let s = step_of(&compiled.query, Some(&compiled), &not_title).unwrap();
    let exact = |t: &str| {
        Query::Combined(CombinedQuery {
            fields: vec![WeightedTerm {
                field: title,
                term: t.into(),
                weight: 2.0,
            }],
        })
    };
    let open = |b: &str, t: &str| {
        Query::Bool(BoolQuery {
            should: vec![exact(t), prefix_of(id(&schema, "body"), b)],
            ..BoolQuery::default()
        })
    };
    assert_eq!(must(&s.query), [open("red", "red"), open("shoe", "shoes")]);
    // No field it may expand: no step.
    assert_eq!(step_of(&compiled.query, Some(&compiled), &|_| None), None);
}

#[test]
fn a_filter_stays_on_the_step() {
    let schema = schema();
    let filter = Filter::Range {
        field: id(&schema, "n"),
        lower: Bound::Included(FilterValue::I64(1)),
        upper: Bound::Unbounded,
    };
    // One word: the compiled query is its one part, wrapped by the filter (ROADMAP 8.1.17).
    let compiled = compile(&schema, "sho").with_filter([filter.clone()]);
    let s = step_of(&compiled.query, Some(&compiled), &text_fields(&schema)).unwrap();
    let Query::Bool(b) = &s.query else { panic!() };
    assert_eq!(b.must, [both(&schema, "sho", "sho")]);
    assert_eq!(b.filter, [filter]);
    // Without a filter, the step is the expanded word itself.
    assert_eq!(
        step(&schema, "sho").unwrap().query,
        both(&schema, "sho", "sho")
    );
}

#[test]
fn a_programmatic_query_expands_its_root_must_terms_the_last_one_last() {
    let schema = schema();
    let (title, brand) = (id(&schema, "title"), id(&schema, "brand"));
    let term = |field: FieldId, t: &str| {
        Query::Term(TermQuery {
            field,
            term: t.into(),
        })
    };
    let excluded = term(title, "used");
    let query = Query::Bool(BoolQuery {
        must: vec![
            term(title, "ab"),
            term(brand, "acme"),
            term(title, "red"),
            term(title, "x"),
        ],
        must_not: vec![excluded.clone()],
        ..BoolQuery::default()
    });
    let s = step_of(&query, None, &text_fields(&schema)).unwrap();
    assert_eq!(s.effective, None);
    let Query::Bool(b) = &s.query else { panic!() };
    // `ab` is short and not last; `acme` is a keyword; `red` is 3; `x` is last.
    assert_eq!(
        b.must,
        [
            term(title, "ab"),
            term(brand, "acme"),
            prefix_of(title, "red"),
            prefix_of(title, "x")
        ]
    );
    assert_eq!(b.must_not, [excluded]);
    // A term alone is its own last term; a root with `should` clauses is not relaxed.
    assert_eq!(
        step_of(&term(title, "x"), None, &text_fields(&schema))
            .unwrap()
            .query,
        prefix_of(title, "x")
    );
    let any = Query::Bool(BoolQuery {
        should: vec![term(title, "red")],
        must: vec![term(title, "red")],
        ..BoolQuery::default()
    });
    assert_eq!(step_of(&any, None, &text_fields(&schema)), None);
    // A boost keeps its weight around what it expands.
    let boosted_red = boosted(term(title, "red"), 3.0);
    assert_eq!(
        step_of(&boosted_red, None, &text_fields(&schema))
            .unwrap()
            .query,
        boosted(prefix_of(title, "red"), 3.0)
    );
}

#[test]
fn of_a_word_the_fields_split_apart_each_fields_last_token_is_last() {
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field(
            "sku",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("whitespace").unwrap(),
                ..TextOptions::default()
            }),
        )
        .build()
        .unwrap();
    let (title, sku) = (id(&schema, "title"), id(&schema, "sku"));
    // `e-ma`: `sku` keeps `e-ma` (its one token, last), `title` gives `e` (short, not last) and `ma` (last).
    let s = step(&schema, "e-ma").unwrap();
    let combined = |field: FieldId, t: &str| {
        Query::Combined(CombinedQuery {
            fields: vec![WeightedTerm {
                field,
                term: t.into(),
                weight: 1.0,
            }],
        })
    };
    assert_eq!(
        s.query,
        Query::Bool(BoolQuery {
            should: vec![
                prefix_of(sku, "e-ma"),
                Query::Bool(BoolQuery {
                    must: vec![combined(title, "e"), prefix_of(title, "ma")],
                    ..BoolQuery::default()
                }),
            ],
            ..BoolQuery::default()
        })
    );
}

#[test]
fn a_fields_weight_scales_a_prefix_by_its_ratio_to_the_schema_boost() {
    // A `prefix` scores with the schema boost as its field weight (SPEC §7.2): `title^5` over `title`'s boost 2 is a
    // ratio of 2.5, and `body` keeps its boost, so no boost. Ratios cap at a boost's 1000 (`title^1000` over 0.5).
    let schema = schema();
    let fields = DefaultFields::parse(&schema, &["title^5", "body"]).unwrap();
    let compiled = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile("sho", &fields, &QueryLimits::default());
    let s = step_of(&compiled.query, Some(&compiled), &text_fields(&schema)).unwrap();
    assert_eq!(
        s.query,
        Query::Bool(BoolQuery {
            // In the order `fields` gives them.
            should: vec![
                boosted(prefix_of(id(&schema, "title"), "sho"), 2.5),
                prefix_of(id(&schema, "body"), "sho"),
            ],
            ..BoolQuery::default()
        })
    );
    let half = |f: FieldId| text_fields(&schema)(f).map(|_| 0.5);
    let fields = DefaultFields::parse(&schema, &["title^1000"]).unwrap();
    let compiled = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile("sho", &fields, &QueryLimits::default());
    let s = step_of(&compiled.query, Some(&compiled), &half).unwrap();
    assert_eq!(
        s.query,
        boosted(prefix_of(id(&schema, "title"), "sho"), 1000.0)
    );
    // A boost in range is a valid query.
    s.query.validate(&schema, &QueryLimits::default()).unwrap();
}

#[test]
fn a_ratio_that_rounds_to_zero_keeps_the_smallest_boost() {
    // The least positive weight over `title`'s boost 2 rounds to 0, which is no boost: the prefix would score as the
    // full schema boost, so it keeps the smallest one instead (SPEC §7.1: `0 < w`).
    let schema = schema();
    let tiny = f64::from_bits(1);
    assert_eq!(tiny / 2.0, 0.0, "the ratio this test is about");
    let query = Query::Combined(CombinedQuery {
        fields: vec![WeightedTerm {
            field: id(&schema, "title"),
            term: "sho".into(),
            weight: tiny,
        }],
    });
    query.validate(&schema, &QueryLimits::default()).unwrap();
    let s = step_of(&query, None, &text_fields(&schema)).unwrap();
    assert_eq!(
        s.query,
        boosted(prefix_of(id(&schema, "title"), "sho"), tiny)
    );
    s.query.validate(&schema, &QueryLimits::default()).unwrap();
}

#[test]
fn an_exclusion_added_to_the_root_stays_excluded() {
    // As at L2: the root's `must_not` is kept whole, an exclusion a caller added beside the excluded clauses too.
    let schema = schema();
    let added = Query::Term(TermQuery {
        field: id(&schema, "brand"),
        term: "acme".into(),
    });
    let mut compiled = compile(&schema, "red sho -blue");
    let Query::Bool(root) = &mut compiled.query else {
        panic!()
    };
    root.must_not.push(added.clone());
    let want = root.must_not.clone();
    let s = step_of(&compiled.query, Some(&compiled), &text_fields(&schema)).unwrap();
    let Query::Bool(b) = &s.query else { panic!() };
    assert_eq!(b.must_not, want);
    assert_eq!(b.must_not.last(), Some(&added));
}
