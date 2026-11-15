//! The filter step (SPEC §7.6, ROADMAP 8.2.6, ADR-0047), as built from a query: L0's without its root filters.
//!
//! Invariants:
//! 1. One step, `filters`, whose query is L0's root `bool` with its `filter` emptied, every other clause as it is
//!    (`must`, `should`, `must_not`, `minimum_should_match`), for a compiled query and a programmatic root `bool` alike.
//! 2. A root without filters, and any other root, builds no step: the level is skipped.
//! 3. Its `effective` is L0's text; it corrects nothing, has no synonym alternatives and reads no statistics.
//! 4. It never fills a page, and covers no earlier step; L5 covers every earlier step.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)] // Tests: inputs are fixed.

use persia_analysis::AnalyzerRegistry;

use super::*;
use crate::query::{DefaultFields, Query};
use crate::query::{Filter, FilterValue, QueryCompiler, QueryLimits, TermQuery};
use crate::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use crate::stats::StatisticsProvider;

/// `body` (standard), `brand` (keyword).
fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
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

/// A provider the filter step never reads: it rewrites no word.
struct NoStats;

impl StatisticsProvider for NoStats {
    fn doc_count(&self) -> u64 {
        panic!("the filter step reads no statistics")
    }
    fn field(&self, _: FieldId) -> crate::stats::FieldTotals {
        panic!("the filter step reads no statistics")
    }
    fn df(&self, _: FieldId, _: &[u8]) -> Result<u64, ReadError> {
        panic!("the filter step reads no statistics")
    }
}

fn steps(query: &Query, compiled: Option<&Compiled>) -> Vec<Step> {
    Filters
        .steps(&LevelInput::new(query, compiled, &NoStats))
        .unwrap()
}

fn one(query: &Query, compiled: Option<&Compiled>) -> Step {
    let mut steps = steps(query, compiled);
    assert_eq!(steps.len(), 1, "the filter step is one step");
    steps.remove(0)
}

fn brand_is(schema: &CollectionSchema, value: &str) -> Filter {
    Filter::Term {
        field: id(schema, "brand"),
        value: FilterValue::Keyword(value.into()),
    }
}

#[test]
fn a_compiled_query_drops_its_request_filter_and_keeps_every_clause() {
    let schema = schema();
    let compiled = compile(&schema, "-blue red -\"green hat\" shoes")
        .with_filter([brand_is(&schema, "acme"), brand_is(&schema, "zeta")]);
    let root = compiled.root().unwrap().clone();
    assert_eq!(root.filter.len(), 2);
    let step = one(&compiled.query, Some(&compiled));
    assert_eq!(
        step.query,
        Query::Bool(BoolQuery {
            filter: Vec::new(),
            ..root.clone()
        })
    );
    // The excluded clauses stay excluded.
    assert_eq!(root.must_not.len(), 2);
    assert_eq!(step.applied, "filters");
    // No word changes: L0's text.
    assert_eq!(step.effective, Some(compiled.effective()));
    assert_eq!(
        step.effective.as_deref(),
        Some("-blue red -\"green hat\" shoes")
    );
    assert_eq!(step.corrected, []);
    assert!(step.alternatives.is_none());

    // A one-word compilation wrapped with a filter is the word alone again, in its `bool`.
    let red = compile(&schema, "red");
    let word = red.query.clone();
    let filtered = red.with_filter([brand_is(&schema, "acme")]);
    let step = one(&filtered.query, Some(&filtered));
    assert_eq!(
        step.query,
        Query::Bool(BoolQuery {
            must: vec![word],
            ..BoolQuery::default()
        })
    );
    assert_eq!(step.effective.as_deref(), Some("red"));
}

