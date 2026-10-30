//! Query execution: matching and top-k (ROADMAP 7.4.1–7.4.3; SPEC §7.1, §7.2, §7.3), differentially against the
//! reference (`tests/reference/search.rs`).
//!
//! Invariants:
//! 1. Over every view (segments with tombstones, a memtable view), a plan matches exactly the live documents the
//!    reference matches, each with the reference's score within [`EPSILON`] of the query's scale (7.4.1, 7.4.2).
//! 2. `advance(t)` lands on the first match at or after `t` (or stays when already past it), so skipping finds the
//!    same documents as stepping (7.4.1).
//! 3. `top_k(k)` is the first `k` of every hit sorted by score descending, then docid ascending, whatever `k`
//!    (7.4.3); `TopK` keeps the same hits whatever order they are pushed in.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated or fixed.

mod reference;

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::memtable::Memtable;
use persia_engine::query::{
    BoolQuery, BoostQuery, CombinedQuery, ConstantScoreQuery, PhraseQuery, PhraseTerm, Query,
    QueryLimits, TermQuery, WeightedTerm,
};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::search::{Hit, Matcher, SearchError, SearchPlan, TopK};
use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
use persia_engine::stats::{SearchStatistics, SegmentStatistics};
use persia_engine::view::{SearchView, SegmentView};
use persia_format::ByteSource;
use proptest::prelude::*;
use reference::bm25f::{Clause, Corpus, Doc, Field as RefField};
use reference::search::Query as RefQuery;
use roaring::RoaringBitmap;

/// The largest difference allowed between an engine score and the reference's, relative to the query's scale
/// (the sum of its leaves' largest scores): as `tests/scoring.rs`, plus the rounding of the sums.
const EPSILON: f64 = 1e-9;

/// A small vocabulary, so phrases occur.
const VOCAB: [&str; 4] = ["ant", "bee", "cat", "dog"];
const TAGS: [&str; 3] = ["x", "y", "z"];
const TITLE: usize = 0;
const BODY: usize = 1;
const TAG: usize = 2;
/// No document.
const NONE: [u32; 0] = [];

/// Per text field: `boost`, `k1`, `b`.
type Params = [(f64, f64, f64); 2];

fn schema(p: &Params) -> CollectionSchema {
    // The whitespace analyzer gives the words as they are, at positions 0, 1, 2...: the reference's tokens.
    let ws = AnalyzerRef::new("whitespace", 1).unwrap();
    let text = |(boost, k1, b)| {
        FieldKind::Text(TextOptions {
            analyzer: ws.clone(),
            boost,
            k1,
            b,
            positions: true,
            ..TextOptions::default()
        })
    };
    SchemaBuilder::new("c")
        .field("title", text(p[0]))
        .field("body", text(p[1]))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap()
}

fn ids(schema: &CollectionSchema) -> [FieldId; 3] {
    ["title", "body", "tag"].map(|n| schema.field(n).unwrap().id())
}

fn document(schema: &CollectionSchema, id: &str, doc: &Doc) -> Document {
    let [title, body, tag] = ids(schema);
    let mut d = Document::new(id);
    if !doc[TITLE].is_empty() {
        d = d.with(title, Value::String(doc[TITLE].join(" ")));
    }
    if !doc[BODY].is_empty() {
        d = d.with(body, Value::String(doc[BODY].join(" ")));
    }
    if !doc[TAG].is_empty() {
        d = d.with(tag, Value::Strings(doc[TAG].clone()));
    }
    d
}

fn segment(schema: &CollectionSchema, id: u128, docs: &[(String, Doc)]) -> Segment {
    let mut b = SegmentBuilder::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    for (name, doc) in docs {
        b.add(&document(schema, name, doc)).unwrap();
    }
    let meta = SegmentMeta {
        segment_id: id,
        created_at: 0,
        merge_generation: 0,
    };
    let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
    Segment::open(ByteSource::new(bytes.into()), schema).unwrap()
}

