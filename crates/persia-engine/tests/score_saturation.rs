//! Scores saturate at `f64::MAX` (ROADMAP 8.1.12; SPEC §7.1, §7.2, §7.4, §7.6, §7.7).
//!
//! SPEC §7.1 allows a `constant_score` up to `f64::MAX` and a `boost` up to 1000, so a `boost` of a large
//! `constant_score`, or a `bool` summing several, overflows to `+∞` in plain arithmetic. Every node that produces,
//! sums or boosts a score saturates its own result at `f64::MAX`.
//!
//! Invariants:
//! 1. No hit score, rank score or explanation node is ever non-finite.
//! 2. Per node: a `constant_score` is its score; a `boost` is `sat(inner · boost)`, where `inner` is already
//!    saturated (so a `boost` of 0.5 around an overflowing `bool` scores `f64::MAX · 0.5`); a `bool` is
//!    `sat(sat(Σ must) + sat(Σ matching should))`, each sum in query order (only one part: that part).
//! 3. Saturation is monotonic: finite scores below the overflow keep their order, overflowing ones tie at `f64::MAX`
//!    and order by location (segment order, memtable last, then docid; SPEC §7.4), under every pruning strategy, with
//!    identical hits and scores bit for bit; paging by cursor over the ties neither repeats nor skips a hit.
//! 4. An explanation's root is the hit's score bit for bit (SPEC §7.7), every nested node finite.
//! 5. A relaxed search's saturated hit has the step's top `rank_score`, and the cascade order (stricter step first)
//!    holds (SPEC §7.6).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    missing_docs
)] // Tests: inputs are fixed or generated.

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_engine::db::{
    CollectionViews, Cursor, Database, DatabaseOptions, Execution, Located, ManualClock, SearchHit,
    SearchRequest, SegmentIds, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{BoolQuery, BoostQuery, ConstantScoreQuery, Query, TermQuery};
use persia_engine::relax::{RelaxOptions, RelaxPlan, rank_score};
use persia_engine::schema::{
    Fallback, FieldId, FieldKind, Level, Levels, RelaxationConfig, SchemaBuilder, TextOptions,
};
use persia_engine::search::{Explanation, Projection, Pruning, SearchPlan};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};

const STRATEGIES: [Pruning; 3] = [
    Pruning::Exhaustive,
    Pruning::MaxScore,
    Pruning::BlockMaxWand,
];

const MAX: f64 = f64::MAX;

/// The model's saturation: what every node does to its own score.
fn sat(score: f64) -> f64 {
    if score > MAX { MAX } else { score }
}

// ---------------------------------------------------------------------------------------------------------------
// Fixture

