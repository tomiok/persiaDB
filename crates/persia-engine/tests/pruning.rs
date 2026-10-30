//! Top-k pruning (ROADMAP 7.4.4; SPEC §7.3) differentially against exhaustive evaluation, over corpora large enough
//! that common terms span many postings blocks with Skip entries (`df ≥ 128`, SPEC §5.3.2).
//!
//! Invariants:
//! 1. `top_k_with(k, p)` returns the exhaustive top `k` for every [`Pruning`]: the same docids, scores bit for bit
//!    (7.4.4.3), over a segment with tombstones and over a memtable view.
//! 2. The threshold contract of [`Matcher::set_min_competitive`]: a pruning matcher returns matches in docid order,
//!    each with its exhaustive score bit for bit, and every match it skips scores at most the threshold in force.
//! 3. Pruning prunes: on skewed queries it returns far fewer documents to the collector than there are matches.
//! 4. The bounds hold: every score is at most `max_score`, and at most the `block_bound` of the range it is in.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    missing_docs
)] // Tests: inputs are generated or fixed.

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
use persia_engine::search::{Hit, Matcher, Pruning, SearchPlan};
use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
use persia_engine::stats::{SearchStatistics, SegmentStatistics};
use persia_engine::view::{SearchView, SegmentView};
use persia_format::ByteSource;
use proptest::prelude::*;
use roaring::RoaringBitmap;

const WORDS: u64 = 40;
const TAGS: [&str; 3] = ["x", "y", "z"];
const PRUNED: [Pruning; 2] = [Pruning::MaxScore, Pruning::BlockMaxWand];

/// xorshift64*: deterministic from the seed proptest prints on failure.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A word, skewed toward low numbers: `w0` is on most documents, `w39` on few.
    fn word(&mut self) -> String {
        let a = self.below(WORDS);
        let b = self.below(WORDS);
        format!("w{}", a * b / WORDS)
    }
}

struct Fields {
    title: FieldId,
    body: FieldId,
    tag: FieldId,
}

fn schema() -> (CollectionSchema, Fields) {
    let ws = AnalyzerRef::new("whitespace", 1).unwrap();
    let text = |boost| {
        FieldKind::Text(TextOptions {
            analyzer: ws.clone(),
            boost,
            positions: true,
            ..TextOptions::default()
        })
    };
    let schema = SchemaBuilder::new("c")
        .field("title", text(2.0))
        .field("body", text(1.0))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let id = |n| schema.field(n).unwrap().id();
    let fields = Fields {
        title: id("title"),
        body: id("body"),
        tag: id("tag"),
    };
    (schema, fields)
}

/// `n` documents: bodies of 1 to 80 words (so norms vary), short titles, a tag on some. The style changes from one
/// stretch of docids to the next (mixed, short with repeated words, long), so block bounds differ and block-max
/// WAND has blocks to skip.
fn documents(seed: u64, n: usize, f: &Fields, prefix: &str) -> Vec<Document> {
    let mut rng = Rng(seed | 1);
    let stretch = 50 + rng.below(400) as usize;
    (0..n)
        .map(|i| {
            let style = (i / stretch + seed as usize) % 3;
            let body: Vec<String> = match style {
                0 => {
                    let longest = if rng.below(4) == 0 { 80 } else { 12 };
                    (0..=rng.below(longest)).map(|_| rng.word()).collect()
                }
                1 => {
                    let w = rng.word();
                    (0..=rng.below(4))
                        .map(|_| w.clone())
                        .chain([rng.word()])
                        .collect()
                }
                _ => (0..40 + rng.below(40)).map(|_| rng.word()).collect(),
            };
            let title: Vec<String> = (0..rng.below(4)).map(|_| rng.word()).collect();
            let mut d =
                Document::new(format!("{prefix}{i}")).with(f.body, Value::String(body.join(" ")));
            if !title.is_empty() {
                d = d.with(f.title, Value::String(title.join(" ")));
            }
            if rng.below(3) == 0 {
                let tag = TAGS[rng.below(3) as usize];
                d = d.with(f.tag, Value::String(tag.to_owned()));
            }
            d
        })
        .collect()
}

fn segment(schema: &CollectionSchema, docs: &[Document]) -> Segment {
    let mut b = SegmentBuilder::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    for d in docs {
        b.add(d).unwrap();
    }
    let meta = SegmentMeta {
        segment_id: 1,
        created_at: 0,
        merge_generation: 0,
    };
    let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
    Segment::open(ByteSource::new(bytes.into()), schema).unwrap()
}

