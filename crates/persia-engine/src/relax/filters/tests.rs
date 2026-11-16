//! The filter step (SPEC §7.6, ROADMAP 8.2.6, ADR-0047), as built from a query: L0's without its root filters.
//!
//! Invariants:
//! 1. One step, `filters`, whose query is L0's root `bool` with its `filter` emptied, every other clause as it is
//!    (`must`, `should`, `must_not`, `minimum_should_match`), for a compiled query and a programmatic root `bool` alike.
//! 2. A root without filters, a root that is not a `bool`, and a root that searches no word, build no step: the level
//!    is skipped. A word is a `term`, `combined`, `phrase`, `prefix` or `fuzzy`, or one of those inside `must` or
//!    `should` through `bool`, `boost` and `constant_score`. `match_all`, an empty `bool` and exclusions alone are not.
//! 3. Its `effective` is L0's text; it corrects nothing, has no synonym alternatives and reads no statistics.
//! 4. It never fills a page, and covers no earlier step; L5 covers every earlier step.
//! 5. Of a compiled query it drops the request filter alone (`Compiled::filter`, ROADMAP 8.2.14), whatever query a
//!    caller put in: a filter the caller's root holds stays, and without a request filter no step is built.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)] // Tests: inputs are fixed.

use persia_analysis::AnalyzerRegistry;
use proptest::prelude::*;