/// The engine's form of a reference query.
fn engine_query(q: &RefQuery, fields: &[FieldId; 3]) -> Query {
    match q {
        RefQuery::Word(clauses) if clauses.len() == 1 => Query::Term(TermQuery {
            field: fields[clauses[0].field],
            term: clauses[0].term.clone(),
        }),
        RefQuery::Word(clauses) => Query::Combined(CombinedQuery {
            fields: clauses
                .iter()
                .map(|c| WeightedTerm {
                    field: fields[c.field],
                    term: c.term.clone(),
                    weight: c.weight,
                })
                .collect(),
        }),
        RefQuery::Phrase { field, terms, .. } => Query::Phrase(PhraseQuery {
            field: fields[*field],
            terms: terms
                .iter()
                .map(|(term, position)| PhraseTerm {
                    term: term.clone(),
                    position: *position,
                })
                .collect(),
        }),
        RefQuery::Bool {
            must,
            should,
            must_not,
        } => {
            let all = |qs: &[RefQuery]| qs.iter().map(|q| engine_query(q, fields)).collect();
            Query::Bool(BoolQuery {
                must: all(must),
                should: all(should),
                must_not: all(must_not),
                filter: Vec::new(),
            })
        }
        RefQuery::Boost(inner, boost) => Query::Boost(BoostQuery {
            query: Box::new(engine_query(inner, fields)),
            boost: *boost,
        }),
        RefQuery::Constant(inner, score) => Query::ConstantScore(ConstantScoreQuery {
            query: Box::new(engine_query(inner, fields)),
            score: *score,
        }),
        RefQuery::All => Query::MatchAll,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Strategies

fn words(max: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop::sample::select(&VOCAB[..]).prop_map(str::to_owned),
        0..max,
    )
}

fn doc_strategy() -> impl Strategy<Value = Doc> {
    (
        prop_oneof![6 => words(6), 1 => words(60)],
        words(14),
        prop::collection::vec(
            prop::sample::select(&TAGS[..]).prop_map(str::to_owned),
            0..3,
        ),
    )
        .prop_map(|(title, body, tags)| vec![title, body, tags])
}

fn word() -> impl Strategy<Value = String> {
    prop::sample::select(&VOCAB[..]).prop_map(str::to_owned)
}

/// A leaf: a `term` on a text field or the keyword field, a `combined` over both text fields, a phrase, or
/// `match_all`. Text `term`s and phrases weigh with the schema `boost`.
fn leaf(params: Params) -> impl Strategy<Value = RefQuery> {
    let text_field = 0..2_usize;
    let weight = prop_oneof![Just(1.0), 0.1..10.0_f64];
    prop_oneof![
        4 => (text_field.clone(), word()).prop_map(move |(field, term)| RefQuery::Word(vec![Clause {
            field,
            term,
            weight: params[field].0,
        }])),
        1 => prop::sample::select(&TAGS[..]).prop_map(|t| RefQuery::Word(vec![Clause {
            field: TAG,
            term: t.to_owned(),
            weight: 1.0,
        }])),
        2 => (word(), word(), weight.clone(), weight).prop_map(|(a, b, wa, wb)| RefQuery::Word(vec![
            Clause { field: TITLE, term: a, weight: wa },
            Clause { field: BODY, term: b, weight: wb },
        ])),
        3 => (
            text_field,
            // Positions start anywhere: only the gaps count. A gap of 0 stacks two terms.
            0..3_u32,
            prop::collection::vec((word(), 0..3_u32), 1..4),
        )
            .prop_map(move |(field, start, terms)| {
                let mut at = start;
                let terms = terms
                    .into_iter()
                    .enumerate()
                    .map(|(i, (t, gap))| {
                        if i > 0 {
                            at += gap.min(1) + gap / 2;
                        }
                        (t, at)
                    })
                    .collect();
                RefQuery::Phrase { field, terms, weight: params[field].0 }
            }),
        1 => Just(RefQuery::All),
    ]
}

fn query(params: Params) -> impl Strategy<Value = RefQuery> {
    leaf(params).prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            4 => (
                prop::collection::vec(inner.clone(), 0..3),
                prop::collection::vec(inner.clone(), 0..3),
                prop::collection::vec(inner.clone(), 0..2),
            )
                .prop_map(|(must, should, must_not)| RefQuery::Bool { must, should, must_not }),
            1 => (inner.clone(), 0.5..4.0_f64).prop_map(|(q, b)| RefQuery::Boost(Box::new(q), b)),
            1 => (inner, prop_oneof![Just(0.0), 0.0..5.0_f64])
                .prop_map(|(q, s)| RefQuery::Constant(Box::new(q), s)),
        ]
    })
}

