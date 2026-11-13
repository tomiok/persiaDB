#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::panic
)] // Tests: inputs are fixed; IDFs are exact.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::query::{
    BoostQuery, CombinedQuery, ConstantScoreQuery, PhraseQuery, PhraseTerm, PrefixQuery, TermQuery,
    WeightedTerm,
};
use crate::schema::FieldId;
use crate::stats::FieldTotals;

/// 100 documents; each term's `df` as given, 0 for any other.
struct Stats(BTreeMap<(u32, &'static str), u64>);

impl StatisticsProvider for Stats {
    fn doc_count(&self) -> u64 {
        100
    }
    fn field(&self, _: FieldId) -> FieldTotals {
        FieldTotals::default()
    }
    fn df(&self, field: FieldId, term: &[u8]) -> Result<u64, ReadError> {
        let term = std::str::from_utf8(term).unwrap();
        Ok(self
            .0
            .iter()
            .find(|((f, t), _)| FieldId::new(*f) == field && *t == term)
            .map_or(0, |(_, df)| *df))
    }
}

fn stats() -> Stats {
    Stats(BTreeMap::from([
        ((0, "red"), 50),
        ((0, "shoes"), 10),
        ((1, "shoes"), 30),
        ((0, "nike"), 2),
        // Only a filter holds it: an IDF that counted filters would change.
        ((2, "acme"), 1),
    ]))
}

fn term(field: u32, term: &str) -> Query {
    Query::Term(TermQuery {
        field: FieldId::new(field),
        term: term.into(),
    })
}

fn importance(stopword: bool, idf: f64, position: usize) -> Importance {
    Importance {
        stopword,
        idf,
        position,
    }
}

#[test]
fn stopwords_drop_first_then_lower_idf_then_later_position() {
    let terms = [
        importance(false, 2.0, 0),
        importance(true, 9.0, 1),
        importance(false, 1.0, 2),
        importance(false, 2.0, 3),
        importance(true, 0.5, 4),
    ];
    // Stopwords first (lower IDF first among them), then IDF 1.0, then the two at 2.0, the later one first.
    assert_eq!(drop_order(&terms), [4, 1, 2, 3, 0]);
    assert_eq!(drop_order(&[]), Vec::<usize>::new());
}

proptest! {
    /// The order is a permutation in which no term is more important than one dropped after it.
    #[test]
    fn drop_order_never_drops_a_more_important_term_first(
        terms in prop::collection::vec((any::<bool>(), 0_u8..4), 0..12),
    ) {
        let terms: Vec<Importance> = terms
            .iter()
            .enumerate()
            .map(|(position, &(stopword, idf))| importance(stopword, f64::from(idf), position))
            .collect();
        let order = drop_order(&terms);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        prop_assert_eq!(sorted, (0..terms.len()).collect::<Vec<_>>());
        for w in order.windows(2) {
            let (a, b) = (terms[w[0]], terms[w[1]]);
            prop_assert_eq!(a.compare(&b), Ordering::Less, "{:?} before {:?}", a, b);
            prop_assert!(a.stopword || !b.stopword, "a stopword after a term that is not one");
            if a.stopword == b.stopword {
                prop_assert!(a.idf < b.idf || (a.idf == b.idf && a.position > b.position));
            }
        }
    }
}

#[test]
fn query_idf_is_the_idf_bm25f_scores_with() {
    let stats = stats();
    let n = 100;
    assert_eq!(query_idf(&term(0, "red"), &stats).unwrap(), idf(n, 50));
    assert_eq!(query_idf(&term(0, "absent"), &stats).unwrap(), idf(n, 0));
    // `combined` takes the largest `df` of its terms.
    let combined = Query::Combined(CombinedQuery {
        fields: [(0, 3.0), (1, 1.0)]
            .map(|(f, weight)| WeightedTerm {
                field: FieldId::new(f),
                term: "shoes".into(),
                weight,
            })
            .into(),
    });
    assert_eq!(query_idf(&combined, &stats).unwrap(), idf(n, 30));
    // A phrase sums its terms'.
    let phrase = Query::Phrase(PhraseQuery {
        field: FieldId::new(0),
        terms: ["red", "shoes"]
            .iter()
            .zip(0..)
            .map(|(t, position)| PhraseTerm {
                term: (*t).into(),
                position,
            })
            .collect(),
    });
    assert_eq!(query_idf(&phrase, &stats).unwrap(), idf(n, 50) + idf(n, 10));
    // A bool sums `must` and adds its best `should`; `must_not` and `filter` do not score.
    let bool = Query::Bool(BoolQuery {
        must: vec![term(0, "red"), term(0, "shoes")],
        should: vec![term(0, "red"), term(0, "nike")],
        must_not: vec![term(0, "absent")],
        filter: vec![Filter::Term {
            field: FieldId::new(2),
            value: crate::query::FilterValue::Keyword("acme".into()),
        }],
        ..BoolQuery::default()
    });
    assert_eq!(
        query_idf(&bool, &stats).unwrap(),
        idf(n, 50) + idf(n, 10) + idf(n, 2)
    );
    // `boost` and `constant_score` are their query's; `prefix` and `match_all` have none.
    let boosted = Query::Boost(BoostQuery {
        query: Box::new(term(0, "nike")),
        boost: 5.0,
    });
    assert_eq!(query_idf(&boosted, &stats).unwrap(), idf(n, 2));
    let constant = Query::ConstantScore(ConstantScoreQuery {
        query: Box::new(term(0, "nike")),
        score: 1.0,
    });
    assert_eq!(query_idf(&constant, &stats).unwrap(), idf(n, 2));
    let prefix = Query::Prefix(PrefixQuery {
        field: FieldId::new(0),
        prefix: "ni".into(),
        max_expansions: 50,
    });
    assert_eq!(query_idf(&prefix, &stats).unwrap(), 0.0);
    assert_eq!(query_idf(&Query::MatchAll, &stats).unwrap(), 0.0);
}

#[test]
fn query_idf_reports_a_df_it_cannot_read() {
    struct Broken;
    impl StatisticsProvider for Broken {
        fn doc_count(&self) -> u64 {
            1
        }
        fn field(&self, _: FieldId) -> FieldTotals {
            FieldTotals::default()
        }
        fn df(&self, field: FieldId, _: &[u8]) -> Result<u64, ReadError> {
            Err(ReadError::UnknownField { field })
        }
    }
    let nested = Query::Bool(BoolQuery {
        should: vec![term(0, "red")],
        ..BoolQuery::default()
    });
    assert!(query_idf(&nested, &Broken).is_err());
}

#[test]
fn the_ladder_counts_each_percentage_once_below_l0() {
    // SPEC §7.6: `max(1, ⌊p · n / 100⌋)` for 75% and 50%, then 1; a count that repeats the previous (L0's `n` first) is
    // not built.
    assert_eq!(ladder(0), []);
    assert_eq!(ladder(1), []);
    assert_eq!(ladder(2), [("75%", 1)]);
    assert_eq!(ladder(3), [("75%", 2), ("50%", 1)]);
    assert_eq!(ladder(4), [("75%", 3), ("50%", 2), ("1", 1)]);
    assert_eq!(ladder(5), [("75%", 3), ("50%", 2), ("1", 1)]);
    assert_eq!(ladder(8), [("75%", 6), ("50%", 4), ("1", 1)]);
    for n in 0..200 {
        let steps = ladder(n);
        let counts: Vec<usize> = steps.iter().map(|&(_, c)| c).collect();
        assert!(counts.windows(2).all(|w| w[0] > w[1]), "{n}: {counts:?}");
        assert!(counts.iter().all(|&c| 1 <= c && c < n), "{n}: {counts:?}");
        assert_eq!(counts.last().copied(), (n >= 2).then_some(1), "{n}");
        assert_eq!(
            counts.first().copied(),
            (n >= 2).then(|| (n * 3 / 4).max(1)),
            "{n}"
        );
    }
}

mod steps {
    use persia_analysis::AnalyzerRegistry;

    use super::*;
    use crate::query::{DefaultFields, Filter, FilterValue, QueryCompiler, QueryLimits};
    use crate::relax::tests::NoStats;
    use crate::schema::{
        AnalyzerRef, CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
    };

    /// `title` (standard), `body` (en) and `brand` (keyword).
    fn schema() -> CollectionSchema {
        SchemaBuilder::new("c")
            .field("title", FieldKind::Text(TextOptions::default()))
            .field(
                "body",
                FieldKind::Text(TextOptions {
                    analyzer: AnalyzerRef::builtin("en").unwrap(),
                    ..TextOptions::default()
                }),
            )
            .field("brand", FieldKind::Keyword(KeywordOptions::default()))
            .build()
            .unwrap()
    }

    fn title() -> FieldId {
        schema().field("title").unwrap().id()
    }

    fn compile(text: &str) -> Compiled {
        let schema = schema();
        let fields = DefaultFields::parse(&schema, &["title"]).unwrap();
        QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())
            .unwrap()
            .compile(text, &fields, &QueryLimits::default())
    }

    fn steps(compiled: &Compiled, stats: &dyn StatisticsProvider) -> Vec<Step> {
        let input = LevelInput::new(&compiled.query, Some(compiled), stats);
        DropTerms.steps(&input).unwrap()
    }

    fn combined(term: &str) -> Query {
        Query::Combined(CombinedQuery {
            fields: vec![WeightedTerm {
                field: title(),
                term: term.into(),
                weight: 1.0,
            }],
        })
    }

    fn bool_of(query: &Query) -> &BoolQuery {
        match query {
            Query::Bool(b) => b,
            q => panic!("not a bool: {q:?}"),
        }
    }

    fn labels(steps: &[Step]) -> Vec<(&str, u32, Option<&str>)> {
        steps
            .iter()
            .map(|s| match &s.query {
                Query::Bool(b) => (
                    s.applied.as_str(),
                    b.minimum_should_match,
                    s.effective.as_deref(),
                ),
                q => panic!("not a bool: {q:?}"),
            })
            .collect()
    }

    #[test]
    fn each_step_requires_its_count_of_the_terms_most_important_first() {
        // `shoes` is the most common, `red` next, `purple` in no document (the rarest).
        let stats = Stats(BTreeMap::from([
            ((title().get(), "red"), 20),
            ((title().get(), "shoes"), 60),
        ]));
        let compiled = compile("red purple shoes -blue");
        let steps = steps(&compiled, &stats);
        assert_eq!(
            labels(&steps),
            [
                ("drop_terms:75%", 2, Some("red purple -blue")),
                ("drop_terms:50%", 1, Some("purple -blue")),
            ]
        );
        for step in &steps {
            let b = bool_of(&step.query);
            // Any `count` of the terms, most important first; the excluded clause stays excluded.
            assert_eq!(
                b.should,
                [combined("purple"), combined("red"), combined("shoes")]
            );
            assert_eq!(b.must, []);
            assert_eq!(b.must_not, [combined("blue")]);
            assert_eq!(b.filter, []);
        }
    }

    #[test]
    fn a_root_that_lost_an_excluded_clause_is_not_relaxed() {
        // Its steps would drop the exclusion their `effective` (`-blue`) still states.
        let stats = Stats(BTreeMap::new());
        let mut compiled = compile("red purple shoes -blue");
        assert_eq!(steps(&compiled, &stats).len(), 2);
        let Query::Bool(b) = &mut compiled.query else {
            panic!()
        };
        b.must_not.clear();
        assert_eq!(labels(&steps(&compiled, &stats)), []);
    }

    #[test]
    fn a_stopword_drops_before_a_rarer_term_and_ties_drop_the_later_term() {
        // `the` is a stopword of `body` (en), so of a field it searches; in no title, it is the rarest.
        let schema = schema();
        let all = DefaultFields::all(&schema);
        let compiled = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())
            .unwrap()
            .compile("the red hat", &all, &QueryLimits::default());
        assert!(compiled.clauses[0].stopword);
        let stats = Stats(BTreeMap::from([
            ((title().get(), "red"), 20),
            ((title().get(), "hat"), 20),
        ]));
        // `red` and `hat` are as rare: the later, `hat`, drops first.
        assert_eq!(
            labels(&steps(&compiled, &stats)),
            [
                ("drop_terms:75%", 2, Some("red hat")),
                ("drop_terms:50%", 1, Some("red")),
            ]
        );
    }

    #[test]
    fn a_multi_part_clause_is_one_term_and_the_root_keeps_its_filters() {
        // `e-mail` is one clause of two parts: one term, kept whole.
        let compiled = compile("e-mail red");
        assert_eq!(compiled.clauses[0].parts, 0..2);
        let stats = Stats(BTreeMap::from([((title().get(), "red"), 99)]));
        let steps = steps(&compiled, &stats);
        assert_eq!(labels(&steps), [("drop_terms:75%", 1, Some("e-mail"))]);
        let b = bool_of(&steps[0].query);
        assert_eq!(
            b.should,
            [
                Query::Bool(BoolQuery {
                    must: vec![combined("e"), combined("mail")],
                    ..BoolQuery::default()
                }),
                combined("red"),
            ]
        );
        // A programmatic root `bool` keeps its `must_not` and `filter` clauses beside the terms, and states no text.
        let filter = Filter::Term {
            field: schema().field("brand").unwrap().id(),
            value: FilterValue::Keyword("acme".into()),
        };
        let query = Query::Bool(BoolQuery {
            must: vec![combined("red"), combined("hat"), combined("shoes")],
            must_not: vec![combined("blue")],
            filter: vec![filter.clone()],
            ..BoolQuery::default()
        });
        let input = LevelInput::new(&query, None, &NoStats);
        let steps = DropTerms.steps(&input).unwrap();
        // Every IDF ties: the later terms drop first.
        assert_eq!(
            labels(&steps),
            [("drop_terms:75%", 2, None), ("drop_terms:50%", 1, None)]
        );
        let b = bool_of(&steps[1].query);
        assert_eq!(
            b.should,
            [combined("red"), combined("hat"), combined("shoes")]
        );
        assert_eq!(
            (&b.must_not[..], &b.filter[..]),
            (&[combined("blue")][..], &[filter][..])
        );
    }

    #[test]
    fn a_programmatic_bool_with_should_clauses_is_not_relaxed() {
        for query in [
            Query::Bool(BoolQuery {
                must: vec![combined("red"), combined("hat")],
                should: vec![combined("shoes")],
                ..BoolQuery::default()
            }),
            Query::Bool(BoolQuery {
                should: vec![combined("red"), combined("hat"), combined("shoes")],
                minimum_should_match: 3,
                ..BoolQuery::default()
            }),
        ] {
            let input = LevelInput::new(&query, None, &NoStats);
            assert_eq!(DropTerms.steps(&input).unwrap(), [], "{query:?}");
        }
    }

    #[test]
    fn a_df_it_cannot_read_is_an_error() {
        struct Broken;
        impl StatisticsProvider for Broken {
            fn doc_count(&self) -> u64 {
                1
            }
            fn field(&self, _: FieldId) -> FieldTotals {
                FieldTotals::default()
            }
            fn df(&self, field: FieldId, _: &[u8]) -> Result<u64, ReadError> {
                Err(ReadError::UnknownField { field })
            }
        }
        let compiled = compile("red hat");
        let input = LevelInput::new(&compiled.query, Some(&compiled), &Broken);
        assert!(DropTerms.steps(&input).is_err());
    }
}

