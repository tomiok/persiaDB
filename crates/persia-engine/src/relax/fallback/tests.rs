//! L5's step (SPEC §7.6, ROADMAP 8.2.5), as built from a query: what it keeps of L0's.
//!
//! Invariants:
//! 1. One step, `fallback`, whose query is L0's root `must_not` and `filter` around `match_all` (`match_all` alone when
//!    there are neither), for a compiled query and a programmatic root `bool` alike; any other root keeps nothing.
//! 2. Its `effective` is the excluded clauses alone, in text order, as written (empty without one); a programmatic
//!    query has none. It corrects nothing and has no synonym alternatives. The level reads no statistics.
//! 3. A compiled query keeps its request filter (`Compiled::filter`, ROADMAP 8.2.14) whatever query a caller put in:
//!    beside the new root `bool`'s `must_not` and `filter`, or alone around `match_all` for a root of another kind.
//!    A caller's root the join wrapped keeps its own `must_not` and `filter` too (ROADMAP 8.2.13).
//! 4. A filter L0 holds in a clause L5 drops (below the root, outside its `must_not`, and outside a caller's wrapped
//!    root's `must_not` and `filter`) means no step: L5 never selects a document a filter of L0 rejects.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)] // Tests: inputs are fixed.

use persia_analysis::AnalyzerRegistry;

use super::*;
use crate::query::{Compiled, DefaultFields, FilterValue, QueryCompiler, QueryLimits, TermQuery};
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

/// A provider L5 never reads: it scores nothing.
struct NoStats;

impl StatisticsProvider for NoStats {
    fn doc_count(&self) -> u64 {
        panic!("L5 reads no statistics")
    }
    fn field(&self, _: FieldId) -> crate::stats::FieldTotals {
        panic!("L5 reads no statistics")
    }
    fn df(&self, _: FieldId, _: &[u8]) -> Result<u64, ReadError> {
        panic!("L5 reads no statistics")
    }
}

fn steps(query: &Query, compiled: Option<&Compiled>) -> Vec<Step> {
    Fallback
        .steps(&LevelInput::new(query, compiled, &NoStats))
        .unwrap()
}

fn one(query: &Query, compiled: Option<&Compiled>) -> Step {
    let mut steps = steps(query, compiled);
    assert_eq!(steps.len(), 1, "L5 builds one step");
    steps.remove(0)
}

fn brand_is(schema: &CollectionSchema, value: &str) -> Filter {
    Filter::Term {
        field: id(schema, "brand"),
        value: FilterValue::Keyword(value.into()),
    }
}

#[test]
fn a_query_without_exclusions_or_filters_falls_back_to_every_document() {
    let schema = schema();
    let compiled = compile(&schema, "red shoes");
    let step = one(compiled.query(), Some(&compiled));
    assert_eq!(step.query, Query::MatchAll);
    assert_eq!(step.applied, "fallback");
    // No word is searched: the text is empty, not L0's.
    assert_eq!(step.effective.as_deref(), Some(""));
    assert_eq!(step.corrected, []);
    assert!(step.alternatives.is_none());
    // One word compiles to the word itself, with no root: nothing to keep either.
    let compiled = compile(&schema, "red");
    assert_eq!(
        one(compiled.query(), Some(&compiled)).query,
        Query::MatchAll
    );
}

#[test]
fn excluded_clauses_and_filters_are_kept_and_the_text_is_the_exclusions() {
    let schema = schema();
    let compiled =
        compile(&schema, "-blue red -\"green hat\" shoes").with_filter([brand_is(&schema, "acme")]);
    let root = compiled.root().unwrap().clone();
    let step = one(compiled.query(), Some(&compiled));
    assert_eq!(
        step.query,
        Query::Bool(BoolQuery {
            must: vec![Query::MatchAll],
            must_not: root.must_not.clone(),
            filter: root.filter.clone(),
            ..BoolQuery::default()
        })
    );
    assert_eq!(root.must_not.len(), 2);
    assert_eq!(root.filter.len(), 1);
    // The excluded clauses as written, in text order; the required words are gone.
    assert_eq!(step.effective.as_deref(), Some("-blue -\"green hat\""));

    // A one-word compilation wrapped with a filter keeps the filter.
    let red = compile(&schema, "red").with_filter([brand_is(&schema, "acme")]);
    let step = one(red.query(), Some(&red));
    assert_eq!(
        step.query,
        Query::Bool(BoolQuery {
            must: vec![Query::MatchAll],
            filter: vec![brand_is(&schema, "acme")],
            ..BoolQuery::default()
        })
    );
    assert_eq!(step.effective.as_deref(), Some(""));
}