fn params() -> impl Strategy<Value = Params> {
    let field = (
        prop_oneof![Just(1.0), 0.5..5.0_f64],
        prop_oneof![Just(1.2), 0.0..3.0_f64],
        prop_oneof![Just(0.75), 0.0..=1.0_f64],
    );
    (field.clone(), field).prop_map(|(a, b)| [a, b])
}

#[derive(Debug, Clone)]
struct Case {
    params: Params,
    segments: Vec<Vec<(Doc, bool)>>,
    memtable: Vec<(Doc, bool)>,
    queries: Vec<RefQuery>,
    k: usize,
    targets: Vec<u32>,
}

fn case() -> impl Strategy<Value = Case> {
    let docs = |max| prop::collection::vec((doc_strategy(), prop::bool::weighted(0.2)), 0..max);
    params().prop_flat_map(move |p| {
        (
            Just(p),
            prop::collection::vec(docs(30), 0..4),
            docs(12),
            prop::collection::vec(query(p), 1..6),
            0..12_usize,
            prop::collection::vec(0..40_u32, 0..8),
        )
            .prop_map(|(params, segments, memtable, queries, k, targets)| Case {
                params,
                segments,
                memtable,
                queries,
                k,
                targets,
            })
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The check

/// Every match of `matcher` with its score, stepping with `next`.
fn exhaust(matcher: &mut dyn Matcher) -> Vec<Hit> {
    let mut out = Vec::new();
    while let Some(docid) = matcher.next().unwrap() {
        assert_eq!(matcher.doc(), Some(docid));
        let score = matcher.score().unwrap();
        assert_eq!(
            score.to_bits(),
            matcher.score().unwrap().to_bits(),
            "score twice"
        );
        out.push(Hit { docid, score });
    }
    assert_eq!(matcher.doc(), None);
    assert_eq!(matcher.next().unwrap(), None, "exhausted stays exhausted");
    out
}

/// Sorted by score descending, then docid ascending; written apart from `Hit::rank`, which it checks. Scores are
/// never NaN, and `-0.0` ties with `0.0`.
fn ranked(mut hits: Vec<Hit>) -> Vec<Hit> {
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap()
            .then(a.docid.cmp(&b.docid))
    });
    hits
}

/// Invariants 1–3 for one view: `docs[docid]` is its document when live.
fn check_view<V: SearchView>(
    view: &V,
    docs: &BTreeMap<u32, Doc>,
    plan: &SearchPlan,
    q: &RefQuery,
    corpus: &Corpus,
    case: &Case,
) {
    // Invariant 1.
    let hits = exhaust(&mut plan.matcher(view).unwrap());
    let expected: Vec<(u32, (f64, f64))> = docs
        .iter()
        .filter_map(|(&d, doc)| corpus.eval(q, doc).map(|s| (d, s)))
        .collect();
    assert_eq!(
        hits.iter().map(|h| h.docid).collect::<Vec<_>>(),
        expected.iter().map(|e| e.0).collect::<Vec<_>>(),
        "matches of {q:?}"
    );
    for (hit, (_, (score, scale))) in hits.iter().zip(&expected) {
        let tolerance = EPSILON * scale.max(1.0);
        assert!(
            (hit.score - score).abs() <= tolerance,
            "doc {}: engine {} vs reference {score} (scale {scale}) for {q:?}",
            hit.docid,
            hit.score
        );
    }

    // Invariant 2: advance to each target in turn.
    let mut matcher = plan.matcher(view).unwrap();
    let mut at = None::<u32>;
    let mut targets = case.targets.clone();
    targets.sort_unstable();
    for t in targets {
        let got = matcher.advance(t).unwrap();
        let floor = at.map_or(t, |a| a.max(t));
        let want = hits.iter().map(|h| h.docid).find(|&d| d >= floor);
        assert_eq!(got, want, "advance({t}) from {at:?} for {q:?}");
        if let Some(d) = got {
            let score = hits.iter().find(|h| h.docid == d).unwrap().score;
            assert_eq!(matcher.score().unwrap().to_bits(), score.to_bits());
            at = Some(d);
        } else {
            break;
        }
    }

    // Invariant 3.
    let all = ranked(hits);
    for k in [0, 1, case.k, all.len(), all.len() + 3] {
        let top = plan.top_k(view, k).unwrap();
        assert_eq!(top, all[..k.min(all.len())], "top {k} of {q:?}");
    }
}

fn check(case: &Case) {
    let schema = schema(&case.params);
    let fields = ids(&schema);
    let registry = AnalyzerRegistry::builtin();
    let mut corpus = Corpus {
        fields: vec![
            RefField {
                k1: case.params[0].1,
                b: case.params[0].2,
                keyword: false,
            },
            RefField {
                k1: case.params[1].1,
                b: case.params[1].2,
                keyword: false,
            },
            RefField {
                k1: 1.2,
                b: 0.0,
                keyword: true,
            },
        ],
        counted: Vec::new(),
    };
    let mut segments = Vec::new();
    let mut seg_docs = Vec::new();
    for (s, docs) in case.segments.iter().enumerate() {
        if docs.is_empty() {
            continue;
        }
        let named: Vec<(String, Doc)> = docs
            .iter()
            .enumerate()
            .map(|(j, (d, _))| (format!("s{s}-{j}"), d.clone()))
            .collect();
        let seg = Arc::new(segment(&schema, s as u128 + 1, &named));
        let mut deleted = RoaringBitmap::new();
        let mut live = BTreeMap::new();
        for (j, (d, del)) in docs.iter().enumerate() {
            corpus.counted.push(d.clone());
            if *del {
                deleted.insert(j as u32);
            } else {
                live.insert(j as u32, d.clone());
            }
        }
        let tombstones = (!deleted.is_empty()).then(|| seg.tombstones(deleted).unwrap());
        segments.push((seg, tombstones));
        seg_docs.push(live);
    }
    let mut mem = Memtable::new(&schema, &registry).unwrap();
    let mut mem_docs = BTreeMap::new();
    for (j, (d, del)) in case.memtable.iter().enumerate() {
        let name = format!("m{j}");
        mem.put(&document(&schema, &name, d)).unwrap();
        if *del {
            assert!(mem.delete(&name).unwrap());
        } else {
            corpus.counted.push(d.clone());
            mem_docs.insert(j as u32, d.clone());
        }
    }
    let view = mem.view();
    let arcs: Vec<Arc<Segment>> = segments.iter().map(|(s, _)| Arc::clone(s)).collect();
    let segment_stats = SegmentStatistics::new(&Arc::new(schema.clone()), &arcs);
    let stats = SearchStatistics::new(&segment_stats, &view);

    for q in &case.queries {
        let query = engine_query(q, &fields);
        query.validate(&schema, &QueryLimits::default()).unwrap();
        let plan = SearchPlan::new(&query, &schema, &stats).unwrap();
        for ((seg, tombstones), docs) in segments.iter().zip(&seg_docs) {
            check_view(
                &SegmentView::new(seg, tombstones.as_ref()),
                docs,
                &plan,
                q,
                &corpus,
                case,
            );
        }
        check_view(&view, &mem_docs, &plan, q, &corpus, case);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 2000 } else { 128 }))]

    #[test]
    fn plans_match_reference(case in case()) {
        check(&case);
    }

    /// Invariant 3 for the collector alone: any push order, scores with many ties.
    #[test]
    fn top_k_is_a_sorted_prefix(
        scores in prop::collection::vec(prop_oneof![Just(0.0), Just(-0.0), Just(1.5), 0.0..3.0_f64], 0..200),
        k in 0..50_usize,
        seed in any::<u64>(),
    ) {
        let hits: Vec<Hit> = scores.iter().enumerate().map(|(i, &s)| Hit { docid: i as u32, score: s + 0.0 }).collect();
        let mut order: Vec<usize> = (0..hits.len()).collect();
        // A deterministic shuffle from the seed.
        let mut x = seed | 1;
        for i in (1..order.len()).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            order.swap(i, (x % (i as u64 + 1)) as usize);
        }
        let mut top = TopK::new(k);
        for &i in &order {
            top.push(hits[i].docid, scores[i]);
        }
        let want = ranked(hits);
        prop_assert_eq!(top.into_hits(), want[..k.min(want.len())].to_vec());
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Hand-written cases

