#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)] // Tests: inputs are fixed.

use persia_analysis::AnalyzerRegistry;

use super::*;
use crate::query::{DefaultFields, QueryCompiler, QueryLimits};
use crate::schema::{FieldKind, SchemaBuilder, TextOptions};
use crate::stats::StatisticsProvider;

fn compile(text: &str) -> Compiled {
    compile_with(text, &QueryLimits::default())
}

fn compile_with(text: &str, limits: &QueryLimits) -> Compiled {
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .synonyms(vec![
            vec!["tv".into(), "telly".into(), "box".into()],
            vec!["new york".into(), "nyc".into()],
        ])
        .build()
        .unwrap();
    QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(text, &DefaultFields::all(&schema), limits)
}

/// A provider no L1 step reads: the level reads no statistics.
struct NoStats;

impl StatisticsProvider for NoStats {
    fn doc_count(&self) -> u64 {
        panic!("L1 reads no statistics")
    }
    fn field(&self, _: crate::schema::FieldId) -> crate::stats::FieldTotals {
        panic!("L1 reads no statistics")
    }
    fn df(&self, _: crate::schema::FieldId, _: &[u8]) -> Result<u64, ReadError> {
        panic!("L1 reads no statistics")
    }
}

fn steps(compiled: &Compiled) -> Vec<Step> {
    Synonyms
        .steps(&LevelInput::new(&compiled.query, Some(compiled), &NoStats))
        .unwrap()
}

fn should(query: &Query) -> &[Query] {
    match query {
        Query::Bool(b) if b.must.is_empty() => &b.should,
        other => panic!("not a should: {other:?}"),
    }
}

#[test]
fn each_run_is_required_as_itself_or_an_alternative_beside_l0s_other_clauses() {
    let compiled = compile("cheap new york tv -used");
    let Query::Bool(root) = &compiled.query else {
        panic!()
    };
    let [step] = steps(&compiled).try_into().unwrap();
    assert_eq!(step.applied, "synonyms");
    assert_eq!(step.effective.as_deref(), Some("cheap new york tv -used"));
    let Query::Bool(b) = &step.query else {
        panic!()
    };
    // `cheap` as is, `new york` (two parts, required together) or `nyc`, `tv` or `telly` or `box`.
    assert_eq!(b.must.len(), 3);
    assert_eq!(b.must[0], root.must[0]);
    let run = should(&b.must[1]);
    assert_eq!(
        run[0],
        Query::Bool(BoolQuery {
            must: root.must[1..3].to_vec(),
            ..BoolQuery::default()
        })
    );
    assert_eq!(run.len(), 2);
    assert_eq!(should(&b.must[2]).len(), 3);
    assert_eq!(should(&b.must[2])[0], root.must[3]);
    // The excluded clause stays excluded, and is not expanded.
    assert_eq!(b.must_not, root.must_not);
    assert!(b.should.is_empty() && b.filter.is_empty() && b.minimum_should_match == 0);
    // What the cascade may cut: each run's alternatives, kept from the first.
    let expansion = step.alternatives.unwrap();
    assert_eq!(expansion.counts(), [1, 2]);
    let Query::Bool(cut) = expansion.query(&[1, 1]) else {
        panic!()
    };
    assert_eq!(
        cut.must[2],
        Query::Bool(BoolQuery {
            should: should(&b.must[2])[..2].to_vec(),
            ..BoolQuery::default()
        })
    );
    assert_eq!(expansion.query(&[1, 2]), step.query);
}

#[test]
fn a_query_of_one_run_is_the_run_or_its_alternatives() {
    let compiled = compile("tv");
    let [step] = steps(&compiled).try_into().unwrap();
    let run = should(&step.query);
    assert_eq!(run[0], compiled.query);
    assert_eq!(&run[1..], compiled.synonyms[0].alternatives.as_slice());
}