#[test]
fn a_programmatic_root_bool_keeps_its_exclusions_and_filters_and_has_no_text() {
    let schema = schema();
    let body = id(&schema, "body");
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: body,
            term: t.into(),
        })
    };
    // Its `should` clauses and `minimum_should_match` are dropped with its words; what it excludes and filters is not.
    let root = Query::Bool(BoolQuery {
        must: vec![term("red")],
        should: vec![term("shoes"), term("hat")],
        must_not: vec![term("blue")],
        filter: vec![brand_is(&schema, "acme")],
        minimum_should_match: 1,
    });
    let step = one(&root, None);
    assert_eq!(
        step.query,
        Query::Bool(BoolQuery {
            must: vec![Query::MatchAll],
            must_not: vec![term("blue")],
            filter: vec![brand_is(&schema, "acme")],
            ..BoolQuery::default()
        })
    );
    assert_eq!(step.effective, None);
    // Any other root has neither.
    assert_eq!(one(&term("red"), None).query, Query::MatchAll);
    assert_eq!(one(&Query::MatchAll, None).query, Query::MatchAll);
}

#[test]
fn a_replaced_query_keeps_the_request_filter() {
    let schema = schema();
    let body = id(&schema, "body");
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: body,
            term: t.into(),
        })
    };
    let acme = brand_is(&schema, "acme");
    let filtered = compile(&schema, "red shoes -blue").with_filter([acme.clone()]);
    let around = |must_not: Vec<Query>, filter: Vec<Filter>| {
        Query::Bool(BoolQuery {
            must: vec![Query::MatchAll],
            must_not,
            filter,
            ..BoolQuery::default()
        })
    };
    // A term, `match_all`, a `bool` of `should` clauses, a `boost`: none holds the clauses' parts, and each is joined
    // to the request filter again, so L5 keeps it (before ROADMAP 8.2.14, L5 was `match_all`).
    let should = Query::Bool(BoolQuery {
        should: vec![term("hat"), term("socks")],
        must_not: vec![term("green")],
        ..BoolQuery::default()
    });
    let boost = Query::Boost(crate::query::BoostQuery {
        query: Box::new(term("hat")),
        boost: 2.0,
    });
    for (caller, must_not) in [
        (term("hat"), Vec::new()),
        (Query::MatchAll, Vec::new()),
        (should, vec![term("green")]),
        (boost, Vec::new()),
    ] {
        let replaced = filtered.clone().with_query(caller.clone());
        assert_eq!(replaced.root(), None, "{caller:?}");
        let step = one(replaced.query(), Some(&replaced));
        // The `should` root the join wrapped keeps its own exclusion (ROADMAP 8.2.13): it is one level down, but L0's.
        assert_eq!(
            step.query,
            around(must_not, vec![acme.clone()]),
            "{caller:?}"
        );
        // The text is still the user's: the excluded clause as written.
        assert_eq!(step.effective.as_deref(), Some("-blue"));
    }
    // A root `bool` of the caller's own keeps its exclusions and filters beside the request filter.
    let own = brand_is(&schema, "other");
    let replaced = filtered.clone().with_query(Query::Bool(BoolQuery {
        must: vec![term("hat")],
        must_not: vec![term("green")],
        filter: vec![own.clone()],
        ..BoolQuery::default()
    }));
    assert_eq!(
        one(replaced.query(), Some(&replaced)).query,
        around(vec![term("green")], vec![own, acme.clone()])
    );
    // Read from the compilation, not only from the root: a root that lacks it still gets it.
    assert_eq!(
        one(&term("hat"), Some(&filtered)).query,
        around(Vec::new(), vec![acme])
    );
}