struct Fixture {
    schema: CollectionSchema,
    segment: Arc<Segment>,
    stats: SegmentStatistics,
}

fn fixture(bodies: &[&str]) -> Fixture {
    let p = [(1.0, 1.2, 0.75), (1.0, 1.2, 0.75)];
    let schema = schema(&p);
    let docs: Vec<(String, Doc)> = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let body = b.split_whitespace().map(str::to_owned).collect();
            (format!("d{i}"), vec![Vec::new(), body, Vec::new()])
        })
        .collect();
    let segment = Arc::new(segment(&schema, 1, &docs));
    let stats = SegmentStatistics::new(&Arc::new(schema.clone()), &[Arc::clone(&segment)]);
    Fixture {
        schema,
        segment,
        stats,
    }
}

impl Fixture {
    fn body(&self) -> FieldId {
        ids(&self.schema)[BODY]
    }

    fn docids(&self, q: &Query) -> Vec<u32> {
        let plan = SearchPlan::new(q, &self.schema, &self.stats).unwrap();
        let mut hits: Vec<u32> = plan
            .top_k(&SegmentView::new(&self.segment, None), usize::MAX)
            .unwrap()
            .iter()
            .map(|h| h.docid)
            .collect();
        hits.sort_unstable();
        hits
    }

    fn term(&self, t: &str) -> Query {
        Query::Term(TermQuery {
            field: self.body(),
            term: t.into(),
        })
    }