/// ROADMAP 8.2.2.3: a query of one term, or none, builds no L2 step and reads no statistic.
mod single_term {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use persia_analysis::AnalyzerRegistry;

    use super::*;
    use crate::query::{DefaultFields, QueryCompiler, QueryLimits};
    use crate::schema::{AnalyzerRef, FieldKind, SchemaBuilder, TextOptions};

    /// Counts its `df` reads; every term is in half the documents.
    #[derive(Default)]
    struct Counting(AtomicUsize);

    impl StatisticsProvider for Counting {
        fn doc_count(&self) -> u64 {
            10
        }
        fn field(&self, _: FieldId) -> FieldTotals {
            FieldTotals::default()
        }
        fn df(&self, _: FieldId, _: &[u8]) -> Result<u64, ReadError> {
            self.0.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(5)
        }
    }

    fn steps(query: &Query, compiled: Option<&Compiled>) -> (Vec<Step>, usize) {
        let stats = Counting::default();
        let input = LevelInput::new(query, compiled, &stats);
        let steps = DropTerms.steps(&input).unwrap();
        (steps, stats.0.load(AtomicOrdering::Relaxed))
    }

    #[test]
    fn one_required_clause_whatever_its_parts_skips_the_level() {
        // `title` (standard, positions) and `body` (en): a word over both fields is one term.
        let schema = SchemaBuilder::new("c")
            .field(
                "title",
                FieldKind::Text(TextOptions {
                    positions: true,
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
            .build()
            .unwrap();
        let query_compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        let fields = DefaultFields::all(&schema);
        for text in [
            "shoes",
            "\"red running shoes\"",
            "e-mail",
            "title:red",
            "shoes -red -\"blue hat\"",
            "the",
            "-red",
            "",
        ] {
            let compiled = query_compiler.compile(text, &fields, &QueryLimits::default());
            let required = compiled.clauses.iter().filter(|c| !c.negated).count();
            assert!(required <= 1, "{text:?}");
            assert_eq!(
                steps(&compiled.query, Some(&compiled)),
                (Vec::new(), 0),
                "{text:?}"
            );
        }
        // Two required clauses relax, whatever else is excluded; their `df` is read once each.
        let compiled = query_compiler.compile("shoes red -blue", &fields, &QueryLimits::default());
        let (built, reads) = steps(&compiled.query, Some(&compiled));
        assert_eq!(built.len(), 1);
        assert_eq!(reads, 4, "two words over two fields");
    }

    #[test]
    fn a_programmatic_query_of_one_term_skips_the_level() {
        let red = term(0, "red");
        for query in [
            red.clone(),
            Query::MatchAll,
            Query::Boost(BoostQuery {
                query: Box::new(Query::Bool(BoolQuery {
                    must: vec![red.clone(), term(0, "hat")],
                    ..BoolQuery::default()
                })),
                boost: 2.0,
            }),
            Query::Bool(BoolQuery {
                must: vec![red.clone()],
                must_not: vec![term(0, "blue")],
                ..BoolQuery::default()
            }),
            Query::Bool(BoolQuery {
                must_not: vec![red.clone()],
                ..BoolQuery::default()
            }),
        ] {
            assert_eq!(steps(&query, None), (Vec::new(), 0), "{query:?}");
        }
    }
}

/// Over random user text: every step of L2 is valid, requires a decreasing count of all the required clauses, keeps
/// L0's excluded clauses, and renders as text whose clauses are L0's, in order, `count` of them required.
mod random_text {
    use persia_analysis::AnalyzerRegistry;

    use super::*;
    use crate::query::{DefaultFields, QueryCompiler, QueryLimits};
    use crate::schema::{AnalyzerRef, FieldKind, SchemaBuilder, TextOptions};

    fn clause() -> impl Strategy<Value = String> {
        let word = prop::sample::select(vec![
            "red", "the", "shoes", "e-mail", "hat", "and", "blue", "running",
        ]);
        (
            any::<bool>(),
            prop::option::of(prop::sample::select(vec!["title:", "body:"])),
            prop::collection::vec(word, 1..3),
        )
            .prop_map(|(negated, field, words)| {
                let body = if words.len() > 1 {
                    format!("\"{}\"", words.join(" "))
                } else {
                    words.concat()
                };
                format!(
                    "{}{}{body}",
                    if negated { "-" } else { "" },
                    field.unwrap_or("")
                )
            })
    }

    proptest! {
        #[test]
        fn every_step_relaxes_l0_and_renders_its_clauses(
            clauses in prop::collection::vec(clause(), 0..8),
            dfs in prop::collection::vec(0_u64..100, 8),
        ) {
            let schema = SchemaBuilder::new("c")
                .field("title", FieldKind::Text(TextOptions { positions: true, ..TextOptions::default() }))
                .field("body", FieldKind::Text(TextOptions {
                    analyzer: AnalyzerRef::builtin("en").unwrap(),
                    ..TextOptions::default()
                }))
                .build()
                .unwrap();
            let query_compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
            let fields = DefaultFields::all(&schema);
            let compile = |text: &str| query_compiler.compile(text, &fields, &QueryLimits::default());
            let text = clauses.join(" ");
            let compiled = compile(&text);
            // Each word's `df` drawn per word, the same in every field.
            let words = ["red", "the", "shoes", "e", "mail", "hat", "and", "blue", "running", "shoe", "run"];
            let stats = Stats(
                words.iter().zip(dfs.iter().cycle()).flat_map(|(w, df)| {
                    schema.fields().iter().map(move |f| ((f.id().get(), *w), *df))
                }).collect(),
            );
            let input = LevelInput::new(&compiled.query, Some(&compiled), &stats);
            let steps = DropTerms.steps(&input).unwrap();
            let required = compiled.clauses.iter().filter(|c| !c.negated).count();
            let negated = compiled.clauses.len() - required;
            prop_assert_eq!(steps.len(), ladder(required).len());
            let l0_must_not = match &compiled.query {
                Query::Bool(b) => b.must_not.clone(),
                _ => Vec::new(),
            };
            let sources: Vec<&str> = (0..compiled.clauses.len()).filter_map(|i| compiled.source(i)).collect();
            let mut previous = required;
            for step in &steps {
                step.query.validate(&schema, &QueryLimits::UNLIMITED).unwrap();
                let Query::Bool(b) = &step.query else {
                    panic!("not a bool: {:?}", step.query)
                };
                let count = b.minimum_should_match as usize;
                prop_assert!(1 <= count && count < previous, "{} after {}", count, previous);
                previous = count;
                prop_assert_eq!(b.should.len(), required);
                prop_assert_eq!(&b.must_not, &l0_must_not);
                prop_assert!(b.must.is_empty() && b.filter.is_empty());
                // The text recompiles to `count` required clauses and every excluded one, each one of L0's, in order.
                let effective = step.effective.as_deref().unwrap();
                let again = compile(effective);
                prop_assert_eq!(again.clauses.iter().filter(|c| !c.negated).count(), count, "{:?}", effective);
                prop_assert_eq!(again.clauses.iter().filter(|c| c.negated).count(), negated);
                let mut rest = sources.iter();
                for i in 0..again.clauses.len() {
                    let source = again.source(i).unwrap();
                    prop_assert!(rest.any(|s| *s == source), "{:?} not in order in {:?}", source, text);
                }
            }
        }
    }
}

/// A compiled query whose root has `should` clauses (added since it compiled: the compiler builds none) is not relaxed:
/// L2 would drop requirements it does not know.
#[test]
fn a_compiled_root_changed_to_hold_should_clauses_is_not_relaxed() {
    use persia_analysis::AnalyzerRegistry;

    use crate::query::{DefaultFields, QueryCompiler, QueryLimits};
    use crate::schema::{FieldKind, SchemaBuilder, TextOptions};

    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let mut compiled = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(
            "red hat",
            &DefaultFields::all(&schema),
            &QueryLimits::default(),
        );
    let input = |compiled: &Compiled| {
        DropTerms
            .steps(&LevelInput::new(
                &compiled.query,
                Some(compiled),
                &crate::relax::tests::NoStats,
            ))
            .unwrap()
            .len()
    };
    assert_eq!(input(&compiled), 1);
    let root = |compiled: &mut Compiled| match &mut compiled.query {
        Query::Bool(root) => std::mem::take(root),
        q => panic!("two clauses compile to a bool: {q:?}"),
    };
    let mut changed = root(&mut compiled);
    changed.should.push(term(0, "shoes"));
    compiled.query = Query::Bool(changed.clone());
    assert_eq!(input(&compiled), 0);
    changed.should.clear();
    changed.minimum_should_match = 1;
    compiled.query = Query::Bool(changed);
    assert_eq!(input(&compiled), 0);
}