#[test]
fn no_run_no_text_or_a_changed_query_builds_no_step() {
    assert_eq!(steps(&compile("cheap stand")).len(), 0);
    let compiled = compile("cheap tv");
    // A programmatic query.
    let input = LevelInput::new(&compiled.query, None, &NoStats);
    assert_eq!(Synonyms.steps(&input).unwrap().len(), 0);
    // A root the compiler would not build.
    for change in [
        |b: &mut BoolQuery| b.should.push(Query::MatchAll),
        |b: &mut BoolQuery| b.minimum_should_match = 1,
        |b: &mut BoolQuery| b.must.clear(),
    ] {
        let mut changed = compiled.clone();
        let Query::Bool(b) = &mut changed.query else {
            panic!()
        };
        change(b);
        assert!(steps(&changed).is_empty(), "{:?}", changed.query);
    }
    // Runs out of order, past the clauses, empty, or over an excluded clause.
    let compiled = compile("tv -box new york");
    for change in [
        |c: &mut Compiled| c.synonyms.reverse(),
        |c: &mut Compiled| c.synonyms[0].clauses = 7..8,
        |c: &mut Compiled| c.synonyms[0].clauses = 0..0,
        |c: &mut Compiled| c.synonyms[0].clauses = 0..2,
        |c: &mut Compiled| c.synonyms[0].alternatives.clear(),
    ] {
        let mut changed = compiled.clone();
        change(&mut changed);
        assert!(steps(&changed).is_empty(), "{:?}", changed.synonyms);
    }
}

mod properties {
    use proptest::prelude::*;

    use super::*;
    use crate::schema::AnalyzerRef;

    const WORDS: &[&str] = &[
        "a", "b", "c", "the", "of", "A", "-a", "\"a b\"", "title:a", "e-b",
    ];
    const TERMS: &[&str] = &["a", "b", "c", "a b", "the c", "b of c", "e-b", "A", "e b"];

    fn text() -> impl Strategy<Value = String> {
        prop::collection::vec(prop::sample::select(WORDS), 0..8).prop_map(|w| w.join(" "))
    }

    fn synonyms() -> impl Strategy<Value = Vec<Vec<String>>> {
        prop::collection::vec(
            prop::collection::btree_set(prop::sample::select(TERMS), 2..5)
                .prop_map(|g| g.into_iter().map(str::to_owned).collect::<Vec<_>>()),
            0..4,
        )
    }

    proptest! {
        /// Whatever the text and groups: runs are of required clauses, in text order, without overlap; each has 1 to
        /// 50 alternatives, none its run's own query, none twice, `max_terms` in all; and L1 builds a valid step from
        /// them that requires every clause L0 requires, each run as itself or an alternative.
        #[test]
        fn runs_are_ordered_required_and_bounded_and_build_a_valid_step(
            text in text(),
            synonyms in synonyms(),
            max_terms in 1usize..12,
        ) {
            let schema = SchemaBuilder::new("c")
                .field("title", FieldKind::Text(TextOptions { positions: true, ..TextOptions::default() }))
                .field(
                    "body",
                    FieldKind::Text(TextOptions {
                        analyzer: AnalyzerRef::builtin("en").unwrap(),
                        ..TextOptions::default()
                    }),
                )
                .synonyms(synonyms)
                .build()
                .unwrap();
            let limits = QueryLimits { max_terms, ..QueryLimits::default() };
            let compiled = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())
                .unwrap()
                .compile(&text, &DefaultFields::all(&schema), &limits);
            let mut end = 0;
            let mut total = 0;
            for run in &compiled.synonyms {
                prop_assert!(run.clauses.start >= end && run.clauses.start < run.clauses.end, "{:?}", compiled.synonyms);
                prop_assert!(run.clauses.end <= compiled.clauses.len());
                prop_assert!(compiled.clauses[run.clauses.clone()].iter().all(|c| !c.negated));
                end = run.clauses.end;
                prop_assert!((1..=50).contains(&run.alternatives.len()));
                for (i, a) in run.alternatives.iter().enumerate() {
                    prop_assert!(!run.alternatives[..i].contains(a));
                    a.validate(&schema, &QueryLimits::default()).unwrap();
                }
                total += run.alternatives.len();
            }
            prop_assert!(total <= max_terms);
            let built = steps(&compiled);
            prop_assert_eq!(built.len(), usize::from(!compiled.synonyms.is_empty()));
            for step in built {
                step.query.validate(&schema, &QueryLimits::UNLIMITED).unwrap();
                prop_assert_eq!(step.effective, Some(compiled.effective()));
                let expansion = Expansion::of(&compiled).unwrap();
                // L0's required parts, in text order, each with its clause: the units consume them all, in order, a
                // run as the one part or all of its clauses' parts required.
                let mut required = Vec::new();
                for (index, clause) in compiled.clauses.iter().enumerate() {
                    if !clause.negated {
                        for part in compiled.parts(index).unwrap() {
                            required.push((index, part.clone()));
                        }
                    }
                }
                let mut required = required.into_iter().peekable();
                let mut run = compiled.synonyms.iter();
                for unit in &expansion.units {
                    match unit {
                        Unit::Plain(q) => {
                            prop_assert_eq!(Some(q.clone()), required.next().map(|(_, p)| p));
                        }
                        Unit::Expanded { original, alternatives } => {
                            let run = run.next().unwrap();
                            prop_assert_eq!(alternatives, &run.alternatives);
                            let mut parts = Vec::new();
                            while let Some((_, part)) = required.next_if(|(c, _)| run.clauses.contains(c)) {
                                parts.push(part);
                            }
                            let expected = match parts.len() {
                                1 => parts.pop().unwrap(),
                                _ => Query::Bool(BoolQuery { must: parts, ..BoolQuery::default() }),
                            };
                            prop_assert_eq!(original, &expected);
                            prop_assert!(!alternatives.contains(original), "{:?}", alternatives);
                        }
                    }
                }
                prop_assert!(run.next().is_none());
                prop_assert!(required.next().is_none());
            }
        }
    }
}

