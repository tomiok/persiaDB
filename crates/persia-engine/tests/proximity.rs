//! Term proximity (ROADMAP 7.3.5; SPEC §7.2, §7.7; ADR-0036), by hand over small segments.
//!
//! Invariants:
//! 1. A `bool` adds, after its `must` clauses, `Σ (λ · min(idf)) · closeness` over its pairs of adjacent `must`
//!    words, `closeness = 1 / d²` for the least distance `d ≤ 5` in a field with positions holding both (`q − p` in
//!    query order, `p − q + 1` reversed or stacked), else 0. So of two documents BM25F ties, the one with the words
//!    together ranks first.
//! 2. Only adjacent `must` words count: not a phrase, a `should` clause, a word under `boost`, a field without
//!    positions, nor two words found only in different fields.
//! 3. The explanation lists each pair and sums to the hit's score bit for bit; `without_proximity` scores the
//!    clauses alone.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    missing_docs
)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::query::{
    BoolQuery, BoostQuery, CombinedQuery, PhraseQuery, PhraseTerm, Query, TermQuery, WeightedTerm,
};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldId, FieldKind, SchemaBuilder, TextOptions,
};
use persia_engine::score::idf;
use persia_engine::search::{Explanation, Hit, Pruning, SearchPlan};
use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
use persia_engine::stats::SegmentStatistics;
use persia_engine::view::SegmentView;
use persia_format::ByteSource;

struct Fixture {
    schema: Arc<CollectionSchema>,
    segment: Arc<Segment>,
    body: FieldId,
    title: FieldId,
    plain: FieldId,
}

/// `body` and `title` with positions, `plain` without; whitespace analysis, so a token's position is its index.
/// Each document is `(body, title, plain)`.
fn fixture(docs: &[(&str, &str, &str)]) -> Fixture {
    let ws = AnalyzerRef::new("whitespace", 1).unwrap();
    let text = |positions| {
        FieldKind::Text(TextOptions {
            analyzer: ws.clone(),
            positions,
            ..TextOptions::default()
        })
    };
    let schema = SchemaBuilder::new("c")
        .field("body", text(true))
        .field("title", text(true))
        .field("plain", text(false))
        .build()
        .unwrap();
    let id = |n| schema.field(n).unwrap().id();
    let (body, title, plain) = (id("body"), id("title"), id("plain"));
    let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    for (i, (bo, ti, pl)) in docs.iter().enumerate() {
        let mut d = Document::new(format!("d{i}"));
        for (field, text) in [(body, bo), (title, ti), (plain, pl)] {
            if !text.is_empty() {
                d = d.with(field, Value::String((*text).into()));
            }
        }
        b.add(&d).unwrap();
    }
    let meta = SegmentMeta {
        segment_id: 1,
        created_at: 0,
        merge_generation: 0,
    };
    let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
    let segment = Arc::new(Segment::open(ByteSource::new(bytes.into()), &schema).unwrap());
    Fixture {
        schema: Arc::new(schema),
        segment,
        body,
        title,
        plain,
    }
}

impl Fixture {
    fn plan(&self, query: &Query) -> SearchPlan {
        let stats = SegmentStatistics::new(&self.schema, &[Arc::clone(&self.segment)]);
        SearchPlan::new(query, &self.schema, &stats).unwrap()
    }