    fn phrase(&self, terms: &[(&str, u32)]) -> Query {
        Query::Phrase(PhraseQuery {
            field: self.body(),
            terms: terms
                .iter()
                .map(|&(t, position)| PhraseTerm {
                    term: t.into(),
                    position,
                })
                .collect(),
        })
    }
}

#[test]
fn phrases_by_hand() {
    let f = fixture(&[
        "ant bee cat",
        "bee ant",
        "ant ant bee",
        "ant cat bee",
        "dog",
    ]);
    assert_eq!(f.docids(&f.phrase(&[("ant", 0), ("bee", 1)])), [0, 2]);
    // A gap: `ant _ bee`.
    assert_eq!(f.docids(&f.phrase(&[("ant", 0), ("bee", 2)])), [2, 3]);
    // The same term twice.
    assert_eq!(f.docids(&f.phrase(&[("ant", 0), ("ant", 1)])), [2]);
    // Only the gaps count, not where the first term is.
    assert_eq!(
        f.docids(&f.phrase(&[("ant", 7), ("bee", 8)])),
        f.docids(&f.phrase(&[("ant", 0), ("bee", 1)]))
    );
    // A term the segment lacks.
    assert_eq!(f.docids(&f.phrase(&[("ant", 0), ("elk", 1)])), NONE);
    // One term is a term query, score included.
    let one = SearchPlan::new(&f.phrase(&[("cat", 0)]), &f.schema, &f.stats).unwrap();
    let term = SearchPlan::new(&f.term("cat"), &f.schema, &f.stats).unwrap();
    let view = SegmentView::new(&f.segment, None);
    assert_eq!(
        one.top_k(&view, 10).unwrap(),
        term.top_k(&view, 10).unwrap()
    );
}

#[test]
fn a_phrase_scores_its_frequency() {
    let f = fixture(&["ant bee x x ant bee", "ant bee x x x x"]);
    let plan = SearchPlan::new(&f.phrase(&[("ant", 0), ("bee", 1)]), &f.schema, &f.stats).unwrap();
    let hits = plan.top_k(&SegmentView::new(&f.segment, None), 10).unwrap();
    // Same length, twice the phrase: ranks first.
    assert_eq!(hits.iter().map(|h| h.docid).collect::<Vec<_>>(), [0, 1]);
    assert!(hits[0].score > hits[1].score);
}