#[test]
fn a_filter_joined_to_the_root_stays_on_the_step() {
    // SPEC §7.1: a request's `filter` is the compiled query's root `bool`'s; L1 keeps it, as L2 does.
    let mut compiled = compile("cheap tv");
    let filter = Filter::Term {
        field: crate::schema::FieldId::new(0),
        value: crate::query::FilterValue::Bool(true),
    };
    let Query::Bool(root) = &mut compiled.query else {
        panic!()
    };
    root.filter.push(filter.clone());
    let [step] = steps(&compiled).try_into().unwrap();
    let Query::Bool(b) = &step.query else {
        panic!()
    };
    assert_eq!(b.filter, std::slice::from_ref(&filter));
    // Cut or not, every query of the step keeps it.
    let expansion = step.alternatives.unwrap();
    let Query::Bool(cut) = expansion.query(&[0]) else {
        panic!()
    };
    assert_eq!(cut.filter, [filter]);
}

#[test]
fn a_filter_joined_to_a_one_part_query_is_not_read_as_its_clause() {
    // ROADMAP 8.1.17: `tv` compiles to its one part. Joined to a filter, by hand or with `with_filter`, the run is that
    // part, and the filter is the step's root's, once.
    let compiled = compile("tv");
    let part = compiled.query.clone();
    let filter = Filter::Term {
        field: crate::schema::FieldId::new(0),
        value: crate::query::FilterValue::Bool(true),
    };
    let mut by_hand = compiled.clone();
    by_hand.query = Query::Bool(BoolQuery {
        must: vec![part.clone()],
        filter: vec![filter.clone()],
        ..BoolQuery::default()
    });
    for joined in [by_hand, compiled.with_filter([filter.clone()])] {
        let [step] = steps(&joined).try_into().unwrap();
        let Query::Bool(b) = &step.query else {
            panic!()
        };
        assert_eq!(b.filter, std::slice::from_ref(&filter));
        let [Query::Bool(run)] = b.must.as_slice() else {
            panic!("{b:?}")
        };
        assert_eq!(run.should.first(), Some(&part), "{run:?}");
        assert!(
            run.should
                .iter()
                .all(|q| !matches!(q, Query::Bool(r) if !r.filter.is_empty()))
        );
    }
}

#[test]
fn the_expansion_is_cut_when_the_compilation_cut_any_run() {
    // `tv nyc` is 2 terms. At 6, 2 alternatives are left for 2 runs: `tv` keeps 1 of its 2, `nyc` its 1.
    let limits = |max_terms| QueryLimits {
        max_terms,
        ..QueryLimits::default()
    };
    let compiled = compile_with("tv nyc", &limits(6));
    let cuts: Vec<bool> = compiled.synonyms.iter().map(|s| s.cut).collect();
    assert_eq!(cuts, [true, false]);
    assert!(Expansion::of(&compiled).unwrap().cut());
    assert!(
        !Expansion::of(&compile_with("tv nyc", &limits(1024)))
            .unwrap()
            .cut()
    );
}