use super::*;
use crate::query::{
    BoostQuery, ConstantScoreQuery, DefaultFields, Filter, FilterValue, Query, QueryCompiler,
    QueryLimits, TermQuery,
};
use crate::schema::{
    AnalyzerRef, CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
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

fn term(schema: &CollectionSchema, text: &str) -> Query {
    Query::Term(TermQuery {
        field: id(schema, "body"),
        term: text.into(),
    })
}

fn boost(query: Query) -> Query {
    Query::Boost(BoostQuery {
        query: Box::new(query),
        boost: 2.0,
    })
}

fn constant(query: Query) -> Query {
    Query::ConstantScore(ConstantScoreQuery {
        query: Box::new(query),
        score: 1.0,
    })
}

/// `clause` as the one positive clause of a root `bool` that also filters on `brand`.
fn filtered(schema: &CollectionSchema, must: bool, clause: Query) -> Query {
    let (must, should) = if must {
        (vec![clause], Vec::new())
    } else {
        (Vec::new(), vec![clause])
    };
    Query::Bool(BoolQuery {
        must,
        should,
        filter: vec![brand_is(schema, "acme")],
        ..BoolQuery::default()
    })
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
    let step = one(compiled.query(), Some(&compiled));
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
    let word = red.query().clone();
    let filtered = red.with_filter([brand_is(&schema, "acme")]);
    let step = one(filtered.query(), Some(&filtered));
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
fn a_replaced_query_drops_the_request_filter_alone() {
    let schema = schema();
    let acme = brand_is(&schema, "acme");
    let own = brand_is(&schema, "other");
    let filtered = compile(&schema, "red shoes").with_filter([acme.clone()]);
    // A term put in place of the compiled query is wrapped to keep the request filter; the step is the wrap without it.
    let replaced = filtered.clone().with_query(term(&schema, "hat"));
    assert_eq!(
        one(replaced.query(), Some(&replaced)).query,
        Query::Bool(BoolQuery {
            must: vec![term(&schema, "hat")],
            ..BoolQuery::default()
        })
    );
    // A root `bool` of the caller's own keeps its own filter: only the request's is dropped.
    let caller = Query::Bool(BoolQuery {
        must: vec![term(&schema, "hat")],
        must_not: vec![term(&schema, "blue")],
        filter: vec![own.clone()],
        ..BoolQuery::default()
    });
    let replaced = filtered.clone().with_query(caller.clone());
    let Query::Bool(root) = replaced.query() else {
        panic!()
    };
    assert_eq!(root.filter, [own.clone(), acme.clone()]);
    assert_eq!(one(replaced.query(), Some(&replaced)).query, caller);
    // So does one that already held the request filter, which is not joined twice: a root filter equal to the request
    // filter counts as the request's, and is dropped.
    let holding = Query::Bool(BoolQuery {
        must: vec![term(&schema, "hat")],
        filter: vec![acme.clone(), own.clone()],
        ..BoolQuery::default()
    });
    let replaced = filtered.clone().with_query(holding);
    let Query::Bool(root) = replaced.query() else {
        panic!()
    };
    assert_eq!(root.filter, [acme.clone(), own.clone()]);
    assert_eq!(
        one(replaced.query(), Some(&replaced)).query,
        Query::Bool(BoolQuery {
            must: vec![term(&schema, "hat")],
            filter: vec![own.clone()],
            ..BoolQuery::default()
        })
    );
    // Without a request filter, a caller's own root filter is part of the query: nothing to relax.
    let unfiltered = compile(&schema, "red shoes").with_query(caller);
    assert_eq!(steps(unfiltered.query(), Some(&unfiltered)), []);
    // A wordless query put in builds no step either, request filter or not.
    let wordless = filtered.with_query(Query::MatchAll);
    assert_eq!(steps(wordless.query(), Some(&wordless)), []);
}

#[test]
fn a_query_without_root_filters_builds_no_step() {
    let schema = schema();
    for text in ["red shoes", "red", "-blue red", ""] {
        let compiled = compile(&schema, text);
        assert_eq!(steps(compiled.query(), Some(&compiled)), [], "{text:?}");
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
    let boosted = Query::Boost(BoostQuery {
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
    // An empty `q` and excluded clauses alone: no word to match without the filter, which would otherwise be every
    // live document the exclusions leave (the unfiltered L5 ADR-0047 rejects). Stopwords are the `en` case below:
    // this schema's `standard` analyzer keeps `the`.
    for text in ["", "-blue", "-blue -\"green hat\""] {
        let compiled = compile(&schema, text).with_filter([brand_is(&schema, "acme")]);
        assert!(
            compiled.root().is_some_and(|r| !r.filter.is_empty()),
            "{text:?}"
        );
        assert_eq!(steps(compiled.query(), Some(&compiled)), [], "{text:?}");
    }
    let blue = term(&schema, "blue");
    // Each of these searches no word, in `must` and in `should`. A non-empty `should` is not itself a word:
    // `match_all` matches every document.
    let wordless = [
        Query::MatchAll,
        Query::Bool(BoolQuery::default()),
        Query::Bool(BoolQuery {
            must_not: vec![blue.clone()],
            ..BoolQuery::default()
        }),
        boost(Query::MatchAll),
        constant(Query::MatchAll),
        boost(constant(Query::MatchAll)),
        constant(boost(Query::MatchAll)),
        Query::Bool(BoolQuery {
            should: vec![Query::MatchAll],
            ..BoolQuery::default()
        }),
        Query::Bool(BoolQuery {
            must: vec![Query::Bool(BoolQuery::default())],
            ..BoolQuery::default()
        }),
        boost(Query::Bool(BoolQuery {
            must_not: vec![blue],
            ..BoolQuery::default()
        })),
    ];
    for clause in wordless {
        for must in [true, false] {
            let root = filtered(&schema, must, clause.clone());
            assert_eq!(steps(&root, None), [], "must {must}: {root:?}");
        }
    }
    // The query itself is not a root `bool`: nothing to drop, even when the wrapped `bool` has a filter.
    assert_eq!(steps(&boost(Query::MatchAll), None), []);
    assert_eq!(steps(&constant(Query::MatchAll), None), []);
}

#[test]
fn stopwords_only_build_no_step() {
    let schema = SchemaBuilder::new("c")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("en").unwrap(),
                ..TextOptions::default()
            }),
        )
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    // `the` and `a` analyze to no token, so the compilation is `match_all` and the filter wraps it. That is still
    // no word: dropping the filter would return every live document.
    for text in ["the", "the a of", "a and the"] {
        let compiled = compile(&schema, text).with_filter([brand_is(&schema, "acme")]);
        assert!(
            matches!(
                compiled.query(),
                Query::Bool(root) if root.must == [Query::MatchAll] && !root.filter.is_empty()
            ),
            "{text:?} compiled to {:?}",
            compiled.query()
        );
        assert_eq!(steps(compiled.query(), Some(&compiled)), [], "{text:?}");
    }
    // A stopword beside a real word still searches that word.
    let red = compile(&schema, "the red").with_filter([brand_is(&schema, "acme")]);
    assert_eq!(steps(red.query(), Some(&red)).len(), 1);
}

#[test]
fn a_word_inside_boost_constant_score_or_a_nested_bool_still_builds_the_step() {
    let schema = schema();
    let red = term(&schema, "red");
    let wordy = [
        boost(red.clone()),
        constant(red.clone()),
        Query::Bool(BoolQuery {
            must: vec![red.clone()],
            ..BoolQuery::default()
        }),
        boost(Query::Bool(BoolQuery {
            should: vec![red.clone()],
            ..BoolQuery::default()
        })),
        // A wordless sibling does not hide the term.
        Query::Bool(BoolQuery {
            should: vec![Query::MatchAll, red.clone()],
            ..BoolQuery::default()
        }),
        Query::Bool(BoolQuery {
            must: vec![red, Query::MatchAll],
            ..BoolQuery::default()
        }),
    ];
    for clause in wordy {
        let root = filtered(&schema, true, clause);
        let step = one(&root, None);
        let Query::Bool(built) = step.query else {
            panic!("the step is the root bool");
        };
        let Query::Bool(source) = &root else {
            panic!("filtered() builds a bool");
        };
        assert_eq!(built.filter, []);
        assert_eq!(&built.must, &source.must);
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

/// A clause built only from `match_all`, empty `bool`s and exclusions, through `boost`, `constant_score` and `bool`.
fn wordless_clause() -> impl Strategy<Value = Query> {
    let leaf = prop_oneof![
        Just(Query::MatchAll),
        Just(Query::Bool(BoolQuery::default())),
        Just(Query::Bool(BoolQuery {
            must_not: vec![Query::Term(TermQuery {
                field: FieldId::new(0),
                term: "blue".into(),
            })],
            ..BoolQuery::default()
        })),
    ];
    leaf.prop_recursive(3, 12, 2, |inner| {
        prop_oneof![
            inner.clone().prop_map(boost),
            inner.clone().prop_map(constant),
            inner.clone().prop_map(|q| Query::Bool(BoolQuery {
                must: vec![q],
                ..BoolQuery::default()
            })),
            inner.clone().prop_map(|q| Query::Bool(BoolQuery {
                should: vec![q],
                ..BoolQuery::default()
            })),
            (inner.clone(), inner).prop_map(|(a, b)| Query::Bool(BoolQuery {
                must: vec![a],
                should: vec![b],
                ..BoolQuery::default()
            })),
        ]
    })
}

/// A clause that holds `red` in `must` or `should`, through the same wrappers, beside wordless siblings.
fn wordy_clause() -> impl Strategy<Value = Query> {
    let red = Query::Term(TermQuery {
        field: FieldId::new(0),
        term: "red".into(),
    });
    Just(red).prop_recursive(3, 12, 2, |inner| {
        prop_oneof![
            inner.clone().prop_map(boost),
            inner.clone().prop_map(constant),
            inner.clone().prop_map(|q| Query::Bool(BoolQuery {
                must: vec![q, Query::MatchAll],
                ..BoolQuery::default()
            })),
            inner.prop_map(|q| Query::Bool(BoolQuery {
                should: vec![Query::Bool(BoolQuery::default()), q],
                ..BoolQuery::default()
            })),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// A root whose positive clauses search no word never builds L6, filter or not (SPEC §7.6).
    #[test]
    fn a_wordless_query_never_builds_the_filter_step(clause in wordless_clause(), in_must in any::<bool>()) {
        let schema = schema();
        let root = filtered(&schema, in_must, clause);
        prop_assert_eq!(steps(&root, None), []);
    }

    /// Wrapping a term in `boost`, `constant_score` or a nested `bool` still searches that word, so the step drops
    /// the root filter and keeps the clause.
    #[test]
    fn a_query_that_searches_a_word_drops_its_root_filter(clause in wordy_clause()) {
        let schema = schema();
        let root = filtered(&schema, true, clause);
        let built = steps(&root, None);
        prop_assert_eq!(built.len(), 1);
        let Query::Bool(query) = &built[0].query else {
            return Err(TestCaseError::fail("the step is the root bool"));
        };
        let Query::Bool(source) = &root else {
            return Err(TestCaseError::fail("filtered() builds a bool"));
        };
        prop_assert_eq!(query.filter.as_slice(), [].as_slice());
        prop_assert_eq!(query.must.as_slice(), source.must.as_slice());
    }
}