#[test]
fn bool_edges_by_hand() {
    let f = fixture(&["ant", "bee", "ant bee", "cat"]);
    let b = |must, should, must_not| {
        Query::Bool(BoolQuery {
            must,
            should,
            must_not,
            filter: Vec::new(),
        })
    };
    assert_eq!(f.docids(&b(vec![], vec![], vec![])), [0, 1, 2, 3]);
    assert_eq!(f.docids(&b(vec![], vec![], vec![f.term("ant")])), [1, 3]);
    // A `should` that cannot match anywhere still makes one required: nothing matches.
    assert_eq!(f.docids(&b(vec![], vec![f.term("elk")], vec![])), NONE);
    assert_eq!(
        f.docids(&b(vec![f.term("ant")], vec![f.term("elk")], vec![])),
        [0, 2]
    );
    assert_eq!(
        f.docids(&b(vec![f.term("ant")], vec![], vec![f.term("bee")])),
        [0]
    );
    assert_eq!(
        f.docids(&b(vec![f.term("ant"), f.term("elk")], vec![], vec![])),
        NONE
    );
    assert_eq!(
        f.docids(&b(vec![], vec![f.term("ant"), f.term("cat")], vec![])),
        [0, 2, 3]
    );
    // `should` beside `must` adds to the score.
    let plan = SearchPlan::new(
        &b(vec![f.term("ant")], vec![f.term("bee")], vec![]),
        &f.schema,
        &f.stats,
    )
    .unwrap();
    let hits = plan.top_k(&SegmentView::new(&f.segment, None), 10).unwrap();
    assert_eq!(hits[0].docid, 2);
}

#[test]
fn unsupported_queries_are_errors() {
    let f = fixture(&["ant"]);
    let prefix = Query::Prefix(persia_engine::query::PrefixQuery {
        field: f.body(),
        prefix: "an".into(),
        max_expansions: 10,
    });
    assert!(matches!(
        SearchPlan::new(&prefix, &f.schema, &f.stats),
        Err(SearchError::Unsupported { what: "prefix" })
    ));
    let filtered = Query::Bool(BoolQuery {
        filter: vec![persia_engine::query::Filter::And(Vec::new())],
        ..BoolQuery::default()
    });
    assert!(matches!(
        SearchPlan::new(&filtered, &f.schema, &f.stats),
        Err(SearchError::Unsupported { .. })
    ));
}

/// Long lists, whose skip entries `advance` uses (SPEC §5.3.2): a conjunction of a rare and a common term.
#[test]
fn conjunctions_skip_over_long_lists() {
    let bodies: Vec<String> = (0..5000)
        .map(|i| {
            if i % 997 == 0 {
                "ant bee".to_owned()
            } else {
                "ant".to_owned()
            }
        })
        .collect();
    let refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let f = fixture(&refs);
    let and = Query::Bool(BoolQuery {
        must: vec![f.term("ant"), f.term("bee")],
        ..BoolQuery::default()
    });
    assert_eq!(f.docids(&and), [0, 997, 1994, 2991, 3988, 4985]);
    let phrase = f.phrase(&[("ant", 0), ("bee", 1)]);
    assert_eq!(f.docids(&phrase), [0, 997, 1994, 2991, 3988, 4985]);
}

/// What `validate` rejects and would plan into different results is rejected by the plan too.
#[test]
fn plans_reject_what_would_change_results() {
    let f = fixture(&["ant bee"]);
    let plain = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let no_positions = Query::Phrase(PhraseQuery {
        field: plain.field("body").unwrap().id(),
        terms: vec![
            PhraseTerm {
                term: "ant".into(),
                position: 0,
            },
            PhraseTerm {
                term: "bee".into(),
                position: 1,
            },
        ],
    });
    let stats = SegmentStatistics::new(&Arc::new(plain.clone()), &[]);
    assert!(matches!(
        SearchPlan::new(&no_positions, &plain, &stats),
        Err(SearchError::Query(_))
    ));
    let decreasing = f.phrase(&[("ant", 1), ("bee", 0)]);
    assert!(matches!(
        SearchPlan::new(&decreasing, &f.schema, &f.stats),
        Err(SearchError::Query(_))
    ));
    for boost in [f64::NAN, 0.0, -1.0, 1001.0] {
        let q = Query::Boost(BoostQuery {
            query: Box::new(f.term("ant")),
            boost,
        });
        assert!(matches!(
            SearchPlan::new(&q, &f.schema, &f.stats),
            Err(SearchError::Query(_))
        ));
    }
    let q = Query::ConstantScore(ConstantScoreQuery {
        query: Box::new(f.term("ant")),
        score: f64::INFINITY,
    });
    assert!(matches!(
        SearchPlan::new(&q, &f.schema, &f.stats),
        Err(SearchError::Query(_))
    ));
}