    fn top(&self, plan: &SearchPlan) -> Vec<Hit> {
        let view = SegmentView::new(&self.segment, None);
        let want = plan.top_k_with(&view, 100, Pruning::Exhaustive).unwrap();
        for pruning in [Pruning::MaxScore, Pruning::BlockMaxWand] {
            let got = plan.top_k_with(&view, 100, pruning).unwrap();
            let bits = |h: &[Hit]| {
                h.iter()
                    .map(|h| (h.docid, h.score.to_bits()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(bits(&got), bits(&want), "{pruning:?}");
        }
        // Every bound of the `bool` adds the bonus's (SPEC §7.3): no hit scores above one.
        let mut m = plan.matcher_with(&view, Pruning::BlockMaxWand).unwrap();
        let max = m.max_score();
        for h in &want {
            let block = m.block_bound(h.docid).unwrap().unwrap();
            assert!(
                h.score <= max && h.score <= block.score,
                "{h:?}: {max}, {block:?}"
            );
        }
        want
    }

    fn explain(&self, plan: &SearchPlan, docid: u32) -> Explanation {
        plan.explain(&SegmentView::new(&self.segment, None), docid)
            .unwrap()
            .unwrap()
    }

    /// `idf` of `term` in `field`.
    fn idf(&self, field: FieldId, term: &str) -> f64 {
        let view = SegmentView::new(&self.segment, None);
        let df = persia_engine::view::SearchView::postings(&view, field, term.as_bytes())
            .unwrap()
            .map_or(0, |p| persia_engine::view::PostingsIterator::df(&p));
        idf(self.segment.doc_count().into(), df.into())
    }
}

fn term(field: FieldId, t: &str) -> Query {
    Query::Term(TermQuery {
        field,
        term: t.into(),
    })
}

fn and(must: Vec<Query>) -> Query {
    Query::Bool(BoolQuery {
        must,
        ..BoolQuery::default()
    })
}

/// The bonus the explanation of `docid` reports, and its pairs' distances.
fn bonus(e: &Explanation) -> Option<(f64, Vec<Option<u32>>)> {
    match e {
        Explanation::Bool {
            proximity: Some(p), ..
        } => Some((p.score, p.pairs.iter().map(|x| x.distance).collect())),
        _ => None,
    }
}

/// Invariant 1: same frequencies and lengths, so the same BM25F; the adjacent pair ranks first, and without
/// proximity the two tie (docid order).
#[test]
fn adjacent_words_rank_first() {
    let f = fixture(&[
        ("red hat blue shoes", "", ""),
        ("blue hat red shoes", "", ""),
        ("green coat", "", ""),
    ]);
    let q = and(vec![term(f.body, "red"), term(f.body, "shoes")]);
    let plan = f.plan(&q);
    let hits = f.top(&plan);
    assert_eq!(hits.iter().map(|h| h.docid).collect::<Vec<_>>(), [1, 0]);
    let weight = f.idf(f.body, "red").min(f.idf(f.body, "shoes"));
    // d1: `red shoes` adjacent (1); d0: `red` at 0, `shoes` at 3 (1/9).
    assert_eq!(bonus(&f.explain(&plan, 1)), Some((weight, vec![Some(1)])));
    assert_eq!(
        bonus(&f.explain(&plan, 0)),
        Some((weight * (1.0 / 9.0), vec![Some(3)]))
    );
    let off = plan.clone().without_proximity();
    assert!(!off.proximity());
    let hits = f.top(&off);
    assert_eq!(hits.iter().map(|h| h.docid).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(hits[0].score.to_bits(), hits[1].score.to_bits());
    assert_eq!(bonus(&f.explain(&off, 0)), None);
}

/// Invariant 1: reversed order costs one more, a stacked or repeated word pairs with its other occurrences only, and
/// beyond five apart a pair adds nothing.
#[test]
fn distances_by_hand() {
    let f = fixture(&[
        ("shoes red", "", ""),           // reversed: 2
        ("red a b c d shoes", "", ""),   // 5
        ("red a b c d e shoes", "", ""), // 6: beyond the window
        ("red red", "", ""),             // the same word twice: 1
        ("red x", "", ""),               // `red red` here: one occurrence, no pair
    ]);
    let pair = and(vec![term(f.body, "red"), term(f.body, "shoes")]);
    let plan = f.plan(&pair);
    let weight = f.idf(f.body, "red").min(f.idf(f.body, "shoes"));
    assert_eq!(
        bonus(&f.explain(&plan, 0)),
        Some((weight * 0.25, vec![Some(2)]))
    );
    assert_eq!(
        bonus(&f.explain(&plan, 1)),
        Some((weight * (1.0 / 25.0), vec![Some(5)]))
    );
    assert_eq!(bonus(&f.explain(&plan, 2)), Some((0.0, vec![None])));
    let twice = f.plan(&and(vec![term(f.body, "red"), term(f.body, "red")]));
    let w = f.idf(f.body, "red");
    assert_eq!(bonus(&f.explain(&twice, 3)), Some((w, vec![Some(1)])));
    assert_eq!(bonus(&f.explain(&twice, 4)), Some((0.0, vec![None])));
}

/// Invariant 1: a `combined` takes the closest of its fields; three words make two pairs, each its own.
#[test]
fn the_best_field_counts_and_pairs_add_in_order() {
    let f = fixture(&[("trail x running x x shoes", "trail running", "")]);
    let word = |t: &str| {
        Query::Combined(CombinedQuery {
            fields: vec![
                WeightedTerm {
                    field: f.body,
                    term: t.into(),
                    weight: 1.0,
                },
                WeightedTerm {
                    field: f.title,
                    term: t.into(),
                    weight: 2.0,
                },
            ],
        })
    };
    let plan = f.plan(&and(vec![word("trail"), word("running"), word("shoes")]));
    let e = f.explain(&plan, 0);
    let Explanation::Bool {
        score,
        must,
        proximity: Some(p),
        should,
    } = &e
    else {
        panic!("{e:?}")
    };
    assert_eq!(should.len(), 0);
    // `trail running`: adjacent in the title (body: 2 apart); `running shoes`: only the body holds both, 3 apart.
    assert_eq!(p.pairs.len(), 2);
    assert_eq!(
        (p.pairs[0].first, p.pairs[0].field, p.pairs[0].distance),
        (0, Some(f.title), Some(1))
    );
    assert_eq!(
        (p.pairs[1].first, p.pairs[1].field, p.pairs[1].distance),
        (1, Some(f.body), Some(3))
    );
    let sum = 0.0 + p.pairs[0].score + p.pairs[1].score;
    assert_eq!(p.score.to_bits(), sum.to_bits());
    // Invariant 3: the clauses in query order, then the bonus.
    let clauses = must.iter().fold(0.0, |s, m| s + m.score());
    assert_eq!(score.to_bits(), (clauses + p.score).to_bits());
    assert_eq!(score.to_bits(), f.top(&plan)[0].score.to_bits());
}

/// Invariant 2: no pair from a phrase, a `should`, a boosted word, a field without positions, or two fields.
#[test]
fn only_adjacent_must_words_with_positions_pair() {
    let f = fixture(&[("red shoes", "red", "red shoes")]);
    let none = |q: Query| {
        let plan = f.plan(&q);
        let e = f.explain(&plan, 0);
        assert_eq!(bonus(&e), None, "{q:?}: {e:?}");
    };
    let phrase = Query::Phrase(PhraseQuery {
        field: f.body,
        terms: vec![
            PhraseTerm {
                term: "red".into(),
                position: 0,
            },
            PhraseTerm {
                term: "shoes".into(),
                position: 1,
            },
        ],
    });
    none(and(vec![phrase, term(f.body, "shoes")]));
    none(Query::Bool(BoolQuery {
        should: vec![term(f.body, "red"), term(f.body, "shoes")],
        ..BoolQuery::default()
    }));
    none(and(vec![
        Query::Boost(BoostQuery {
            query: Box::new(term(f.body, "red")),
            boost: 2.0,
        }),
        term(f.body, "shoes"),
    ]));
    none(and(vec![term(f.plain, "red"), term(f.plain, "shoes")]));
    // `red` in the title, `shoes` in the body: no field holds both.
    none(and(vec![term(f.title, "red"), term(f.body, "shoes")]));
    // Not adjacent in the query: `red` and `shoes` with a phrase between them.
    let between = Query::Phrase(PhraseQuery {
        field: f.title,
        terms: vec![PhraseTerm {
            term: "red".into(),
            position: 0,
        }],
    });
    none(and(vec![
        term(f.body, "red"),
        between,
        term(f.body, "shoes"),
    ]));
}

/// Invariant 1, under `should` beside `must`: the bonus comes after the `must` clauses, before the `should` ones.
#[test]
fn the_bonus_sits_between_must_and_should() {
    let f = fixture(&[("red shoes hat", "", ""), ("red x shoes", "", "")]);
    let q = Query::Bool(BoolQuery {
        must: vec![term(f.body, "red"), term(f.body, "shoes")],
        should: vec![term(f.body, "hat")],
        ..BoolQuery::default()
    });
    let plan = f.plan(&q);
    let hits = f.top(&plan);
    let e = f.explain(&plan, 0);
    let Explanation::Bool {
        score,
        must,
        proximity: Some(p),
        should,
    } = &e
    else {
        panic!("{e:?}")
    };
    let required = must.iter().fold(0.0, |s, m| s + m.score()) + p.score;
    assert_eq!(score.to_bits(), (required + should[0].score()).to_bits());
    assert_eq!(hits[0].docid, 0);
    assert_eq!(hits[0].score.to_bits(), score.to_bits());
}