// ---------------------------------------------------------------------------------------------------------------
// Queries

/// A query shape, made concrete over the schema's fields by [`Shape::query`].
#[derive(Debug, Clone)]
enum Shape {
    Term(u8),
    Title(u8),
    Tag(u8),
    Combined(u8, u8, f64),
    Phrase(u8, u8),
    Or(Vec<Shape>),
    OrNot(Vec<Shape>, Box<Shape>),
    And(Vec<Shape>),
    AndOr(Box<Shape>, Vec<Shape>),
    Boost(Box<Shape>, f64),
    Constant(Box<Shape>, f64),
}

impl Shape {
    fn query(&self, f: &Fields) -> Query {
        let term = |field, w: &u8| {
            Query::Term(TermQuery {
                field,
                term: format!("w{w}"),
            })
        };
        let all = |s: &[Shape]| s.iter().map(|s| s.query(f)).collect();
        match self {
            Self::Term(w) => term(f.body, w),
            Self::Title(w) => term(f.title, w),
            Self::Tag(t) => Query::Term(TermQuery {
                field: f.tag,
                term: TAGS[usize::from(*t) % TAGS.len()].to_owned(),
            }),
            Self::Combined(a, b, weight) => Query::Combined(CombinedQuery {
                fields: vec![
                    WeightedTerm {
                        field: f.title,
                        term: format!("w{a}"),
                        weight: *weight,
                    },
                    WeightedTerm {
                        field: f.body,
                        term: format!("w{b}"),
                        weight: 1.0,
                    },
                ],
            }),
            Self::Phrase(a, b) => Query::Phrase(PhraseQuery {
                field: f.body,
                terms: vec![
                    PhraseTerm {
                        term: format!("w{a}"),
                        position: 0,
                    },
                    PhraseTerm {
                        term: format!("w{b}"),
                        position: 1,
                    },
                ],
            }),
            Self::Or(s) => Query::Bool(BoolQuery {
                should: all(s),
                ..BoolQuery::default()
            }),
            Self::OrNot(s, not) => Query::Bool(BoolQuery {
                should: all(s),
                must_not: vec![not.query(f)],
                ..BoolQuery::default()
            }),
            Self::And(s) => Query::Bool(BoolQuery {
                must: all(s),
                ..BoolQuery::default()
            }),
            Self::AndOr(must, should) => Query::Bool(BoolQuery {
                must: vec![must.query(f)],
                should: all(should),
                ..BoolQuery::default()
            }),
            Self::Boost(s, b) => Query::Boost(BoostQuery {
                query: Box::new(s.query(f)),
                boost: *b,
            }),
            Self::Constant(s, c) => Query::ConstantScore(ConstantScoreQuery {
                query: Box::new(s.query(f)),
                score: *c,
            }),
        }
    }
}

fn word() -> impl Strategy<Value = u8> {
    // Common words most of the time, so lists span many blocks; rare ones too, for high `idf`s.
    prop_oneof![3 => 0..8_u8, 2 => 0..WORDS as u8, 1 => Just(WORDS as u8)]
}