/// A database whose collection `c` has one `text` field `body`: each of `segments` committed as one segment, in order,
/// then `memtable` put and left uncommitted.
fn database(
    segments: &[&[(&str, &str)]],
    memtable: &[(&str, &str)],
) -> (Database<SimFile>, FieldId) {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(12),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [8; 16], options).unwrap();
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    db.create_collection(schema).unwrap();
    let put = |db: &mut Database<SimFile>, (id, text): &(&str, &str)| {
        let doc = Document::new(*id).with(body, Value::String((*text).into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
    };
    for segment in segments {
        for d in *segment {
            put(&mut db, d);
        }
        db.commit().unwrap();
    }
    for d in memtable {
        put(&mut db, d);
    }
    let snapshot = db.snapshot();
    assert_eq!(
        snapshot.collection("c").unwrap().segments().len(),
        segments.len(),
        "one segment per commit"
    );
    (db, body)
}

/// Runs `f` over the views of one snapshot and memtable view, with `query`'s plan: a cursor pages within them.
fn with_views<T>(
    db: &Database<SimFile>,
    query: &Query,
    f: impl FnOnce(&CollectionViews<'_>, &SearchPlan) -> T,
) -> T {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let plan =
        SearchPlan::with_terms(query, c.schema(), &c.search_statistics(&memtable), &views).unwrap();
    f(&views, &plan)
}

fn term(field: FieldId, t: &str) -> Query {
    Query::Term(TermQuery {
        field,
        term: t.into(),
    })
}

fn constant(score: f64, query: Query) -> Query {
    Query::ConstantScore(ConstantScoreQuery {
        query: Box::new(query),
        score,
    })
}

fn boost(boost: f64, query: Query) -> Query {
    Query::Boost(BoostQuery {
        query: Box::new(query),
        boost,
    })
}

fn bool_query(must: Vec<Query>, should: Vec<Query>) -> Query {
    Query::Bool(BoolQuery {
        must,
        should,
        ..BoolQuery::default()
    })
}

fn exec(pruning: Pruning) -> Execution {
    Execution::new(pruning)
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

/// A hit as the tests compare it: id, location, score bits.
type Got = (String, Located, u64);

/// Where a hit ranks among ties (SPEC §7.4): segment order, the memtable last, then docid.
fn location_key(l: Located) -> (usize, u32) {
    match l {
        Located::Segment { index, docid } => (index, docid),
        Located::Memtable(docid) => (usize::MAX, docid),
    }
}

/// Every score of `e`'s tree, depth first, root first.
fn scores(e: &Explanation, out: &mut Vec<f64>) {
    out.push(e.score());
    match e {
        Explanation::Bool {
            must,
            should,
            proximity,
            ..
        } => {
            if let Some(p) = proximity {
                out.push(p.score);
                out.extend(p.pairs.iter().map(|pair| pair.score));
            }
            for c in must.iter().chain(should) {
                scores(c, out);
            }
        }
        Explanation::Boost { query, .. } | Explanation::Constant { query, .. } => {
            scores(query, out);
        }
        Explanation::Relaxed(r) => scores(&r.explanation, out),
        Explanation::Word(w) => {
            out.extend([w.idf, w.tf]);
            out.extend(w.fields.iter().flat_map(|f| [f.factor, f.contribution]));
        }
        Explanation::Phrase(p) => out.push(p.score),
        _ => {}
    }
}

/// The explanation's root is the hit's score bit for bit, and no node of it is non-finite.
fn check_explanation(score: f64, e: &Explanation, context: &str) {
    assert_eq!(
        e.score().to_bits(),
        score.to_bits(),
        "{context}: explanation {} vs hit {score}",
        e.score()
    );
    let mut all = Vec::new();
    scores(e, &mut all);
    assert!(
        all.iter().all(|s| s.is_finite()),
        "{context}: a non-finite node in {e:?}"
    );
}

/// One page, every hit finite and explained (invariants 1 and 4), in score-then-location order (invariant 3).
fn page(
    views: &CollectionViews<'_>,
    plan: &SearchPlan,
    pruning: Pruning,
    request: &SearchRequest,
) -> Vec<Got> {
    page_and_cursor(views, plan, pruning, request).0
}

/// [`page`], and the cursor after its last hit.
fn page_and_cursor(
    views: &CollectionViews<'_>,
    plan: &SearchPlan,
    pruning: Pruning,
    request: &SearchRequest,
) -> (Vec<Got>, Option<Cursor>) {
    let request = SearchRequest {
        explain: true,
        projection: Projection::None,
        ..request.clone()
    };
    let out = views.search_with(plan, &request, &exec(pruning)).unwrap();
    let hits: Vec<Got> = out
        .hits
        .iter()
        .map(|h| {
            let context = format!("{pruning:?} {}", h.document.id());
            assert!(h.score.is_finite(), "{context}: score {}", h.score);
            let e = h
                .explanation
                .as_ref()
                .expect("explain: true explains every hit");
            check_explanation(h.score, e, &context);
            (h.document.id().to_owned(), h.location, h.score.to_bits())
        })
        .collect();
    for w in hits.windows(2) {
        let (a, b) = (f64::from_bits(w[0].2), f64::from_bits(w[1].2));
        assert!(
            a > b || (a == b && location_key(w[0].1) < location_key(w[1].1)),
            "{pruning:?}: {:?} before {:?}",
            w[0],
            w[1]
        );
    }
    (hits, out.hits.last().map(SearchHit::cursor))
}

fn request(limit: usize) -> SearchRequest {
    SearchRequest {
        limit,
        ..SearchRequest::default()
    }
}

/// Every hit of `query` (at most 100), the same under every strategy, bit for bit; the top `k` are a prefix of it, and
/// pages of `k` by cursor add up to it, without repeating or skipping a hit.
fn all_hits(db: &Database<SimFile>, query: &Query) -> Vec<Got> {
    with_views(db, query, |views, plan| {
        let whole = page(views, plan, Pruning::Exhaustive, &request(100));
        for pruning in STRATEGIES {
            assert_eq!(
                page(views, plan, pruning, &request(100)),
                whole,
                "{pruning:?}"
            );
            for k in 1..=whole.len() {
                assert_eq!(
                    page(views, plan, pruning, &request(k)),
                    whole[..k],
                    "{pruning:?} top {k}"
                );
            }
            for k in 1..=3 {
                let mut paged: Vec<Got> = Vec::new();
                let mut next = request(k);
                loop {
                    let (hits, cursor) = page_and_cursor(views, plan, pruning, &next);
                    let Some(cursor) = cursor else { break };
                    next.search_after = Some(cursor);
                    paged.extend(hits);
                    assert!(
                        paged.len() <= whole.len(),
                        "{pruning:?} pages of {k} repeat a hit"
                    );
                }
                assert_eq!(paged, whole, "{pruning:?} pages of {k}");
            }
        }
        whole
    })
}

fn ids_and_scores(hits: &[Got]) -> Vec<(&str, f64)> {
    hits.iter()
        .map(|(id, _, bits)| (id.as_str(), f64::from_bits(*bits)))
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// 1. A boost of a large constant_score

#[test]
fn a_boost_of_a_large_constant_score_saturates_at_max() {
    let (db, body) = database(&[&[("s", "x")]], &[("m", "x")]);
    let cases = [
        // (constant, boost, score)
        (MAX, 2.0, MAX),
        (MAX, 1000.0, MAX),
        (MAX / 1.5, 2.0, MAX),
        (MAX / 1.5, 1000.0, MAX),
        (MAX / 1.5, 1.5, sat(MAX / 1.5 * 1.5)),
        (MAX * 0.75, 1.5, MAX),
        (MAX, 1.0, MAX),
        (MAX, 0.5, MAX * 0.5),
        (1e300, 1000.0, 1e303),
    ];
    for (score, factor, want) in cases {
        let query = boost(factor, constant(score, term(body, "x")));
        let hits = all_hits(&db, &query);
        assert_eq!(
            ids_and_scores(&hits),
            [("s", want), ("m", want)],
            "constant_score {score:e} boost {factor}"
        );
        // JSON has no `Infinity` (SPEC §12): a saturated score is still a plain number, read back exactly.
        let json = serde_json::to_string(&want).unwrap();
        assert_eq!(serde_json::from_str::<f64>(&json).unwrap(), want, "{json}");
    }
}

#[test]
fn every_boost_saturates_its_own_score_before_the_next_scales_it() {
    let (db, body) = database(&[&[("s", "x")]], &[("m", "x")]);
    let x = || constant(MAX, term(body, "x"));
    // The inner boost overflows and saturates at MAX: the outer one halves MAX, not `+∞`.
    let hits = all_hits(&db, &boost(0.5, boost(2.0, x())));
    assert_eq!(ids_and_scores(&hits), [("s", MAX * 0.5), ("m", MAX * 0.5)]);
    let hits = all_hits(&db, &boost(0.001, boost(1000.0, boost(1000.0, x()))));
    assert_eq!(
        ids_and_scores(&hits),
        [("s", MAX * 0.001), ("m", MAX * 0.001)]
    );
    // Finite all the way: no saturation, the plain product.
    let hits = all_hits(
        &db,
        &boost(1000.0, boost(1000.0, constant(1e300, term(body, "x")))),
    );
    let want: f64 = 1e300 * 1000.0 * 1000.0;
    assert!(want.is_finite());
    assert_eq!(ids_and_scores(&hits), [("s", want), ("m", want)]);
}

// ---------------------------------------------------------------------------------------------------------------
// 2 and 3. A bool summing past MAX; order of finite scores, ties by location

/// `x` and `y` each score `0.6 · MAX`, together past MAX; `z` 1, `w` 2.
const BIG: f64 = MAX * 0.6;

fn ordering_db() -> (Database<SimFile>, FieldId) {
    database(
        &[
            &[("s0a", "x y"), ("s0b", "z"), ("s0c", "x y z")],
            &[("s1a", "w"), ("s1b", "x y"), ("s1c", "z w")],
        ],
        &[("m0", "x y w"), ("m1", "x")],
    )
}

#[test]
fn a_bool_whose_should_clauses_sum_past_max_scores_max_and_ties_break_by_location() {
    let (db, body) = ordering_db();
    let should = vec![
        constant(BIG, term(body, "x")),
        constant(BIG, term(body, "y")),
        constant(1.0, term(body, "z")),
        constant(2.0, term(body, "w")),
    ];
    let hits = all_hits(&db, &bool_query(vec![], should));
    // The overflowing ones tie at MAX, by segment order, memtable last; then the finite ones, in score order.
    assert_eq!(
        ids_and_scores(&hits),
        [
            ("s0a", MAX),
            ("s0c", MAX),
            ("s1b", MAX),
            ("m0", MAX),
            ("m1", BIG),
            ("s1c", 3.0),
            ("s1a", 2.0),
            ("s0b", 1.0),
        ]
    );
    let locations: Vec<Located> = hits.iter().take(4).map(|h| h.1).collect();
    assert!(matches!(locations[0], Located::Segment { index: 0, .. }));
    assert!(matches!(locations[2], Located::Segment { index: 1, .. }));
    assert!(matches!(locations[3], Located::Memtable(_)));
}

#[test]
fn a_bool_whose_must_clauses_sum_past_max_scores_max_and_a_half_boost_of_it_half_max() {
    let (db, body) = ordering_db();
    let must = || {
        vec![
            constant(BIG, term(body, "x")),
            constant(BIG, term(body, "y")),
        ]
    };
    let tied = ["s0a", "s0c", "s1b", "m0"];
    let hits = all_hits(&db, &bool_query(must(), vec![]));
    assert_eq!(ids_and_scores(&hits), tied.map(|id| (id, MAX)));
    // The bool saturates at MAX before the boost scales it: MAX · 0.5, not `+∞ · 0.5` saturated to MAX.
    let hits = all_hits(&db, &boost(0.5, bool_query(must(), vec![])));
    assert_eq!(ids_and_scores(&hits), tied.map(|id| (id, MAX * 0.5)));
    let should = vec![
        constant(BIG, term(body, "x")),
        constant(BIG, term(body, "y")),
    ];
    let hits = all_hits(&db, &boost(0.5, bool_query(vec![], should)));
    assert_eq!(
        ids_and_scores(&hits),
        [
            ("s0a", MAX * 0.5),
            ("s0c", MAX * 0.5),
            ("s1b", MAX * 0.5),
            ("m0", MAX * 0.5),
            ("m1", BIG * 0.5),
        ]
    );
}

#[test]
fn a_bool_whose_must_and_should_parts_sum_past_max_scores_max() {
    let (db, body) = ordering_db();
    // Each part is finite alone; their sum overflows.
    let query = bool_query(
        vec![constant(BIG, term(body, "x"))],
        vec![
            constant(BIG, term(body, "y")),
            constant(1.0, term(body, "z")),
        ],
    );
    let hits = all_hits(&db, &query);
    assert_eq!(
        ids_and_scores(&hits),
        [
            ("s0a", MAX),
            ("s0c", MAX),
            ("s1b", MAX),
            ("m0", MAX),
            ("m1", BIG),
        ]
    );
    // A `must` part that already saturated, plus a `should` part: still MAX, and a boost of it halves MAX.
    let query = boost(
        0.5,
        bool_query(
            vec![
                constant(MAX, term(body, "x")),
                constant(MAX, term(body, "y")),
            ],
            vec![
                constant(MAX, term(body, "z")),
                constant(MAX, term(body, "w")),
            ],
        ),
    );
    let hits = all_hits(&db, &query);
    assert_eq!(
        ids_and_scores(&hits),
        ["s0a", "s0c", "s1b", "m0"].map(|id| (id, MAX * 0.5))
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 5. Relaxed search

#[test]
fn a_saturated_relaxed_hit_has_its_steps_top_rank_score_and_the_cascade_order_holds() {
    let (db, body) = ordering_db();
    // L0 needs `x`, `y` and `z` (only `s0c`, at MAX); L2 drops terms, so later steps match more, also at MAX.
    let query = bool_query(
        vec![
            constant(MAX, term(body, "x")),
            constant(MAX, term(body, "y")),
            constant(MAX, term(body, "z")),
        ],
        vec![],
    );
    let relax = RelaxPlan::new(
        &RelaxationConfig::default(),
        &RelaxOptions {
            min_results: Some(*RelaxationConfig::MIN_RESULTS.end()),
            levels: Some(Levels::Custom(vec![Level::DropTerms])),
            fallback: Some(Fallback::None),
            ..RelaxOptions::default()
        },
    )
    .unwrap();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    for pruning in STRATEGIES {
        let request = SearchRequest {
            limit: 100,
            explain: true,
            projection: Projection::None,
            ..SearchRequest::default()
        };
        let out = views
            .search_relaxed(&query, &request, &relax, &exec(pruning))
            .unwrap();
        let first = &out.hits[0];
        assert_eq!(
            (first.hit.document.id(), first.step, first.hit.score),
            ("s0c", 0, MAX),
            "{pruning:?}"
        );
        assert_eq!(first.rank_score, 1.0, "{pruning:?}");
        assert!(
            out.hits.iter().any(|h| h.step > 0 && h.hit.score == MAX),
            "{pruning:?}: a later step's saturated hit"
        );
        for h in &out.hits {
            let context = format!("{pruning:?} {} step {}", h.hit.document.id(), h.step);
            assert!(h.hit.score.is_finite(), "{context}");
            assert!(h.rank_score.is_finite(), "{context}");
            assert_eq!(h.rank_score, rank_score(h.step, h.hit.score), "{context}");
            if h.hit.score == MAX {
                assert_eq!(h.rank_score, rank_score(h.step, MAX), "{context}");
                // The top of step `i`'s band is `2^−i`.
                assert_eq!(h.rank_score, 0.5_f64.powi(i32::try_from(h.step).unwrap()));
            }
            let e = h.hit.explanation.as_ref().expect("explain: true");
            assert!(matches!(e, Explanation::Relaxed(_)), "{context}");
            check_explanation(h.hit.score, e, &context);
        }
        // Stricter steps first; within a step by score then location; rank scores never increase.
        for w in out.hits.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            assert!(
                a.step < b.step
                    || (a.step == b.step
                        && (a.hit.score > b.hit.score
                            || (a.hit.score == b.hit.score
                                && location_key(a.hit.location) < location_key(b.hit.location)))),
                "{pruning:?}: {a:?} before {b:?}"
            );
            assert!(a.rank_score >= b.rank_score, "{pruning:?}");
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 6. Random trees against the model

/// Constants near and far from the overflow, and boosts in SPEC §7.1's `(0, 1000]`.
const CONSTANTS: [f64; 10] = [
    MAX,
    MAX / 1.5,
    MAX / 2.0,
    MAX / 3.0,
    MAX * 0.6,
    1e308,
    1e300,
    3.5,
    1.0,
    0.0,
];
const BOOSTS: [f64; 8] = [0.001, 0.25, 0.5, 1.0, 1.5, 2.0, 7.0, 1000.0];
const WORDS: [&str; 3] = ["a", "b", "c"];

/// A query of `bool`, `boost` and `constant_score` (of a word) nodes.
#[derive(Debug, Clone)]
enum Tree {
    Constant(f64, usize),
    Boost(Box<Tree>, f64),
    Bool(Vec<Tree>, Vec<Tree>),
}

impl Tree {
    fn query(&self, body: FieldId) -> Query {
        match self {
            Self::Constant(score, w) => constant(*score, term(body, WORDS[*w])),
            Self::Boost(t, b) => boost(*b, t.query(body)),
            Self::Bool(must, should) => bool_query(
                must.iter().map(|t| t.query(body)).collect(),
                should.iter().map(|t| t.query(body)).collect(),
            ),
        }
    }

    /// The model: whether a document holding `words` matches, and its score, every node saturated (invariant 2).
    fn score(&self, words: &[&str]) -> Option<f64> {
        match self {
            Self::Constant(score, w) => words.contains(&WORDS[*w]).then_some(*score),
            Self::Boost(t, b) => t.score(words).map(|s| sat(s * b)),
            Self::Bool(must, should) => {
                let mut required = 0.0;
                for t in must {
                    required += t.score(words)?;
                }
                let matched: Vec<f64> = should.iter().filter_map(|t| t.score(words)).collect();
                // Without `must` (or a filter), one `should` clause is required (SPEC §7.1).
                if must.is_empty() && matched.is_empty() {
                    return None;
                }
                let optional = sat(matched.iter().fold(0.0, |sum, s| sum + s));
                let required = sat(required);
                Some(match (must.is_empty(), matched.is_empty()) {
                    (false, false) => sat(required + optional),
                    (false, true) => required,
                    (true, _) => optional,
                })
            }
        }
    }
}

fn tree() -> impl Strategy<Value = Tree> {
    let leaf =
        (0..CONSTANTS.len(), 0..WORDS.len()).prop_map(|(c, w)| Tree::Constant(CONSTANTS[c], w));
    leaf.prop_recursive(4, 24, 4, |inner| {
        prop_oneof![
            (inner.clone(), 0..BOOSTS.len()).prop_map(|(t, b)| Tree::Boost(Box::new(t), BOOSTS[b])),
            (prop::collection::vec(inner, 1..4), 0..4usize).prop_map(|(mut clauses, k)| {
                let should = clauses.split_off(k.min(clauses.len()));
                Tree::Bool(clauses, should)
            }),
        ]
    })
}

/// Every non-empty set of words, twice, in two segments and the memtable: ties to break by location.
const CORPUS: [(&str, &str); 14] = [
    ("d0", "a"),
    ("d1", "a b"),
    ("d2", "c"),
    ("d3", "a b c"),
    ("d4", "b"),
    ("d5", "a c"),
    ("d6", "b c"),
    ("d7", "b c"),
    ("d8", "a"),
    ("d9", "c"),
    ("d10", "a b c"),
    ("d11", "a c"),
    ("d12", "a b"),
    ("d13", "b"),
];

#[test]
fn random_trees_of_large_constants_score_as_the_saturating_model() {
    let (db, body) = database(&[&CORPUS[..5], &CORPUS[5..10]], &CORPUS[10..]);
    let config = Config {
        cases: if cfg!(feature = "slow") { 2048 } else { 128 },
        rng_seed: RngSeed::Fixed(0x8112),
        failure_persistence: None,
        ..Config::default()
    };
    let mut runner = TestRunner::new(config);
    runner
        .run(&tree(), |t| {
            let query = t.query(body);
            let want: BTreeMap<&str, u64> = CORPUS
                .iter()
                .filter_map(|(id, text)| {
                    let words: Vec<&str> = text.split(' ').collect();
                    t.score(&words).map(|s| (*id, s.to_bits()))
                })
                .collect();
            let got = with_views(&db, &query, |views, plan| {
                let whole = page(views, plan, Pruning::Exhaustive, &request(100));
                for pruning in STRATEGIES {
                    prop_assert_eq!(&page(views, plan, pruning, &request(100)), &whole);
                    prop_assert_eq!(
                        &page(views, plan, pruning, &request(3)),
                        &whole[..3.min(whole.len())]
                    );
                }
                Ok::<_, TestCaseError>(whole)
            })?;
            let got: BTreeMap<&str, u64> = got.iter().map(|(id, _, s)| (id.as_str(), *s)).collect();
            prop_assert_eq!(got, want);
            Ok(())
        })
        .unwrap();
}