/// The same document scores the same bits in segments whose clause costs order a conjunction differently, and
/// where a `should` term is missing (SPEC §7.2: clauses are added in query order).
#[test]
fn a_document_scores_the_same_in_any_segment() {
    let schema = schema(&[(1.0, 1.2, 0.75), (1.0, 1.2, 0.75)]);
    let body = ids(&schema)[BODY];
    let doc = |id: &str, text: &str| {
        let words = text.split_whitespace().map(str::to_owned).collect();
        (id.to_owned(), vec![Vec::new(), words, Vec::new()])
    };
    // `elk` is rare in `a` and `ant` in `b`, so their conjunctions lead with different clauses.
    let mut a = vec![doc("t", "ant bee elk")];
    let mut b = vec![doc("t", "ant bee elk")];
    for i in 0..40 {
        a.push(doc(&format!("a{i}"), "ant bee cat"));
        b.push(doc(&format!("b{i}"), "bee elk dog"));
    }
    let a = Arc::new(segment(&schema, 1, &a));
    let b = Arc::new(segment(&schema, 2, &b));
    let stats =
        SegmentStatistics::new(&Arc::new(schema.clone()), &[Arc::clone(&a), Arc::clone(&b)]);
    let term = |t: &str| {
        Query::Term(TermQuery {
            field: body,
            term: t.into(),
        })
    };
    let boosted = |t: &str, boost| {
        Query::Boost(BoostQuery {
            query: Box::new(term(t)),
            boost,
        })
    };
    let q = Query::Bool(BoolQuery {
        // Three scores of different magnitudes: their sum rounds differently in another order.
        must: vec![
            boosted("ant", 1.0),
            boosted("bee", 0.003_141_6),
            boosted("elk", 777.7),
        ],
        should: vec![term("cat"), term("ant")],
        ..BoolQuery::default()
    });
    let plan = SearchPlan::new(&q, &schema, &stats).unwrap();
    let score = |s: &Segment| {
        let view = SegmentView::new(s, None);
        let hits = plan.top_k(&view, 100).unwrap();
        let t = view.docid_of("t").unwrap().unwrap();
        hits.iter().find(|h| h.docid == t).unwrap().score
    };
    assert_eq!(score(&a).to_bits(), score(&b).to_bits());
}

/// A memtable's replaced versions are not live: `match_all` and a `must_not`-only bool see one version per id.
#[test]
fn memtable_replaced_versions_are_left_out() {
    let schema = schema(&[(1.0, 1.2, 0.75), (1.0, 1.2, 0.75)]);
    let body = ids(&schema)[BODY];
    let mut mem = Memtable::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    let doc = |text: &str| vec![Vec::new(), vec![text.to_owned()], Vec::new()];
    mem.put(&document(&schema, "a", &doc("ant"))).unwrap();
    mem.put(&document(&schema, "a", &doc("bee"))).unwrap();
    mem.put(&document(&schema, "b", &doc("ant"))).unwrap();
    let view = mem.view();
    let segment_stats = SegmentStatistics::new(&Arc::new(schema.clone()), &[]);
    let stats = SearchStatistics::new(&segment_stats, &view);
    let ids_of = |q: &Query| -> Vec<String> {
        let plan = SearchPlan::new(q, &schema, &stats).unwrap();
        let mut ids: Vec<String> = plan
            .top_k(&view, 10)
            .unwrap()
            .iter()
            .map(|h| view.document(h.docid).unwrap().id().to_owned())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(ids_of(&Query::MatchAll), ["a", "b"]);
    let not_ant = Query::Bool(BoolQuery {
        must_not: vec![Query::Term(TermQuery {
            field: body,
            term: "ant".into(),
        })],
        ..BoolQuery::default()
    });
    assert_eq!(ids_of(&not_ant), ["a"]);
}