fn shape() -> impl Strategy<Value = Shape> {
    let leaf = prop_oneof![
        6 => word().prop_map(Shape::Term),
        2 => word().prop_map(Shape::Title),
        1 => (0..3_u8).prop_map(Shape::Tag),
        2 => (word(), word(), prop_oneof![Just(1.0), 0.1..10.0_f64]).prop_map(|(a, b, w)| Shape::Combined(a, b, w)),
        1 => (0..4_u8, 0..4_u8).prop_map(|(a, b)| Shape::Phrase(a, b)),
    ];
    leaf.prop_recursive(3, 32, 8, |inner| {
        let clauses = prop::collection::vec(inner.clone(), 2..8);
        prop_oneof![
            6 => clauses.clone().prop_map(Shape::Or),
            2 => (clauses.clone(), inner.clone()).prop_map(|(s, n)| Shape::OrNot(s, Box::new(n))),
            1 => prop::collection::vec(inner.clone(), 1..3).prop_map(Shape::And),
            1 => (inner.clone(), clauses).prop_map(|(m, s)| Shape::AndOr(Box::new(m), s)),
            2 => (inner.clone(), prop_oneof![Just(0.5), 0.01..20.0_f64]).prop_map(|(s, b)| Shape::Boost(Box::new(s), b)),
            1 => (inner, prop_oneof![Just(0.0), 0.0..5.0_f64]).prop_map(|(s, c)| Shape::Constant(Box::new(s), c)),
        ]
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The checks

fn bits(hits: &[Hit]) -> Vec<(u32, u64)> {
    hits.iter().map(|h| (h.docid, h.score.to_bits())).collect()
}

/// Every match with its score, by docid.
fn exhaust(plan: &SearchPlan, view: &impl SearchView) -> BTreeMap<u32, f64> {
    let mut m = plan.matcher(view).unwrap();
    let mut out = BTreeMap::new();
    while let Some(d) = m.next().unwrap() {
        out.insert(d, m.score().unwrap());
    }
    out
}

/// Invariant 1.
fn same_top_k(plan: &SearchPlan, view: &impl SearchView, ks: &[usize], what: &str) {
    for &k in ks {
        let want = plan.top_k_with(view, k, Pruning::Exhaustive).unwrap();
        for pruning in PRUNED {
            let got = plan.top_k_with(view, k, pruning).unwrap();
            assert_eq!(
                bits(&got),
                bits(&want),
                "top {k} of {what} with {pruning:?}"
            );
        }
    }
}

/// Invariant 2: thresholds from `quantiles` of the match scores, raised after every `every` documents returned.
fn threshold_contract(
    plan: &SearchPlan,
    view: &impl SearchView,
    all: &BTreeMap<u32, f64>,
    quantiles: &[f64],
    every: usize,
    what: &str,
) {
    let mut sorted: Vec<f64> = all.values().copied().collect();
    sorted.sort_by(f64::total_cmp);
    let mut thresholds: Vec<f64> = quantiles
        .iter()
        .filter_map(|q| {
            sorted
                .get(((sorted.len() as f64 - 1.0) * q) as usize)
                .copied()
        })
        .collect();
    thresholds.sort_by(f64::total_cmp);
    for pruning in PRUNED {
        let mut m = plan.matcher_with(view, pruning).unwrap();
        let mut min = None::<f64>;
        let mut next_threshold = thresholds.iter();
        let mut prev = None::<u32>;
        let mut returned = 0_usize;
        loop {
            let got = m.next().unwrap();
            // Every match after `prev` and before `got` was skipped under `min`.
            let skipped = all
                .range(prev.map_or(0, |p| p + 1)..got.unwrap_or(u32::MAX))
                .filter(|&(&d, _)| got != Some(d));
            for (&d, &s) in skipped {
                assert!(
                    min.is_some_and(|t| s <= t),
                    "{pruning:?} skipped doc {d} scoring {s} under {min:?} for {what}"
                );
            }
            let Some(d) = got else { break };
            assert!(prev.is_none_or(|p| d > p));
            let want = all.get(&d).copied();
            assert_eq!(
                Some(m.score().unwrap().to_bits()),
                want.map(f64::to_bits),
                "{pruning:?} doc {d} for {what}"
            );
            prev = Some(d);
            returned += 1;
            if returned.is_multiple_of(every)
                && let Some(&t) = next_threshold.next()
            {
                min = Some(min.map_or(t, |old: f64| old.max(t)));
                m.set_min_competitive(min.unwrap_or(t));
            }
        }
    }
}

/// Invariant 4: `max_score` bounds every score; each `block_bound` from a target, the scores through its `up_to`.
fn bounds_hold(plan: &SearchPlan, view: &impl SearchView, all: &BTreeMap<u32, f64>, what: &str) {
    for pruning in [
        Pruning::Exhaustive,
        Pruning::MaxScore,
        Pruning::BlockMaxWand,
    ] {
        let mut m = plan.matcher_with(view, pruning).unwrap();
        let max = m.max_score();
        for (d, s) in all {
            assert!(
                *s <= max,
                "doc {d} scores {s} above max_score {max} of {what}"
            );
        }
        let mut target = 0_u32;
        while let Some(b) = m.block_bound(target).unwrap() {
            for (d, s) in all.range(target..=b.up_to) {
                assert!(
                    *s <= b.score,
                    "doc {d} scores {s} above block bound {b:?} from {target} of {what}"
                );
            }
            let Some(next) = b.up_to.checked_add(1) else {
                break;
            };
            target = next;
        }
    }
}

struct Case {
    schema: CollectionSchema,
    fields: Fields,
    segment: Arc<Segment>,
    tombstones: Option<persia_engine::segment::Tombstones>,
    memtable: Memtable,
}

fn build(seed: u64, n: usize, deleted_per_mille: u64) -> Case {
    let (schema, fields) = schema();
    let docs = documents(seed, n, &fields, "d");
    let segment = Arc::new(segment(&schema, &docs));
    let mut rng = Rng(seed.rotate_left(17) | 1);
    let deleted: RoaringBitmap = (0..n as u32)
        .filter(|_| rng.below(1000) < deleted_per_mille)
        .collect();
    let tombstones = (!deleted.is_empty()).then(|| segment.tombstones(deleted).unwrap());
    let mut memtable = Memtable::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    for d in documents(seed ^ 0x5eed, n / 4, &fields, "m") {
        memtable.put(&d).unwrap();
    }
    Case {
        schema,
        fields,
        segment,
        tombstones,
        memtable,
    }
}

fn check(case: &Case, shapes: &[Shape], k: usize, quantiles: &[f64], every: usize) {
    let mem = case.memtable.view();
    let arcs = [Arc::clone(&case.segment)];
    let segment_stats = SegmentStatistics::new(&Arc::new(case.schema.clone()), &arcs);
    let stats = SearchStatistics::new(&segment_stats, &mem);
    let seg = SegmentView::new(&case.segment, case.tombstones.as_ref());
    for shape in shapes {
        let query = shape.query(&case.fields);
        query
            .validate(&case.schema, &QueryLimits::default())
            .unwrap();
        let plan = SearchPlan::new(&query, &case.schema, &stats).unwrap();
        let what = format!("{shape:?}");
        same_top_k(&plan, &seg, &[1, 2, 10, k, 100], &what);
        same_top_k(&plan, &mem, &[1, 10, k], &what);
        let (all_seg, all_mem) = (exhaust(&plan, &seg), exhaust(&plan, &mem));
        bounds_hold(&plan, &seg, &all_seg, &what);
        bounds_hold(&plan, &mem, &all_mem, &what);
        threshold_contract(&plan, &seg, &all_seg, quantiles, every, &what);
        threshold_contract(&plan, &mem, &all_mem, quantiles, every, &what);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 256 } else { 16 }))]

    #[test]
    fn pruning_matches_exhaustive(
        seed in any::<u64>(),
        n in 200..1200_usize,
        deleted in prop_oneof![Just(0_u64), 0..300_u64],
        shapes in prop::collection::vec(shape(), 1..6),
        k in 1..40_usize,
        quantiles in prop::collection::vec(0.0..=1.0_f64, 0..6),
        every in 1..20_usize,
    ) {
        let case = build(seed, n, deleted);
        check(&case, &shapes, k, &quantiles, every);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Hand-written cases

/// How many documents a top-`k` collection is handed with `pruning`, and the hits.
fn handed(
    plan: &SearchPlan,
    view: &impl SearchView,
    k: usize,
    pruning: Pruning,
) -> (usize, Vec<Hit>) {
    let mut m = plan.matcher_with(view, pruning).unwrap();
    let mut top = persia_engine::search::TopK::new(k);
    let mut count = 0;
    while let Some(d) = m.next().unwrap() {
        count += 1;
        top.push(d, m.score().unwrap());
        if let Some(t) = top.threshold() {
            m.set_min_competitive(t);
        }
    }
    (count, top.into_hits())
}

/// Invariant 3, over 6000 documents.
#[test]
fn pruning_skips_most_documents() {
    let case = build(7, 6000, 0);
    let mem = case.memtable.view();
    let arcs = [Arc::clone(&case.segment)];
    let segment_stats = SegmentStatistics::new(&Arc::new(case.schema.clone()), &arcs);
    let stats = SearchStatistics::new(&segment_stats, &mem);
    let view = SegmentView::new(&case.segment, None);
    let pruned = |shape: &Shape, pruning| {
        let plan = SearchPlan::new(&shape.query(&case.fields), &case.schema, &stats).unwrap();
        let (all, want) = handed(&plan, &view, 10, Pruning::Exhaustive);
        let (count, hits) = handed(&plan, &view, 10, pruning);
        assert_eq!(bits(&hits), bits(&want), "{shape:?} with {pruning:?}");
        (count, all)
    };
    // A common word beside rare ones: `MaxScore` soon leaves `w0` out; block-max WAND skips most of its blocks.
    let rare = Shape::Or(vec![Shape::Term(0), Shape::Term(30), Shape::Term(35)]);
    let (count, all) = pruned(&rare, Pruning::MaxScore);
    assert!(
        count * 10 < all,
        "MaxScore: {count} of {all} documents scored"
    );
    let (count, all) = pruned(&rare, Pruning::BlockMaxWand);
    assert!(
        count * 2 < all,
        "block-max WAND: {count} of {all} documents scored"
    );
    // Two common words: some.
    let common = Shape::Or(vec![Shape::Term(1), Shape::Term(2)]);
    for pruning in PRUNED {
        let (count, all) = pruned(&common, pruning);
        assert!(
            count * 10 < all * 8,
            "{pruning:?}: {count} of {all} documents scored"
        );
    }
    // A word alone: block-max WAND skips the blocks whose bound is below the threshold; `MaxScore` cannot, with one
    // bound for the whole list.
    let (count, all) = pruned(&Shape::Term(5), Pruning::BlockMaxWand);
    assert!(
        count * 10 < all * 8,
        "w5: {count} of {all} documents scored"
    );
    let (count, all) = pruned(&Shape::Term(5), Pruning::MaxScore);
    assert_eq!(count, all);
}

/// A threshold no document beats ends every pruned walk at once; one below every score skips nothing.
#[test]
fn extreme_thresholds() {
    let case = build(11, 1500, 50);
    let mem = case.memtable.view();
    let arcs = [Arc::clone(&case.segment)];
    let segment_stats = SegmentStatistics::new(&Arc::new(case.schema.clone()), &arcs);
    let stats = SearchStatistics::new(&segment_stats, &mem);
    let view = SegmentView::new(&case.segment, case.tombstones.as_ref());
    let shape = Shape::OrNot(
        vec![
            Shape::Term(0),
            Shape::Combined(3, 5, 2.0),
            Shape::Boost(Box::new(Shape::Term(20)), 3.0),
        ],
        Box::new(Shape::Term(39)),
    );
    let plan = SearchPlan::new(&shape.query(&case.fields), &case.schema, &stats).unwrap();
    let all = exhaust(&plan, &view);
    assert!(all.len() > 500);
    for pruning in PRUNED {
        let mut m = plan.matcher_with(&view, pruning).unwrap();
        m.set_min_competitive(f64::MAX);
        assert_eq!(m.next().unwrap(), None, "{pruning:?}");
        let mut m = plan.matcher_with(&view, pruning).unwrap();
        m.set_min_competitive(-1.0);
        let mut got = BTreeMap::new();
        while let Some(d) = m.next().unwrap() {
            got.insert(d, m.score().unwrap());
        }
        assert_eq!(
            got.iter()
                .map(|(d, s)| (*d, s.to_bits()))
                .collect::<Vec<_>>(),
            all.iter()
                .map(|(d, s)| (*d, s.to_bits()))
                .collect::<Vec<_>>(),
            "{pruning:?}"
        );
    }
}

/// Blocks that end at different docids: the body's `a` is on every document in blocks of 128, scoring low but for
/// doc 200 (short, `a` five times), and lacking from docs 1024 to 4095; the title holds `a` and `b` on docs 0 and 600 only (one block, through 600).
/// A bound taken at 0 holds only through 127, the first of those blocks to end; past it, doc 200 beats it.
fn staggered_blocks() -> (CollectionSchema, Fields, Segment) {
    let (schema, fields) = schema();
    let filler = |n: usize| {
        (0..n)
            .map(|j| format!(" f{j}"))
            .collect::<Vec<_>>()
            .concat()
    };
    // Docs from 1024 on lack `a`, so it has an `idf` that counts.
    let docs: Vec<Document> = (0..4096_usize)
        .map(|i| {
            let body = match i {
                200 => "a a a a a".to_owned(),
                0..1024 => format!("a{}", filler(30)),
                _ => filler(30),
            };
            let mut d = Document::new(format!("d{i}")).with(fields.body, Value::String(body));
            if i == 0 || i == 600 {
                d = d.with(fields.title, Value::String(format!("a b{}", filler(20))));
            }
            d
        })
        .collect();
    let segment = segment(&schema, &docs);
    (schema, fields, segment)
}

/// The documents a pruning matcher returns under threshold `min`, set before the first.
fn returned_under(
    plan: &SearchPlan,
    view: &impl SearchView,
    pruning: Pruning,
    min: f64,
) -> Vec<u32> {
    let mut m = plan.matcher_with(view, pruning).unwrap();
    m.set_min_competitive(min);
    let mut out = Vec::new();
    while let Some(d) = m.next().unwrap() {
        out.push(d);
    }
    out
}

/// A word over two fields bounds a range by the field block that ends first.
#[test]
fn a_word_bounds_through_the_first_block_to_end() {
    let (schema, f, segment) = staggered_blocks();
    let segment = Arc::new(segment);
    let stats = SegmentStatistics::new(&Arc::new(schema.clone()), &[Arc::clone(&segment)]);
    let view = SegmentView::new(&segment, None);
    let word = Shape::Combined(0, 0, 1.0).query(&f);
    let Query::Combined(mut q) = word else {
        panic!()
    };
    for c in &mut q.fields {
        c.term = "a".into();
    }
    let plan = SearchPlan::new(&Query::Combined(q), &schema, &stats).unwrap();
    let first = plan
        .matcher(&view)
        .unwrap()
        .block_bound(0)
        .unwrap()
        .unwrap();
    assert_eq!(first.up_to, 127);
    let all = exhaust(&plan, &view);
    assert!(
        all[&200] > first.score,
        "doc 200 must beat the first range's bound"
    );
    let got = returned_under(&plan, &view, Pruning::BlockMaxWand, first.score);
    assert!(got.contains(&200), "{got:?}");
    assert!(!got.iter().any(|&d| d < 128), "{got:?}");
}

/// Block-max WAND skips from the pivot only through the first of its clauses' blocks to end.
#[test]
fn block_max_wand_skips_through_the_first_block_to_end() {
    let (schema, f, segment) = staggered_blocks();
    let segment = Arc::new(segment);
    let stats = SegmentStatistics::new(&Arc::new(schema.clone()), &[Arc::clone(&segment)]);
    let view = SegmentView::new(&segment, None);
    let body_a = Query::Term(TermQuery {
        field: f.body,
        term: "a".into(),
    });
    // A rare word: boosted down, so its bound does not hide doc 200.
    let title_b = Query::Boost(BoostQuery {
        query: Box::new(Query::Term(TermQuery {
            field: f.title,
            term: "b".into(),
        })),
        boost: 0.01,
    });
    let block_at_0 = |q: &Query| {
        let plan = SearchPlan::new(q, &schema, &stats).unwrap();
        plan.matcher(&view)
            .unwrap()
            .block_bound(0)
            .unwrap()
            .unwrap()
    };
    let (a, b) = (block_at_0(&body_a), block_at_0(&title_b));
    assert_eq!((a.up_to, b.up_to), (127, 600));
    // The bound of the documents from 0 through 127, summed in query order.
    let min = 0.0 + a.score + b.score;
    let or = Query::Bool(BoolQuery {
        should: vec![body_a, title_b],
        ..BoolQuery::default()
    });
    let plan = SearchPlan::new(&or, &schema, &stats).unwrap();
    let all = exhaust(&plan, &view);
    assert!(all[&200] > min, "doc 200 must beat the bound at 0");
    let got = returned_under(&plan, &view, Pruning::BlockMaxWand, min);
    assert!(got.contains(&200), "{got:?}");
    for (d, s) in &all {
        assert!(
            *s <= min || got.contains(d),
            "doc {d} scoring {s} skipped under {min}"
        );
    }
}

/// A phrase's bound is its terms' smallest `max_tf`: here a document holds the phrase as often as each term.
#[test]
fn a_phrase_bound_covers_a_phrase_on_every_occurrence() {
    let (schema, f) = schema();
    let docs: Vec<Document> = ["a b a b a b", "a b", "b a c", "a c b"]
        .iter()
        .enumerate()
        .map(|(i, body)| {
            Document::new(format!("d{i}")).with(f.body, Value::String((*body).to_owned()))
        })
        .collect();
    let segment = Arc::new(segment(&schema, &docs));
    let stats = SegmentStatistics::new(&Arc::new(schema.clone()), &[Arc::clone(&segment)]);
    let view = SegmentView::new(&segment, None);
    let Query::Phrase(mut q) = Shape::Phrase(0, 0).query(&f) else {
        panic!()
    };
    q.terms[0].term = "a".into();
    q.terms[1].term = "b".into();
    let plan_ab = SearchPlan::new(&Query::Phrase(q), &schema, &stats).unwrap();
    let all = exhaust(&plan_ab, &view);
    assert_eq!(all.keys().copied().collect::<Vec<_>>(), [0, 1]);
    bounds_hold(&plan_ab, &view, &all, "\"a b\"");
}