/// ROADMAP 8.2.13: a caller's root the join wrapped (it has `should` clauses) keeps its own exclusions and filters at
/// L5, beside the request's.
#[test]
fn a_wrapped_caller_root_keeps_its_own_exclusions_and_filters() {
    let schema = schema();
    let body = id(&schema, "body");
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: body,
            term: t.into(),
        })
    };
    let (acme, own) = (brand_is(&schema, "acme"), brand_is(&schema, "other"));
    let filtered = compile(&schema, "red shoes").with_filter([acme.clone()]);
    let caller = Query::Bool(BoolQuery {
        must: vec![term("red")],
        should: vec![term("hat")],
        must_not: vec![term("green")],
        filter: vec![own.clone(), acme.clone()],
        ..BoolQuery::default()
    });
    let replaced = filtered.with_query(caller.clone());
    let Query::Bool(wrap) = replaced.query() else {
        panic!()
    };
    assert_eq!(wrap.must, [caller]);
    assert_eq!(
        one(replaced.query(), Some(&replaced)).query,
        Query::Bool(BoolQuery {
            must: vec![Query::MatchAll],
            must_not: vec![term("green")],
            filter: vec![acme, own],
            ..BoolQuery::default()
        })
    );
}

/// ROADMAP 8.2.13: a filter in a clause L5 drops would not hold under `match_all`, so L5 builds no step. A filter inside
/// an exclusion, or in a caller's required clause, is kept with it.
#[test]
fn a_filter_below_the_root_in_a_dropped_clause_builds_no_step() {
    let schema = schema();
    let body = id(&schema, "body");
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: body,
            term: t.into(),
        })
    };
    let acme = brand_is(&schema, "acme");
    let filtered = |q: Query| {
        Query::Bool(BoolQuery {
            must: vec![q],
            filter: vec![acme.clone()],
            ..BoolQuery::default()
        })
    };
    let boost = |q: Query| {
        Query::Boost(crate::query::BoostQuery {
            query: Box::new(q),
            boost: 2.0,
        })
    };
    // Programmatic: a `boost` of a filtered `bool`, a `constant_score` of one, a filtered `bool` as a `must` or a
    // `should` clause, deeper still.
    let nested = [
        boost(filtered(term("red"))),
        Query::ConstantScore(crate::query::ConstantScoreQuery {
            query: Box::new(filtered(term("red"))),
            score: 1.0,
        }),
        Query::Bool(BoolQuery {
            must: vec![term("shoes"), filtered(term("red"))],
            ..BoolQuery::default()
        }),
        Query::Bool(BoolQuery {
            should: vec![term("shoes"), boost(filtered(term("red")))],
            ..BoolQuery::default()
        }),
        Query::Bool(BoolQuery {
            must: vec![Query::Bool(BoolQuery {
                must: vec![boost(filtered(term("red")))],
                ..BoolQuery::default()
            })],
            ..BoolQuery::default()
        }),
    ];
    for query in &nested {
        assert_eq!(steps(query, None), [], "{query:?}");
    }
    // Compiled, a query put in: one that holds such a filter, in place or wrapped, builds none either.
    let compiled = compile(&schema, "red shoes").with_filter([brand_is(&schema, "tenant")]);
    for query in &nested {
        let replaced = compiled.clone().with_query(query.clone());
        assert_eq!(steps(replaced.query(), Some(&replaced)), [], "{query:?}");
    }
    let wrapped = compiled.clone().with_query(Query::Bool(BoolQuery {
        must: vec![filtered(term("red"))],
        should: vec![term("hat")],
        ..BoolQuery::default()
    }));
    assert_eq!(steps(wrapped.query(), Some(&wrapped)), []);
    // A filter inside an exclusion is kept with it; so is one in a caller's required clause after the clauses.
    let excluded = Query::Bool(BoolQuery {
        must: vec![term("red")],
        must_not: vec![filtered(term("blue"))],
        ..BoolQuery::default()
    });
    assert_eq!(
        one(&excluded, None).query,
        Query::Bool(BoolQuery {
            must: vec![Query::MatchAll],
            must_not: vec![filtered(term("blue"))],
            ..BoolQuery::default()
        })
    );
    let Query::Bool(mut root) = compile(&schema, "red shoes").query().clone() else {
        panic!()
    };
    root.must.push(filtered(term("hat")));
    let extra = compile(&schema, "red shoes").with_query(Query::Bool(root));
    assert_eq!(extra.extra_must(), [filtered(term("hat"))]);
    assert_eq!(
        one(extra.query(), Some(&extra)).query,
        Query::Bool(BoolQuery {
            must: vec![filtered(term("hat"))],
            ..BoolQuery::default()
        })
    );
}