#[test]
fn a_query_without_root_filters_builds_no_step() {
    let schema = schema();
    for text in ["red shoes", "red", "-blue red", ""] {
        let compiled = compile(&schema, text);
        assert_eq!(steps(&compiled.query, Some(&compiled)), [], "{text:?}");
    }
    let body = id(&schema, "body");
    let red = Query::Term(TermQuery {
        field: body,
        term: "red".into(),
    });
    assert_eq!(steps(&red, None), []);
    assert_eq!(steps(&Query::MatchAll, None), []);
    // A filter deeper than the root is not the request's: it stays, and the root has none to drop.
    let deep = Query::Bool(BoolQuery {
        must: vec![Query::Bool(BoolQuery {
            must: vec![red.clone()],
            filter: vec![brand_is(&schema, "acme")],
            ..BoolQuery::default()
        })],
        ..BoolQuery::default()
    });
    assert_eq!(steps(&deep, None), []);
    let boosted = Query::Boost(crate::query::BoostQuery {
        query: Box::new(Query::Bool(BoolQuery {
            must: vec![red],
            filter: vec![brand_is(&schema, "acme")],
            ..BoolQuery::default()
        })),
        boost: 2.0,
    });
    assert_eq!(steps(&boosted, None), []);
}

#[test]
fn a_programmatic_root_bool_drops_only_its_filters() {
    let schema = schema();
    let body = id(&schema, "body");
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: body,
            term: t.into(),
        })
    };
    let nested_filter = Query::Bool(BoolQuery {
        must: vec![term("hat")],
        filter: vec![brand_is(&schema, "deep")],
        ..BoolQuery::default()
    });
    let root = BoolQuery {
        must: vec![term("red"), nested_filter],
        should: vec![term("shoes")],
        must_not: vec![term("blue")],
        filter: vec![brand_is(&schema, "acme")],
        minimum_should_match: 1,
    };
    let step = one(&Query::Bool(root.clone()), None);
    assert_eq!(
        step.query,
        Query::Bool(BoolQuery {
            filter: Vec::new(),
            ..root
        })
    );
    assert_eq!(step.effective, None);
}

#[test]
fn the_filter_step_never_fills_and_l5_covers_every_earlier_step() {
    assert!(!Filters.fills());
    assert!(!Filters.covers_earlier());
    assert!(!Filters.nested());
    assert!(super::super::fallback::Fallback.covers_earlier());
    assert!(super::super::fallback::Fallback.fills());
    assert!(!super::super::drop_terms::DropTerms.covers_earlier());
}

#[test]
fn a_query_without_words_builds_no_step() {
    let schema = schema();
    // An empty `q`, one of excluded clauses alone, and stopwords only: no word to match without the filter, which would
    // otherwise be every live document the exclusions leave (the unfiltered L5 ADR-0047 rejects).
    for text in ["", "-blue", "-blue -\"green hat\""] {
        let compiled = compile(&schema, text).with_filter([brand_is(&schema, "acme")]);
        assert!(
            compiled.root().is_some_and(|r| !r.filter.is_empty()),
            "{text:?}"
        );
        assert_eq!(steps(&compiled.query, Some(&compiled)), [], "{text:?}");
    }
    // A programmatic root of filters, or of `match_all` and filters, alike.
    for must in [Vec::new(), vec![Query::MatchAll]] {
        let root = Query::Bool(BoolQuery {
            must,
            must_not: vec![Query::Term(TermQuery {
                field: id(&schema, "body"),
                term: "blue".into(),
            })],
            filter: vec![brand_is(&schema, "acme")],
            ..BoolQuery::default()
        });
        assert_eq!(steps(&root, None), []);
    }
}

#[test]
fn a_root_of_should_clauses_keeps_them_and_one_is_then_required() {
    let schema = schema();
    let body = id(&schema, "body");
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: body,
            term: t.into(),
        })
    };
    // Under L0 the filter makes the `should` clauses optional (they only score); without it, one is required, as a
    // `bool` of `should` clauses alone requires: the step matches documents holding a word, not every document.
    let root = BoolQuery {
        should: vec![term("red"), term("shoes")],
        filter: vec![brand_is(&schema, "acme")],
        ..BoolQuery::default()
    };
    let step = one(&Query::Bool(root.clone()), None);
    assert_eq!(
        step.query,
        Query::Bool(BoolQuery {
            filter: Vec::new(),
            ..root
        })
    );
}
