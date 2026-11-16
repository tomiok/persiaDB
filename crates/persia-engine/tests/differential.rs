//! The differential suite (ROADMAP 7.8.1; SPEC §7.1–§7.4, §7.7, §8.3): random corpora written through a `Database`
//! (puts, upserts, deletes, commits that flush the memtable into segments and drop the segments left with no live
//! document), then random queries with filters at any depth, searched over the whole collection, against the
//! reference (`tests/reference`), which scans the live documents and counts the statistics itself. The reference
//! evaluates every filter kind SPEC §7.1.2 allows (ROADMAP 8.1.15: `term`, `terms`, `range` with every bound shape,
//! `exists`, `and`, `or`, `not`) by the documents' values, beside `must`, beside `should` alone and beside a
//! `minimum_should_match`.
//!
//! Invariants, after every operation:
//! 1. A search returns exactly the live documents the reference matches, filters included, each once, with
//!    the reference's score within [`EPSILON`] of the query's scale. Statistics count every document of every
//!    segment, deleted or replaced ones too, plus the memtable's live documents (SPEC §7.2).
//! 2. Hits rank in the reference's order: no hit above one the reference scores higher beyond the tolerance, every
//!    pair compared. Ties rank by segment order with the memtable last, then docid (SPEC §7.3).
//! 3. Every pruning strategy, on the calling thread and on a pool, returns the same page, scores bit for bit; a page
//!    of `k` is the first `k` of the full ranking, and the page after its last cursor is the next `k`.
//! 4. Each hit's explanation scores the hit, bit for bit; each field of a word lists the frequency the reference
//!    counts, and the contributions sum to its `tf~` (SPEC §7.7). Explaining every docid of a view in one pass
//!    (`SearchPlan::explain_all`, ROADMAP 7.7.2), in descending order and one past the end, gives what a fresh tree
//!    gives each of them, matched or not.
//! 5. Relaxed with the collection's levels, a search returns what the reference cascade returns (SPEC §7.6, ROADMAP
//!    8.1.2, 8.2.2, 8.2.5, 8.2.6, 8.1.15, 8.1.19): L0, then L2 `drop_terms`' `minimum_should_match` ladder over a
//!    root `bool`'s `must` clauses beside its `must_not` and `filter`, then L3 `prefix` (the last term and every other
//!    of 3 characters or more as a `prefix` of 50 expansions, after the tier step `prefix:last` unless L2's last rung
//!    came first), then L5 `fallback` (every live document the root's filters keep and its `must_not` does not match,
//!    by recency: memtable first, then segments and docids descending), and under `filters: "last_resort"` L6
//!    `filters` (the root without its filters, when it has some and searches a word), each step's matches the earlier
//!    steps lack, scored as the reference scores that step; L0 alone, ranked as a plain search, when it has
//!    `min_results` hits. Every step after L0 is held to the case's `max_expansions` beside the steps it excludes
//!    (SPEC §7.1.3): `applied` lists it `:cut` or `:skipped` as the reference does. The response is flagged
//!    `fallback` exactly when L5 is the last step that found a hit, and `filters_dropped` exactly when L6 is. User
//!    text, compiled as SPEC §7.1.1 says (checked), also states its `original` and its hit step's `effective`. A plan
//!    without L2 runs too, so L3 meets L0's misses alone.
//! 6. A sort led by the score with a later key on the `n` column (each order and `missing` pair, with the proximity
//!    bonus and without, in turn) ranks as the score order re-sorted, among exact score ties, by the reference's `n`
//!    values, then location: every strategy, pages walked by cursor across ties, and on the pool a page of `k` and
//!    the one after its cursor (ROADMAP 7.4.11; SPEC §7.4).
//!
//! Queries hold `prefix` leaves of few expansions (SPEC §7.1: the terms of highest collection `df`, ties by bytes, one
//! shared `idf`), checked by invariants 1–4 like any leaf. The vocabulary ([`VOCAB`], [`TYPOS`]) makes completions
//! common, and the test counts the steps the reference cascade builds, so the property does not pass vacuously.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::float_cmp,
    missing_docs
)] // Tests: inputs are generated; scores tie exactly or not at all.

mod reference;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::ops::Bound;
use std::sync::{Arc, Mutex};

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    CollectionSnapshot, CollectionViews, Database, DatabaseOptions, Execution, Located,
    ManualClock, RelaxedResults, SearchHit, SearchRequest, SegmentIds, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::MemtableView;
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::query::{
    BoolQuery, BoostQuery, CombinedQuery, Compiled, ConstantScoreQuery, DefaultFields, Filter,
    FilterValue, PhraseQuery, PhraseTerm, PrefixQuery, Query, QueryCompiler, QueryLimits,
    TermQuery, WeightedTerm,
};
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldId, FieldKind, FilterRelaxation, KeywordOptions, Level,
    Levels, SchemaBuilder, TextOptions,
};
use persia_engine::search::{
    Explanation, Missing, Order, Pruning, SearchPlan, Sort, SortKey, SortValue,
};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestCaseError, TestRunner};
use reference::bm25f::{Clause, Corpus, Doc, Field as RefField};
use reference::filter::selects;
use reference::search::Query as RefQuery;

/// As `tests/search.rs`: the largest difference from the reference's score, relative to the query's scale.
const EPSILON: f64 = 1e-9;
/// The documents' words: families a prefix completes (`ca` and `car` start several) and one- and two-edit neighbours
/// (`cart` and `card`, `cafe` and `café` one character apart in two bytes, `elephnat` a swap from `elephant`, `elegant`
/// two edits from it), of 3 to 8 characters, so L3 (ROADMAP 8.1.19) and L4 (ROADMAP 8.1.21) build steps often.
const VOCAB: [&str; 10] = [
    "cat", "cats", "cart", "card", "café", "cafe", "bee", "elephant", "elephnat", "elegant",
];
/// Words of queries alone, which no document holds: prefixes of the documents' words (`ca` shorter than L3 expands
/// other than last, `ele` not) and typos (`crat` a swap from `cart`, `elehpant` a swap from `elephant`, `elepant` one
/// edit from both `elephant` and `elegant`, `bees` and `carts` an insertion).
const TYPOS: [&str; 7] = ["ca", "ele", "crat", "carts", "bees", "elepant", "elehpant"];
const TAGS: [&str; 3] = ["x", "y", "z"];
const NS: [i64; 4] = [-1, 0, 1, 2];
const TITLE: usize = 0;
const BODY: usize = 1;
const TAG: usize = 2;
const IDS: u8 = 10;
const PROPTEST_SEED: u64 = 0x7801_0000_0001;
const STRATEGIES: [Pruning; 3] = [
    Pruning::Exhaustive,
    Pruning::MaxScore,
    Pruning::BlockMaxWand,
];

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
        .field("n", FieldKind::I64 { stored: false })
        .build()
        .unwrap()
}

/// `title`, `body`, `tag` (the reference's field indices), then `n`.
fn ids(schema: &CollectionSchema) -> [FieldId; 4] {
    ["title", "body", "tag", "n"].map(|n| schema.field(n).unwrap().id())
}

/// A document's values: the reference's fields and `n`.
#[derive(Debug, Clone)]
struct Values {
    doc: Doc,
    n: Option<i64>,
}

impl Values {
    fn document(&self, fields: &[FieldId; 4], id: &str) -> Document {
        let mut d = Document::new(id);
        for f in [TITLE, BODY] {
            if !self.doc[f].is_empty() {
                d = d.with(fields[f], Value::String(self.doc[f].join(" ")));
            }
        }
        match self.doc[TAG].as_slice() {
            [] => {}
            [one] => d = d.with(fields[TAG], Value::String(one.clone())),
            many => d = d.with(fields[TAG], Value::Strings(many.to_vec())),
        }
        if let Some(n) = self.n {
            d = d.with(fields[3], Value::I64(n));
        }
        d
    }
}

/// The engine's form of a reference query.
fn engine_query(q: &RefQuery, fields: &[FieldId; 4]) -> Query {
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
            filter,
            minimum_should_match,
        } => {
            let all = |qs: &[RefQuery]| qs.iter().map(|q| engine_query(q, fields)).collect();
            Query::Bool(BoolQuery {
                must: all(must),
                should: all(should),
                must_not: all(must_not),
                filter: filter.clone(),
                minimum_should_match: u32::try_from(*minimum_should_match).unwrap(),
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
        RefQuery::Prefix {
            field,
            prefix,
            max_expansions,
            ..
        } => Query::Prefix(PrefixQuery {
            field: fields[*field],
            prefix: prefix.clone(),
            max_expansions: u32::try_from(*max_expansions).unwrap(),
        }),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Strategies

fn word() -> impl Strategy<Value = String> {
    prop::sample::select(&VOCAB[..]).prop_map(str::to_owned)
}

/// A word of a query: mostly a document's, else one no document holds ([`TYPOS`]) or a document's word cut short,
/// which L3 completes.
fn query_word() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => word(),
        1 => prop::sample::select(&TYPOS[..]).prop_map(str::to_owned),
        1 => (word(), 2..8_usize).prop_map(|(w, n)| w.chars().take(n.min(w.chars().count() - 1)).collect()),
    ]
}

/// A prefix of a query word, of 1 character or more.
fn prefix_of() -> impl Strategy<Value = String> {
    (query_word(), 1..9_usize).prop_map(|(w, n)| w.chars().take(n).collect())
}

/// The `max_expansions` of a programmatic `prefix`: small enough to cut the vocabulary's expansions short, or the 50
/// the engine builds.
fn max_expansions() -> impl Strategy<Value = usize> {
    prop_oneof![2 => 1..4_usize, 1 => Just(50_usize)]
}

fn words(max: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(word(), 0..max)
}

fn values() -> impl Strategy<Value = Values> {
    (
        prop_oneof![6 => words(6), 1 => words(40)],
        words(12),
        prop::collection::vec(
            prop::sample::select(&TAGS[..]).prop_map(str::to_owned),
            0..3,
        ),
        prop::option::of(prop::sample::select(&NS[..])),
    )
        .prop_map(|(title, body, tags, n)| Values {
            doc: vec![title, body, tags],
            n,
        })
}

#[derive(Debug, Clone)]
enum Op {
    Put(u8, Values),
    Delete(u8),
    Commit,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..IDS, values()).prop_map(|(id, v)| Op::Put(id, v)),
        2 => (0..IDS).prop_map(Op::Delete),
        2 => Just(Op::Commit),
    ]
}

/// A leaf: a `term` on a text field or the keyword field, a `combined` over both text fields, a phrase, or
/// `match_all`.
fn leaf(params: Params) -> impl Strategy<Value = RefQuery> {
    let weight = prop_oneof![Just(1.0), 0.1..10.0_f64];
    prop_oneof![
        4 => (0..2_usize, query_word()).prop_map(move |(field, term)| RefQuery::Word(vec![Clause {
            field,
            term,
            weight: params[field].0,
        }])),
        1 => prop::sample::select(&TAGS[..]).prop_map(|t| RefQuery::Word(vec![Clause {
            field: TAG,
            term: t.to_owned(),
            weight: 1.0,
        }])),
        2 => (query_word(), query_word(), weight.clone(), weight).prop_map(|(a, b, wa, wb)| RefQuery::Word(vec![
            Clause { field: TITLE, term: a, weight: wa },
            Clause { field: BODY, term: b, weight: wb },
        ])),
        2 => (0..2_usize, prop::collection::vec((word(), 0..2_u32), 1..4)).prop_map(move |(field, terms)| {
            let mut at = 0;
            let terms = terms
                .into_iter()
                .enumerate()
                .map(|(i, (t, gap))| {
                    if i > 0 {
                        at += 1 + gap;
                    }
                    (t, at)
                })
                .collect();
            RefQuery::Phrase { field, terms, weight: params[field].0 }
        }),
        1 => Just(RefQuery::All),
        // SPEC §7.1 expansion (ROADMAP 8.1.19), its few `max_expansions` keeping the terms of highest `df`.
        1 => (0..2_usize, prefix_of(), max_expansions()).prop_map(move |(field, prefix, max_expansions)| {
            RefQuery::Prefix { field, prefix, max_expansions, weight: params[field].0 }
        }),
    ]
}

/// `filter` clauses of a `bool`: none half the time, else one or two (ROADMAP 8.1.15).
fn filters(fields: [FieldId; 4]) -> impl Strategy<Value = Vec<Filter>> {
    prop_oneof![
        1 => Just(Vec::new()),
        1 => prop::collection::vec(filter(fields), 1..3),
    ]
}

/// A query of the reference, its `bool`s with filters at any depth (ROADMAP 8.1.15): beside `must`, beside `should`
/// alone (which they make optional, SPEC §7.1), and beside a `minimum_should_match`.
fn query(params: Params, fields: [FieldId; 4]) -> impl Strategy<Value = RefQuery> {
    leaf(params).prop_recursive(3, 16, 4, move |inner| {
        prop_oneof![
            4 => (
                prop::collection::vec(inner.clone(), 0..3),
                prop::collection::vec(inner.clone(), 0..3),
                prop::collection::vec(inner.clone(), 0..2),
                filters(fields),
            )
                .prop_map(|(must, should, must_not, filter)| RefQuery::Bool { must, should, must_not, filter, minimum_should_match: 0 }),
            // Several required clauses and no `should`: what L2 `drop_terms` relaxes (ROADMAP 8.2.2), its steps keeping
            // the root's filters.
            2 => (prop::collection::vec(inner.clone(), 2..6), prop::collection::vec(inner.clone(), 0..2), filters(fields))
                .prop_map(|(must, must_not, filter)| RefQuery::Bool { must, should: Vec::new(), must_not, filter, minimum_should_match: 0 }),
            // `minimum_should_match` (ROADMAP 8.1.6): up to one past the `should` clauses, with or without `must` and
            // filters. The count is drawn once the clauses are, so every value from 1 to one past them comes up.
            2 => (
                prop::collection::vec(inner.clone(), 0..2),
                prop::collection::vec(inner.clone(), 1..5),
                prop::collection::vec(inner.clone(), 0..2),
                filters(fields),
            )
                .prop_flat_map(|(must, should, must_not, filter)| {
                    let most = should.len() + 1;
                    (Just(must), Just(should), Just(must_not), Just(filter), 1..=most)
                })
                .prop_map(|(must, should, must_not, filter, minimum_should_match)| RefQuery::Bool { must, should, must_not, filter, minimum_should_match }),
            // `should` clauses beside filters alone: optional at 0 (every selected document matches), required at
            // `minimum_should_match` 1 or more (ROADMAP 8.1.15).
            2 => (
                prop::collection::vec(inner.clone(), 1..4),
                prop::collection::vec(filter(fields), 1..3),
                0..3_usize,
            )
                .prop_map(|(should, filter, minimum_should_match)| RefQuery::Bool { must: Vec::new(), should, must_not: Vec::new(), filter, minimum_should_match }),
            // Wide: a disjunction past its few-clause scan, onto its heap (ROADMAP 7.4.7).
            1 => prop::collection::vec(leaf(params), 9..14)
                .prop_map(|should| RefQuery::Bool { must: Vec::new(), should, must_not: Vec::new(), filter: Vec::new(), minimum_should_match: 0 }),
            // Wide with `minimum_should_match`: the heap skipping documents on too few clauses.
            1 => (prop::collection::vec(leaf(params), 9..14), 2..5_usize).prop_map(|(should, minimum_should_match)| {
                RefQuery::Bool { must: Vec::new(), should, must_not: Vec::new(), filter: Vec::new(), minimum_should_match }
            }),
            1 => (inner.clone(), 0.5..4.0_f64).prop_map(|(q, b)| RefQuery::Boost(Box::new(q), b)),
            1 => (inner, 0.0..5.0_f64).prop_map(|(q, s)| RefQuery::Constant(Box::new(q), s)),
        ]
    })
}

/// A range bound on `n`: none, inclusive or exclusive.
fn bound(kind: u8, v: i64) -> Bound<FilterValue> {
    match kind {
        0 => Bound::Unbounded,
        1 => Bound::Included(FilterValue::I64(v)),
        _ => Bound::Excluded(FilterValue::I64(v)),
    }
}

/// A filter of every kind SPEC §7.1.2 allows (ROADMAP 8.1.15), by field id (the schema's ids do not depend on the
/// parameters): `term` and `terms` on the `keyword` `tag` and the `i64` `n` (values `n` never holds too), `range` on
/// `n` with every bound shape (`gt`/`gte`, `lt`/`lte`, either side open; never empty), `exists` on `tag`, `n` and the
/// `text` `title`, and `and`, `or` (empty too) and `not` of those.
fn filter(fields: [FieldId; 4]) -> impl Strategy<Value = Filter> {
    let (title, tag, n) = (fields[TITLE], fields[TAG], fields[3]);
    let keyword = || prop::sample::select(&TAGS[..]).prop_map(|t| FilterValue::Keyword(t.into()));
    // One past the values the documents hold: a term that selects nothing.
    let number = || (-1_i64..4).prop_map(FilterValue::I64);
    let leaf = prop_oneof![
        2 => keyword().prop_map(move |value| Filter::Term { field: tag, value }),
        2 => prop::collection::vec(keyword(), 0..3)
            .prop_map(move |values| Filter::Terms { field: tag, values }),
        1 => number().prop_map(move |value| Filter::Term { field: n, value }),
        1 => prop::collection::vec(number(), 0..3)
            .prop_map(move |values| Filter::Terms { field: n, values }),
        3 => (0..3_u8, 0..3_u8, prop::sample::select(&NS[..]), prop::sample::select(&NS[..])).prop_map(
            move |(lo, hi, a, b)| {
                let (a, b) = (a.min(b), a.max(b));
                // At least one bound, and not empty: equal bounds are both inclusive (SPEC §7.1.2).
                let (lo, hi) = match (lo, hi) {
                    (0, 0) => (1, 0),
                    (lo, hi) if a == b && lo != 0 && hi != 0 => (1, 1),
                    kinds => kinds,
                };
                Filter::Range { field: n, lower: bound(lo, a), upper: bound(hi, b) }
            },
        ),
        1 => Just(Filter::Exists { field: n }),
        1 => Just(Filter::Exists { field: tag }),
        1 => Just(Filter::Exists { field: title }),
    ];
    leaf.prop_recursive(2, 6, 2, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Filter::And),
            prop::collection::vec(inner.clone(), 0..3).prop_map(Filter::Or),
            inner.prop_map(|f| Filter::Not(Box::new(f))),
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
    ops: Vec<Op>,
    /// Each with an optional filter, and whether the query is beside it as a `should` (optional: every selected
    /// document matches, scoring the query or 0) rather than as the `must`.
    queries: Vec<(RefQuery, Option<(Filter, bool)>)>,
    /// User text, compiled and relaxed (invariant 5): what a level states of a query from `q`.
    texts: Vec<Text>,
    /// The `max_expansions` limit the relaxed steps are held to (SPEC §7.1.3), the other limits the defaults'.
    max_expansions: usize,
    k: usize,
}

/// User text of whitespace-separated words, some excluded (`-`), over the default fields `title` and `body`, and an
/// optional request filter (SPEC §7.1.1).
#[derive(Debug, Clone)]
struct Text {
    /// Each word, and whether it is excluded.
    clauses: Vec<(String, bool)>,
    filter: Option<Filter>,
}

impl Text {
    /// Each clause as written.
    fn sources(&self) -> Vec<String> {
        self.clauses
            .iter()
            .map(|(w, excluded)| {
                if *excluded {
                    format!("-{w}")
                } else {
                    w.clone()
                }
            })
            .collect()
    }

    fn render(&self) -> String {
        self.sources().join(" ")
    }

    /// SPEC §7.1.1 for this text: each word a `combined` over the default fields (in the schema's order, by `fields`),
    /// each at its schema boost; the excluded ones the root's `must_not`; one required word alone is the query, unless
    /// a filter joins it.
    fn root(&self, params: &Params, fields: &[FieldId; 4]) -> RefQuery {
        let mut order = [TITLE, BODY];
        order.sort_by_key(|&f| fields[f]);
        let word = |w: &str| {
            RefQuery::Word(
                order
                    .iter()
                    .map(|&field| Clause {
                        field,
                        term: w.to_owned(),
                        weight: params[field].0,
                    })
                    .collect(),
            )
        };
        let mut must: Vec<RefQuery> = self
            .clauses
            .iter()
            .filter(|c| !c.1)
            .map(|c| word(&c.0))
            .collect();
        let must_not: Vec<RefQuery> = self
            .clauses
            .iter()
            .filter(|c| c.1)
            .map(|c| word(&c.0))
            .collect();
        let filter: Vec<Filter> = self.filter.iter().cloned().collect();
        if must.len() == 1 && must_not.is_empty() && filter.is_empty() {
            return must.remove(0);
        }
        RefQuery::Bool {
            must,
            should: Vec::new(),
            must_not,
            filter,
            minimum_should_match: 0,
        }
    }
}

fn text(fields: [FieldId; 4]) -> impl Strategy<Value = Text> {
    (
        prop::collection::vec((query_word(), prop::bool::weighted(0.2)), 1..5),
        prop::option::weighted(0.3, filter(fields)),
    )
        .prop_map(|(clauses, filter)| Text { clauses, filter })
}

fn case() -> impl Strategy<Value = Case> {
    let fields = ids(&schema(&[(1.0, 1.2, 0.75); 2]));
    params().prop_flat_map(move |p| {
        (
            Just(p),
            prop::collection::vec(op(), 1..20),
            prop::collection::vec(
                (
                    query(p, fields),
                    prop::option::weighted(0.5, (filter(fields), any::<bool>())),
                ),
                1..4,
            ),
            prop::collection::vec(text(fields), 0..3),
            // The default, which the steps' 50-term expansions reach late in a cascade, or a limit that cuts them
            // and skips steps sooner (ROADMAP 8.1.19).
            prop_oneof![2 => Just(QueryLimits::default().max_expansions), 1 => 1..12_usize, 1 => 40..240_usize],
            1..6_usize,
        )
            .prop_map(|(params, ops, queries, texts, max_expansions, k)| Case {
                params,
                ops,
                queries,
                texts,
                max_expansions,
                k,
            })
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The model

/// What the collection holds, as the reference sees it: every segment's documents (deleted ones too, which the
/// statistics count), and the memtable's operations since the last commit.
#[derive(Default)]
struct Model {
    /// Per segment in manifest order: each document's id, values, and whether it is the live copy of its id.
    segments: Vec<Vec<(String, Values, bool)>>,
    /// Per id, its last put (`Some`) or delete (`None`) since the last commit.
    memtable: BTreeMap<String, Option<Values>>,
}

impl Model {
    fn write(&mut self, id: &str, values: Option<Values>) {
        self.memtable.insert(id.to_owned(), values);
    }

    /// A commit: the memtable's puts become a segment, every older copy of an id it names is deleted, and a segment
    /// left with no live document is dropped (SPEC §8.3, §8.5).
    fn commit(&mut self) {
        let memtable = std::mem::take(&mut self.memtable);
        for segment in &mut self.segments {
            for (id, _, live) in segment.iter_mut() {
                if memtable.contains_key(id) {
                    *live = false;
                }
            }
        }
        let flushed: Vec<_> = memtable
            .into_iter()
            .filter_map(|(id, v)| v.map(|v| (id, v, true)))
            .collect();
        if !flushed.is_empty() {
            self.segments.push(flushed);
        }
        self.segments.retain(|s| s.iter().any(|(_, _, live)| *live));
    }

    /// The live documents by id.
    fn live(&self) -> BTreeMap<String, Values> {
        let mut live = BTreeMap::new();
        for segment in &self.segments {
            for (id, v, alive) in segment {
                if *alive && !self.memtable.contains_key(id) {
                    live.insert(id.clone(), v.clone());
                }
            }
        }
        for (id, v) in &self.memtable {
            if let Some(v) = v {
                live.insert(id.clone(), v.clone());
            }
        }
        live
    }

    /// What the statistics count (SPEC §7.2).
    fn corpus(&self, params: &Params) -> Corpus {
        let mut counted: Vec<Doc> = self
            .segments
            .iter()
            .flatten()
            .map(|(_, v, _)| v.doc.clone())
            .collect();
        counted.extend(self.memtable.values().flatten().map(|v| v.doc.clone()));
        let text = |(_, k1, b): (f64, f64, f64)| RefField {
            k1,
            b,
            keyword: false,
            positions: true,
        };
        Corpus {
            fields: vec![
                text(params[0]),
                text(params[1]),
                RefField {
                    k1: 1.2,
                    b: 0.0,
                    keyword: true,
                    positions: false,
                },
            ],
            counted,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The check

fn order(location: Located) -> (usize, u32) {
    match location {
        Located::Segment { index, docid } => (index, docid),
        Located::Memtable(docid) => (usize::MAX, docid),
    }
}

fn bits(hits: &[SearchHit]) -> Vec<(Located, u64, String)> {
    hits.iter()
        .map(|h| (h.location, h.score.to_bits(), h.document.id().to_owned()))
        .collect()
}

/// The segments the engine holds are the model's: the same ids in each, in manifest order.
fn same_segments(c: &CollectionSnapshot, model: &Model) -> Result<(), TestCaseError> {
    let engine: Vec<Vec<String>> = c
        .segments()
        .iter()
        .map(|s| {
            let mut ids: Vec<String> = (0..s.doc_count()).map(|d| s.id_of(d).unwrap()).collect();
            ids.sort();
            ids
        })
        .collect();
    let modelled: Vec<Vec<String>> = model
        .segments
        .iter()
        .map(|s| {
            let mut ids: Vec<String> = s.iter().map(|(id, _, _)| id.clone()).collect();
            ids.sort();
            ids
        })
        .collect();
    prop_assert_eq!(engine, modelled);
    Ok(())
}

/// Every docid of `view`, and one past it, explained in one pass from the last: the same as each alone (ROADMAP
/// 7.7.2).
fn one_pass<V: SearchView>(plan: &SearchPlan, view: &V) -> Result<(), TestCaseError> {
    let docids: Vec<u32> = (0..=view.doc_count()).rev().collect();
    let together = plan.explain_all(view, &docids).unwrap();
    for (&docid, e) in docids.iter().zip(together) {
        prop_assert_eq!(e, plan.explain(view, docid).unwrap(), "docid {}", docid);
    }
    Ok(())
}

/// Checks a Word explanation's fields against the document: frequencies the reference counts, contributions
/// summing to `tf~`; recurses through the tree.
fn check_explanation(
    e: &Explanation,
    doc: &Doc,
    fields: &[FieldId; 4],
) -> Result<(), TestCaseError> {
    match e {
        Explanation::Word(w) => {
            let mut tf = 0.0;
            for f in &w.fields {
                let index = fields.iter().position(|&id| id == f.field).unwrap();
                let counted = doc[index]
                    .iter()
                    .filter(|t| t.as_bytes() == f.term.as_slice())
                    .count();
                prop_assert_eq!(f.tf as usize, counted);
                prop_assert_eq!(
                    f.contribution.to_bits(),
                    (f64::from(f.tf) * f.factor).to_bits()
                );
                prop_assert_eq!(f.length.is_none(), index == TAG);
                tf += f.contribution;
            }
            prop_assert_eq!(w.tf.to_bits(), tf.to_bits());
        }
        Explanation::Phrase(p) => {
            prop_assert!(p.freq >= 1);
            prop_assert_eq!(p.tf.to_bits(), (f64::from(p.freq) * p.factor).to_bits());
        }
        Explanation::Bool { must, should, .. } => {
            for c in must.iter().chain(should) {
                check_explanation(c, doc, fields)?;
            }
        }
        Explanation::Boost { query, .. } | Explanation::Constant { query, .. } => {
            check_explanation(query, doc, fields)?;
        }
        Explanation::All => {}
        other => prop_assert!(false, "unknown explanation {:?}", other),
    }
    Ok(())
}

fn check(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    model: &Model,
    case: &Case,
) -> Result<(), TestCaseError> {
    same_segments(c, model)?;
    let schema = c.schema();
    let fields = ids(schema);
    let live = model.live();
    let corpus = model.corpus(&case.params);
    let views = c.views(memtable).unwrap();
    let stats = c.search_statistics(memtable);
    let all = live.len().max(1);
    // The reference's score of `step` for live document `id`, its filters selecting by the document's values.
    let eval = |step: &RefQuery, id: &str, proximity: bool| {
        let v = &live[id];
        let document = v.document(&fields, id);
        corpus.eval_filtered(step, &v.doc, proximity, &|f| selects(f, &document, schema))
    };
    for (i, (q, filter)) in case.queries.iter().enumerate() {
        // A case's filter joins its query at a root of its own (SPEC §7.1): the query beside it as the `must`, or as a
        // `should`, which the filter makes optional (every selected document matches, scoring the query or 0). The
        // reference evaluates that root, filters included (ROADMAP 8.1.15).
        let root = match filter {
            Some((f, should)) => {
                let (must, should) = if *should {
                    (Vec::new(), vec![q.clone()])
                } else {
                    (vec![q.clone()], Vec::new())
                };
                RefQuery::Bool {
                    must,
                    should,
                    must_not: Vec::new(),
                    filter: vec![f.clone()],
                    minimum_should_match: 0,
                }
            }
            None => q.clone(),
        };
        let query = engine_query(&root, &fields);
        query.validate(schema, &QueryLimits::default()).unwrap();
        // Planned over the views' terms, which a `prefix` expands over (SPEC §7.1).
        let plan = SearchPlan::with_terms(&query, schema, &stats, &views)
            .unwrap()
            .with_proximity();

        // Invariant 1: the matches and their scores, with the proximity bonus on (SPEC §7.2; off by default).
        let matches = |proximity: bool| -> BTreeMap<&String, (f64, f64)> {
            live.keys()
                .filter_map(|id| eval(&root, id, proximity).map(|s| (id, s)))
                .collect()
        };
        let want = matches(true);
        let request = SearchRequest {
            limit: all,
            explain: true,
            ..SearchRequest::default()
        };
        let full = views
            .search(&plan, &request, Pruning::Exhaustive)
            .unwrap()
            .hits;
        let got: BTreeMap<&str, f64> = full.iter().map(|h| (h.document.id(), h.score)).collect();
        prop_assert_eq!(got.len(), full.len(), "an id twice");
        prop_assert_eq!(
            got.keys().copied().collect::<Vec<_>>(),
            want.keys().map(|s| s.as_str()).collect::<Vec<_>>(),
            "{:?}",
            query
        );
        for (id, (score, scale)) in &want {
            let s = got[id.as_str()];
            prop_assert!(
                (s - score).abs() <= EPSILON * scale.max(1.0),
                "{}: {} vs reference {} (scale {}) for {:?}",
                id,
                s,
                score,
                scale,
                query
            );
        }

        // Invariant 1 without the proximity bonus (SPEC §7.2): the same matches, scored as the reference does
        // without it.
        let off = plan.clone().without_proximity();
        let no_bonus = views
            .search(&off, &request, Pruning::Exhaustive)
            .unwrap()
            .hits;
        let ids: BTreeMap<&str, f64> = no_bonus
            .iter()
            .map(|h| (h.document.id(), h.score))
            .collect();
        prop_assert_eq!(ids.len(), no_bonus.len(), "an id twice without proximity");
        prop_assert_eq!(
            ids.keys().copied().collect::<Vec<_>>(),
            want.keys().map(|s| s.as_str()).collect::<Vec<_>>(),
            "without proximity: {:?}",
            query
        );
        for h in &no_bonus {
            let (score, scale) = eval(&root, h.document.id(), false).unwrap();
            prop_assert!(
                (h.score - score).abs() <= EPSILON * scale.max(1.0),
                "{}: {} vs reference {} without proximity for {:?}",
                h.document.id(),
                h.score,
                score,
                query
            );
        }

        // Invariant 2: the order is the reference's: no hit ranks above one the reference scores higher beyond the
        // scores' tolerance (every pair, since a tolerance is not transitive), so a page of `k` is the reference's top
        // `k` up to near-ties.
        let reference: Vec<(f64, f64)> = full
            .iter()
            .map(|h| {
                want.iter()
                    .find(|(id, _)| id.as_str() == h.document.id())
                    .map(|(_, s)| *s)
                    .unwrap()
            })
            .collect();
        for (i, (above, above_scale)) in reference.iter().enumerate() {
            for (j, (below, below_scale)) in reference.iter().enumerate().skip(i + 1) {
                let tolerance = 2.0 * EPSILON * above_scale.max(*below_scale).max(1.0);
                prop_assert!(
                    *above >= below - tolerance,
                    "rank {} ({}, reference {}) above rank {} ({}, reference {}) for {:?}",
                    i,
                    full[i].document.id(),
                    above,
                    j,
                    full[j].document.id(),
                    below,
                    query
                );
            }
        }
        for pair in full.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            prop_assert!(
                a.score > b.score || (a.score == b.score && order(a.location) < order(b.location)),
                "{:?} before {:?}",
                (a.location, a.score),
                (b.location, b.score)
            );
        }

        // Invariant 4: explanations.
        for h in &full {
            let e = h.explanation.as_ref().unwrap();
            prop_assert_eq!(e.score().to_bits(), h.score.to_bits());
            check_explanation(e, &live[h.document.id()].doc, &fields)?;
        }
        for segment in views.segments() {
            one_pass(&plan, &segment)?;
        }
        one_pass(&plan, views.memtable())?;

        // Invariant 3: every strategy and thread, pages and cursors.
        for pruning in STRATEGIES {
            for threads in [Threads::Current, Threads::Global] {
                let exec = Execution::new(pruning).threads(threads);
                let whole = views.search_with(&plan, &request, &exec).unwrap().hits;
                prop_assert_eq!(bits(&whole), bits(&full), "{:?}", pruning);
                let page = SearchRequest {
                    limit: case.k,
                    ..SearchRequest::default()
                };
                let first = views.search_with(&plan, &page, &exec).unwrap().hits;
                prop_assert_eq!(&bits(&first)[..], &bits(&full)[..case.k.min(full.len())]);
                if let Some(last) = first.last() {
                    let next = SearchRequest {
                        search_after: Some(last.cursor()),
                        ..page
                    };
                    let second = views.search_with(&plan, &next, &exec).unwrap().hits;
                    let end = (2 * case.k).min(full.len());
                    prop_assert_eq!(bits(&second), bits(&full[first.len()..end]).clone());
                }
            }
        }

        check_score_then_column(
            &views,
            [(&plan, &full), (&off, &no_bonus)],
            &live,
            (schema, fields[3]),
            (case.k, live.len() + i),
        )?;

        // Invariant 5: relaxed, the reference cascade's steps (ROADMAP 8.1.19).
        let reference = Reference {
            schema,
            corpus: &corpus,
            params: &case.params,
            live: &live,
            eval: &|q, id| eval(q, id, false),
            max_expansions: case.max_expansions,
        };
        check_relaxed(
            &views,
            Engine::Query(&query),
            &reference,
            (&root, None),
            Some(&no_bonus),
            case,
        )?;
    }
    // Invariant 5 for user text: compiled as SPEC §7.1.1 says (checked against the reference's own form), then
    // relaxed, each step's `effective` the reference's.
    let registry = AnalyzerRegistry::builtin();
    let text_compiler = QueryCompiler::new(schema, &registry).unwrap();
    let defaults = DefaultFields::all(schema);
    for text in &case.texts {
        let mut compiled =
            text_compiler.compile(&text.render(), &defaults, &QueryLimits::default());
        if let Some(f) = &text.filter {
            compiled = compiled.with_filter([f.clone()]);
        }
        let root = text.root(&case.params, &fields);
        prop_assert_eq!(
            compiled.query(),
            &engine_query(&root, &fields),
            "{}",
            text.render()
        );
        let reference = Reference {
            schema,
            corpus: &corpus,
            params: &case.params,
            live: &live,
            eval: &|q, id| eval(q, id, false),
            max_expansions: case.max_expansions,
        };
        check_relaxed(
            &views,
            Engine::Compiled(&compiled),
            &reference,
            (&root, Some(text)),
            None,
            case,
        )?;
    }
    Ok(())
}

/// Invariant 6: a sort led by the score with a later key on the `n` column (ROADMAP 7.4.11; SPEC §7.4), both orders
/// and `missing` placements, ranks as the reference does: a plan's whole score order (its scores the reference's
/// within the tolerance) re-sorted, among hits tied bit for bit on the score, by the reference's own `n` values, then
/// location. On the calling thread every strategy returns that order, scores bit for bit, and so do pages of `k`
/// walked by cursor, a page boundary falling inside a tie group (the vocabulary and `n` are small, so ties are
/// common). On the pool, where only a page smaller than the matches gives the views a threshold to share, the first
/// page of `k` and the page after its cursor are slices of it.
///
/// Each call runs two sorts, one per plan (with proximity, then the default plan without it), each costing a whole
/// search, a cursor walk and two pages per strategy. `turn` picks which of the four order and `missing` pairs: the
/// plan with proximity takes pair `turn % 4`, the default plan the next one, so as `turn` varies (it is the live
/// count plus the query's index, and the check runs after every operation) each plan meets every pair.
fn check_score_then_column(
    views: &CollectionViews<'_>,
    plans: [(&SearchPlan, &[SearchHit]); 2],
    live: &BTreeMap<String, Values>,
    (schema, n): (&CollectionSchema, FieldId),
    (k, turn): (usize, usize),
) -> Result<(), TestCaseError> {
    const PAIRS: [(Order, Missing); 4] = [
        (Order::Desc, Missing::Last),
        (Order::Asc, Missing::First),
        (Order::Desc, Missing::First),
        (Order::Asc, Missing::Last),
    ];
    let sorts = [PAIRS[turn % 4], PAIRS[(turn + 1) % 4]];
    for ((plan, full), (order, missing)) in plans.into_iter().zip(sorts) {
        let later = SortKey::Field {
            field: n,
            order,
            missing,
        };
        let rank = |h: &SearchHit| -> (bool, i64) {
            let v = live[h.document.id()].n;
            let first = v.is_none() == (missing == Missing::First);
            let v = v.unwrap_or(0);
            (!first, if order == Order::Desc { -v } else { v })
        };
        let mut want: Vec<SearchHit> = full.to_vec();
        // `full` is in score order, ties by location: a stable sort keeps the location order within a tie on `n`.
        want.sort_by(|a, b| {
            (b.score + 0.0)
                .total_cmp(&(a.score + 0.0))
                .then_with(|| rank(a).cmp(&rank(b)))
        });
        let sort = Sort::new([SortKey::score(), later], schema).unwrap();
        let what = (order, missing, plan.proximity());
        for pruning in STRATEGIES {
            let exec = Execution::new(pruning).threads(Threads::Current);
            let whole = SearchRequest {
                sort: sort.clone(),
                limit: full.len().max(1),
                ..SearchRequest::default()
            };
            let got = views.search_with(plan, &whole, &exec).unwrap().hits;
            prop_assert_eq!(bits(&got), bits(&want), "{:?} {:?}", what, pruning);
            for h in &got {
                let key = live[h.document.id()].n.map_or(SortValue::Missing, |v| {
                    SortValue::Key(persia_format::for_codec::i64_to_key(v))
                });
                prop_assert_eq!(&h.sort[1], &key);
            }
            let page = |after: Option<persia_engine::db::Cursor>| SearchRequest {
                sort: sort.clone(),
                limit: k,
                search_after: after,
                ..SearchRequest::default()
            };
            let mut seen: Vec<SearchHit> = Vec::new();
            let mut after = None;
            loop {
                let hits = views.search_with(plan, &page(after), &exec).unwrap().hits;
                let Some(last) = hits.last() else {
                    break;
                };
                after = Some(last.cursor());
                seen.extend(hits);
                prop_assert!(seen.len() <= want.len(), "pages repeat hits");
            }
            prop_assert_eq!(
                bits(&seen),
                bits(&want),
                "pages of {}, {:?} {:?}",
                k,
                what,
                pruning
            );
            // On the pool: a page of `k`, then the page after its last cursor.
            let pool = Execution::new(pruning).threads(Threads::Global);
            let first = views.search_with(plan, &page(None), &pool).unwrap().hits;
            prop_assert_eq!(&bits(&first)[..], &bits(&want)[..k.min(want.len())]);
            if let Some(last) = first.last() {
                let second = views
                    .search_with(plan, &page(Some(last.cursor())), &pool)
                    .unwrap()
                    .hits;
                let end = (first.len() + k).min(want.len());
                prop_assert_eq!(
                    bits(&second),
                    bits(&want[first.len()..end]),
                    "pool, {:?} {:?}",
                    what,
                    pruning
                );
            }
        }
    }
    Ok(())
}

fn database(schema: CollectionSchema) -> Database<SimFile> {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        // No tier merge or deletion rewrite, so every segment keeps what it was flushed with (the model's); a segment
        // with no live document is still dropped, as the model does. Merges run inside the commit.
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: usize::MAX,
                deleted_percent: 100,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [5; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    db
}

fn run_case(case: &Case) -> Result<(), TestCaseError> {
    let schema = schema(&case.params);
    let fields = ids(&schema);
    let mut db = database(schema);
    let mut model = Model::default();
    for op in &case.ops {
        match op {
            Op::Put(id, v) => {
                let id = format!("d{id}");
                db.put("c", &v.document(&fields, &id), Durability::Buffered)
                    .unwrap();
                model.write(&id, Some(v.clone()));
            }
            Op::Delete(id) => {
                let id = format!("d{id}");
                db.delete("c", &id, Durability::Buffered).unwrap();
                model.write(&id, None);
            }
            Op::Commit => {
                db.commit().unwrap();
                model.commit();
            }
        }
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        check(c, &memtable, &model, case)?;
    }
    Ok(())
}

fn run<S>(
    name: &'static str,
    cases: u32,
    strategy: S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) where
    S: Strategy,
    S::Value: Debug,
{
    let seed = std::env::var("PROPTEST_RNG_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(PROPTEST_SEED);
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(cases);
    let config = ProptestConfig {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        source_file: Some(file!()),
        test_name: Some(name),
        ..ProptestConfig::default()
    };
    let mut runner = TestRunner::new(config);
    if let Err(e) = runner.run(&strategy, test) {
        panic!("{name} failed with PROPTEST_RNG_SEED={seed}: {e}\n{runner}");
    }
}

/// Invariants 1–5 over random histories and queries. The cascade's levels must run, or invariant 5 passes vacuously:
/// each kind of step listed is built, and finds hits, in some case (ROADMAP 8.1.19).
#[test]
fn collection_searches_match_the_reference() {
    let cases = if cfg!(feature = "slow") { 512 } else { 48 };
    run(
        "collection_searches_match_the_reference",
        cases,
        case(),
        |case| run_case(&case),
    );
    let coverage = COVERAGE.lock().unwrap().clone();
    eprintln!("steps the reference cascade built: {coverage:#?}");
    if std::env::var("PROPTEST_CASES").is_err() {
        for key in [
            "drop_terms:75% hit",
            "prefix:last hit",
            "prefix hit",
            "prefix:cut",
            "prefix:last:cut hit",
            "prefix:skipped",
            "fallback hit",
            "filters hit",
        ] {
            assert!(coverage.get(key).is_some_and(|&n| n > 0), "no {key} step");
        }
    }
}

/// The model's own assumptions, by hand: a commit deletes older copies, flushes only puts, and drops a segment
/// with no live document; the statistics count deleted copies until then.
#[test]
fn the_model_tracks_segments() {
    let params = [(1.0, 1.2, 0.75); 2];
    let v = |w: &str| Values {
        doc: vec![vec![], vec![w.to_owned()], vec![]],
        n: None,
    };
    let schema = schema(&params);
    let fields = ids(&schema);
    let mut db = database(schema);
    let mut model = Model::default();
    let step = |db: &mut Database<SimFile>, model: &mut Model, op: Op| {
        match &op {
            Op::Put(id, val) => {
                let id = format!("d{id}");
                db.put("c", &val.document(&fields, &id), Durability::Buffered)
                    .unwrap();
                model.write(&id, Some(val.clone()));
            }
            Op::Delete(id) => {
                let id = format!("d{id}");
                db.delete("c", &id, Durability::Buffered).unwrap();
                model.write(&id, None);
            }
            Op::Commit => {
                db.commit().unwrap();
                model.commit();
            }
        }
        let snapshot = db.snapshot();
        same_segments(snapshot.collection("c").unwrap(), model).unwrap();
    };
    step(&mut db, &mut model, Op::Put(1, v("ant")));
    step(&mut db, &mut model, Op::Put(2, v("bee")));
    step(&mut db, &mut model, Op::Commit);
    step(&mut db, &mut model, Op::Put(1, v("cat")));
    step(&mut db, &mut model, Op::Put(3, v("dog")));
    step(&mut db, &mut model, Op::Delete(3));
    step(&mut db, &mut model, Op::Commit);
    assert_eq!(model.segments.len(), 2);
    // `d1`'s first copy still counts: 3 documents, `d3` never flushed.
    assert_eq!(model.corpus(&params).counted.len(), 3);
    step(&mut db, &mut model, Op::Delete(2));
    step(&mut db, &mut model, Op::Commit);
    // The first segment has no live document left: dropped.
    assert_eq!(model.segments.len(), 1);
    assert_eq!(model.live().len(), 1);
}

/// One operation on both the database and the model.
fn apply(db: &mut Database<SimFile>, model: &mut Model, fields: &[FieldId; 4], op: &Op) {
    match op {
        Op::Put(id, v) => {
            let id = format!("d{id}");
            db.put("c", &v.document(fields, &id), Durability::Buffered)
                .unwrap();
            model.write(&id, Some(v.clone()));
        }
        Op::Delete(id) => {
            let id = format!("d{id}");
            db.delete("c", &id, Durability::Buffered).unwrap();
            model.write(&id, None);
        }
        Op::Commit => {
            db.commit().unwrap();
            model.commit();
        }
    }
}

/// SPEC §7.1 by hand (ROADMAP 8.1.19), through a database and the reference alike: a `prefix` of one expansion keeps
/// the term of highest collection `df`, which counts a segment's deleted documents and the memtable's live ones, and a
/// tie goes to the lower bytes. The term kept may be one no live document holds: the query then matches nothing.
#[test]
fn a_prefix_keeps_the_term_of_highest_collection_df() {
    let params = [(1.0, 1.2, 0.75); 2];
    let schema = schema(&params);
    let fields = ids(&schema);
    let mut db = database(schema);
    let mut model = Model::default();
    let body = |w: &str| Values {
        doc: vec![vec![], vec![w.to_owned()], vec![]],
        n: None,
    };
    let prefix = RefQuery::Prefix {
        field: BODY,
        prefix: "ca".to_owned(),
        max_expansions: 1,
        weight: 1.0,
    };
    // The term the reference keeps, its matches and the engine's hits.
    let kept = |db: &Database<SimFile>, model: &Model| {
        let corpus = model.corpus(&params);
        let terms = corpus.prefix_terms(BODY, "ca", 1);
        let mut matches: Vec<String> = model
            .live()
            .iter()
            .filter(|(_, v)| corpus.eval(&prefix, &v.doc).is_some())
            .map(|(id, _)| id.clone())
            .collect();
        matches.sort();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let stats = c.search_statistics(&memtable);
        let query = engine_query(&prefix, &ids(c.schema()));
        let plan = SearchPlan::with_terms(&query, c.schema(), &stats, &views).unwrap();
        let request = SearchRequest {
            limit: 10,
            ..SearchRequest::default()
        };
        let out = views.search(&plan, &request, Pruning::Exhaustive).unwrap();
        let mut hits: Vec<String> = out
            .hits
            .iter()
            .map(|h| h.document.id().to_owned())
            .collect();
        hits.sort();
        assert_eq!(hits, matches);
        (terms, hits)
    };
    for op in [
        Op::Put(0, body("cart")),
        Op::Put(1, body("cart")),
        Op::Put(2, body("cat")),
        Op::Commit,
        Op::Delete(0),
        Op::Delete(1),
        Op::Commit,
    ] {
        apply(&mut db, &mut model, &fields, &op);
    }
    // Both `cart`s are deleted, and still count: `cart` 2 over `cat` 1. No live document holds it.
    assert_eq!(model.segments.len(), 1);
    assert_eq!(kept(&db, &model), (vec![("cart".to_owned(), 2)], vec![]));
    // A live `cat` in the memtable ties them at 2: the lower bytes, `cart`, still.
    apply(&mut db, &mut model, &fields, &Op::Put(3, body("cat")));
    assert_eq!(kept(&db, &model), (vec![("cart".to_owned(), 2)], vec![]));
    // A memtable `cat` replaced by `cats` counts no more; a third live `cat` does: 3 over 2.
    apply(&mut db, &mut model, &fields, &Op::Put(4, body("cat")));
    apply(&mut db, &mut model, &fields, &Op::Put(4, body("cats")));
    assert_eq!(kept(&db, &model).0, [("cart".to_owned(), 2)]);
    apply(&mut db, &mut model, &fields, &Op::Put(5, body("cat")));
    let ids = |ids: &[&str]| ids.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
    assert_eq!(
        kept(&db, &model),
        (vec![("cat".to_owned(), 3)], ids(&["d2", "d3", "d5"]))
    );
}

/// SPEC §7.1.3 by hand (ROADMAP 8.1.19): L3's step for `ca` over `body` and `title` holds two `prefix`es of 50, and a
/// limit of 3 expansions cuts them to 1 each, the one left to the first in query order (`body`, the lower field id):
/// `body` keeps `cat` and `cart`, `title` its most frequent, `cafe`, not `card`. The whole case, every operation,
/// is also checked against the reference (invariants 1–5).
#[test]
fn a_cut_gives_the_expansions_left_in_query_order() {
    let doc = |title: &[&str], body: &[&str]| Values {
        doc: vec![
            title.iter().map(|&w| w.to_owned()).collect(),
            body.iter().map(|&w| w.to_owned()).collect(),
            Vec::new(),
        ],
        n: None,
    };
    let mut ops = vec![
        Op::Put(0, doc(&[], &["cat"])),
        Op::Put(1, doc(&[], &["cat"])),
        Op::Put(2, doc(&[], &["cart"])),
        Op::Put(3, doc(&["cafe"], &[])),
        Op::Put(4, doc(&["cafe"], &[])),
        Op::Put(5, doc(&["card"], &[])),
    ];
    let schema = schema(&[(1.0, 1.2, 0.75); 2]);
    let fields = ids(&schema);
    assert!(fields[BODY] < fields[TITLE], "body is first in query order");
    let text = Text {
        clauses: vec![("ca".to_owned(), false)],
        filter: None,
    };
    let case = Case {
        params: [(1.0, 1.2, 0.75); 2],
        ops: ops.clone(),
        queries: Vec::new(),
        texts: vec![text.clone()],
        max_expansions: 3,
        k: 2,
    };
    run_case(&case).unwrap();
    // The engine's hits, which `run_case` found to be the reference's.
    ops.push(Op::Commit);
    let mut db = database(schema.clone());
    let mut model = Model::default();
    for op in &ops {
        apply(&mut db, &mut model, &fields, op);
    }
    let registry = AnalyzerRegistry::builtin();
    let compiled = QueryCompiler::new(&schema, &registry).unwrap().compile(
        "ca",
        &DefaultFields::all(&schema),
        &QueryLimits::default(),
    );
    let relax = RelaxPlan::new(schema.relaxation(), &RelaxOptions::default())
        .unwrap()
        .with_limits(QueryLimits {
            max_expansions: 3,
            ..QueryLimits::default()
        });
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let request = SearchRequest {
        limit: 10,
        ..SearchRequest::default()
    };
    let exec = Execution::default().threads(Threads::Current);
    let out = views
        .search_relaxed(&compiled, &request, &relax, &exec)
        .unwrap();
    assert_eq!(out.relaxation.applied, ["prefix:cut"]);
    let mut hits: Vec<&str> = out.hits.iter().map(|h| h.hit.document.id()).collect();
    hits.sort_unstable();
    assert_eq!(hits, ["d0", "d1", "d2", "d3", "d4"]);
}

/// SPEC §5.3.4's properties of a scored length `q` of `n`: never above `n`, exactly `n` below 40, and otherwise
/// below it by less than `(n − 24) / 8`; longer fields never score as shorter ones (checked by the callers).
fn spec_quantization(n: u64, q: u64) -> Result<(), String> {
    if q > n {
        return Err(format!("{n} scores as {q}, above it"));
    }
    if n < 40 && q != n {
        return Err(format!("{n} scores as {q}, not exactly"));
    }
    if n >= 40 && (n - q) * 8 >= n - 24 {
        return Err(format!("{n} scores as {q}: (n - 24) / 8 or more below"));
    }
    Ok(())
}

/// SPEC §5.3.4 by hand: 41 − 24 = 17 = `0b1_0001` keeps `0b1000 << 1` = 16, so 40; 100 − 24 = 76 = `0b100_1100`
/// keeps `0b1001 << 3` = 72, so 96.
#[test]
fn quantization_by_hand() {
    use reference::bm25f::quantized;
    assert_eq!([23, 39, 41, 100].map(quantized), [23, 39, 40, 96]);
}

/// The reference's quantization and the engine's codec agree on every length up to 2^17, where real fields are,
/// and both keep the SPEC's properties there (ROADMAP 7.8.5): the reference states the rounding, the engine encodes
/// a byte, and neither is derived from the other.
#[test]
fn quantization_matches_the_engine_and_the_spec_for_every_short_length() {
    use reference::bm25f::quantized;
    let mut previous = 0;
    for n in 0..1u64 << 17 {
        let q = quantized(n);
        spec_quantization(n, q).unwrap();
        assert!(q >= previous, "{n} scores shorter than {}", n - 1);
        previous = q;
        let engine = persia_engine::segment::scored_length(u32::try_from(n).unwrap());
        assert_eq!(u64::from(engine), q, "length {n}");
    }
}

proptest! {
    /// As above, for lengths up to 2^31 (past it every length shares the last code).
    #[test]
    fn quantization_matches_the_engine_and_the_spec(a in 0u64..1 << 31, b in 0u64..1 << 31) {
        use reference::bm25f::quantized;
        for n in [a, b] {
            let q = quantized(n);
            prop_assert!(spec_quantization(n, q).is_ok(), "{:?}", spec_quantization(n, q));
            let engine = persia_engine::segment::scored_length(u32::try_from(n).unwrap());
            prop_assert_eq!(u64::from(engine), q, "length {}", n);
        }
        let (lo, hi) = (a.min(b), a.max(b));
        prop_assert!(quantized(lo) <= quantized(hi), "{} scores longer than {}", lo, hi);
    }
}

/// The engine's form of a query a relaxed search runs: programmatic, or compiled from user text.
#[derive(Clone, Copy)]
enum Engine<'a> {
    Query(&'a Query),
    Compiled(&'a Compiled),
}

fn search_relaxed(
    views: &CollectionViews<'_>,
    query: Engine<'_>,
    request: &SearchRequest,
    relax: &RelaxPlan,
    exec: &Execution,
) -> RelaxedResults {
    match query {
        Engine::Query(q) => views.search_relaxed(q, request, relax, exec),
        Engine::Compiled(c) => views.search_relaxed(c, request, relax, exec),
    }
    .unwrap()
}

/// How often the reference cascade built each kind of step, over a test's cases (ROADMAP 8.1.19, 8.1.21): its
/// `applied` (`:cut` and `:skipped` included), and `"<applied> hit"` when it found a hit. The property must not pass
/// because the levels it checks never run.
static COVERAGE: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());

fn cover(key: String) {
    *COVERAGE.lock().unwrap().entry(key).or_default() += 1;
}

/// Invariant 5: relaxed with the collection's levels (ROADMAP 8.1.19), held to the case's `max_expansions` (SPEC
/// §7.1.3), a search returns what the reference cascade ([`Reference::cascade`]) returns: the same steps, in order,
/// with what each applied (`:cut` and `:skipped` included) and its level; each step's matches the earlier steps lack,
/// scored and ranked as the reference scores the step's query (L5: by recency); the response's `level`, its `fallback`
/// and `filters_dropped` flags, and for user text its `original` and its hit step's `effective`. L0 alone ranks as
/// the `plain` search when it has `min_results` hits, and pages are slices of the one ranking.
fn check_relaxed(
    views: &CollectionViews<'_>,
    engine: Engine<'_>,
    reference: &Reference<'_>,
    (root, text): (&RefQuery, Option<&Text>),
    plain: Option<&[SearchHit]>,
    case: &Case,
) -> Result<(), TestCaseError> {
    let schema = reference.schema;
    let request = SearchRequest {
        limit: reference.live.len().max(1),
        explain: true,
        ..SearchRequest::default()
    };
    let limits = QueryLimits {
        max_expansions: case.max_expansions,
        ..QueryLimits::default()
    };
    let exec = Execution::default()
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)));
    // The collection's word levels, then a plan without L2, whose L3 runs on L0's misses alone and builds the tier
    // step `prefix:last` whenever it expands another term beside the last (SPEC §7.6).
    let collection = [RelaxLevel::DropTerms, RelaxLevel::Prefix];
    let without_l2 = [RelaxLevel::Prefix];
    for (min_results, filters, words) in [
        (1, FilterRelaxation::Never, &collection[..]),
        (10_000, FilterRelaxation::Never, &collection[..]),
        (1, FilterRelaxation::LastResort, &collection[..]),
        (10_000, FilterRelaxation::LastResort, &collection[..]),
        (10_000, FilterRelaxation::Never, &without_l2[..]),
    ] {
        let relax = RelaxPlan::new(
            schema.relaxation(),
            &RelaxOptions {
                min_results: Some(min_results),
                levels: Some(Levels::Custom(words.iter().map(|&l| level_of(l)).collect())),
                filters: Some(filters),
                ..RelaxOptions::default()
            },
        )
        .unwrap()
        .with_limits(limits);
        let relaxed = search_relaxed(views, engine, &request, &relax, &exec);
        let last_resort = filters == FilterRelaxation::LastResort;
        let steps = reference.cascade(root, text, words, min_results as usize, last_resort);
        for step in &steps[1..] {
            cover(step.applied.clone());
            if step.matches.as_ref().is_some_and(|m| !m.is_empty()) {
                cover(format!("{} hit", step.applied));
            }
        }
        let what = || format!("{root:?} / {:?}", text.map(Text::render));
        let applied: Vec<&str> = steps[1..].iter().map(|s| s.applied.as_str()).collect();
        prop_assert_eq!(&relaxed.relaxation.applied, &applied, "{}", what());
        let level = steps[1..]
            .iter()
            .rev()
            .find(|s| s.matches.is_some())
            .map_or(RelaxLevel::Exact, |s| s.level);
        prop_assert_eq!(relaxed.relaxation.level, level, "{}", what());
        // The hit step: the last that found a hit, L0 when none did (SPEC §7.6, ROADMAP 8.1.22).
        let hit_step = steps
            .iter()
            .rposition(|s| s.matches.as_ref().is_some_and(|m| !m.is_empty()))
            .unwrap_or(0);
        let hit_level = steps[hit_step].level;
        prop_assert_eq!(
            relaxed.relaxation.fallback.is_some(),
            hit_level == RelaxLevel::Fallback
        );
        prop_assert_eq!(
            relaxed.relaxation.filters_dropped,
            hit_level == RelaxLevel::Filters
        );
        let corrected: Vec<(&str, &str)> = relaxed
            .relaxation
            .corrected
            .iter()
            .map(|c| (c.original.as_str(), c.corrected.as_str()))
            .collect();
        let want: Vec<(&str, &str)> = steps[hit_step]
            .corrected
            .iter()
            .map(|(o, c)| (o.as_str(), c.as_str()))
            .collect();
        prop_assert_eq!(corrected, want, "{}", what());
        prop_assert_eq!(
            &relaxed.relaxation.original,
            &text.map(Text::render),
            "{}",
            what()
        );
        prop_assert_eq!(
            &relaxed.relaxation.effective,
            &steps[hit_step].effective,
            "step {} of {}",
            hit_step,
            what()
        );
        // Step order first, each step's hits its reference matches, within the tolerance, in the reference order.
        prop_assert!(relaxed.hits.windows(2).all(|w| w[0].step <= w[1].step));
        let none = BTreeMap::new();
        for (i, step) in steps.iter().enumerate() {
            let matches = step.matches.as_ref().unwrap_or(&none);
            let hits: Vec<_> = relaxed.hits.iter().filter(|h| h.step == i).collect();
            for h in &hits {
                prop_assert_eq!(h.level, step.level);
            }
            let ids: Vec<&str> = {
                let mut ids: Vec<&str> = hits.iter().map(|h| h.hit.document.id()).collect();
                ids.sort_unstable();
                ids
            };
            prop_assert_eq!(
                ids,
                matches.keys().map(String::as_str).collect::<Vec<_>>(),
                "step {} ({}) of {}",
                i,
                step.applied,
                what()
            );
            let scores: Vec<(f64, f64)> =
                hits.iter().map(|h| matches[h.hit.document.id()]).collect();
            for (h, (score, scale)) in hits.iter().zip(&scores) {
                prop_assert!(
                    (h.hit.score - score).abs() <= EPSILON * scale.max(1.0),
                    "step {} ({}): {} vs reference {} for {}",
                    i,
                    step.applied,
                    h.hit.score,
                    score,
                    what()
                );
            }
            for (a, (above, above_scale)) in scores.iter().enumerate() {
                for (below, below_scale) in scores.iter().skip(a + 1) {
                    let tolerance = 2.0 * EPSILON * above_scale.max(*below_scale).max(1.0);
                    prop_assert!(
                        *above >= below - tolerance,
                        "step {} out of order for {}",
                        i,
                        what()
                    );
                }
            }
            // Within a step, as in a plain search: score descending, ties by segment order (memtable last), then
            // docid (SPEC §7.3). L5 ranks by recency: every score 0, the memtable first, then segments and docids
            // descending.
            let l5 = step.level == RelaxLevel::Fallback;
            for pair in hits.windows(2) {
                let (a, b) = (&pair[0].hit, &pair[1].hit);
                let recency = l5 && a.score == 0.0 && b.score == 0.0;
                prop_assert!(
                    recency && order(a.location) > order(b.location)
                        || !l5 && a.score > b.score
                        || !l5 && a.score == b.score && order(a.location) < order(b.location),
                    "step {}: {:?} before {:?}",
                    i,
                    (a.location, a.score),
                    (b.location, b.score)
                );
            }
        }
        let whole: Vec<SearchHit> = relaxed.hits.iter().map(|h| h.hit.clone()).collect();
        if let (1, Some(plain)) = (steps.len(), plain) {
            // L0 alone ranks as a plain search under the default plan (no proximity).
            prop_assert_eq!(bits(&whole), bits(plain));
        }
        // At every step: a page of `k` is the first `k` of the whole ranking, and the page after its last cursor
        // the next `k` (SPEC §7.6: the pages of one query are slices of one ranking).
        let page = SearchRequest {
            limit: case.k,
            ..SearchRequest::default()
        };
        let first = search_relaxed(views, engine, &page, &relax, &exec);
        let first_hits: Vec<SearchHit> = first.hits.iter().map(|h| h.hit.clone()).collect();
        prop_assert_eq!(
            &bits(&first_hits)[..],
            &bits(&whole)[..case.k.min(whole.len())]
        );
        prop_assert_eq!(&first.relaxation, &relaxed.relaxation);
        if let Some(last) = first.hits.last() {
            let next = SearchRequest {
                limit: case.k,
                search_after: Some(last.cursor()),
                ..SearchRequest::default()
            };
            let second = search_relaxed(views, engine, &next, &relax, &exec);
            prop_assert_eq!(&second.relaxation, &first.relaxation);
            let hits: Vec<SearchHit> = second.hits.into_iter().map(|h| h.hit).collect();
            let end = (first_hits.len() + case.k).min(whole.len());
            prop_assert_eq!(bits(&hits), bits(&whole[first_hits.len()..end]));
        }
    }
    Ok(())
}

/// The reference's score of a query for the live document of an id, `None` when it does not match.
type Eval<'a> = dyn Fn(&RefQuery, &str) -> Option<(f64, f64)> + 'a;

/// The reference cascade's view of a collection (SPEC §7.6): its statistics, its live documents, how the reference
/// scores a query for one of them (filters selecting by its values), and the `max_expansions` limit the steps after
/// L0 are held to (SPEC §7.1.3; the other limits the defaults, which the generated queries stay well within).
struct Reference<'a> {
    schema: &'a CollectionSchema,
    corpus: &'a Corpus,
    params: &'a Params,
    live: &'a BTreeMap<String, Values>,
    eval: &'a Eval<'a>,
    max_expansions: usize,
}

/// A step of the reference cascade: its query as searched (cut when the limits cut it), what it applied, its level,
/// its matches the earlier steps lack (`None` when skipped), and of user text its `effective` and `corrected`.
#[derive(Debug)]
struct RefStep {
    applied: String,
    level: RelaxLevel,
    matches: Option<BTreeMap<String, (f64, f64)>>,
    effective: Option<String>,
    corrected: Vec<(String, String)>,
}

/// A step a level built, before the limits hold it.
struct Built {
    query: RefQuery,
    applied: String,
    effective: Option<String>,
    corrected: Vec<(String, String)>,
}

/// The query a level puts in place of a field's analyzed term: `(field, term, last)`, `last` for the last term.
type Leaf<'l> = dyn Fn(usize, &str, bool) -> Option<RefQuery> + 'l;

/// The fewest characters of a term L3 expands when it is not the last (SPEC §7.6).
const PREFIX_LEN: usize = 3;

impl Reference<'_> {
    /// SPEC §7.6's cascade for `root` (of user `text`, or programmatic): L0, then the plan's `words` levels in cascade
    /// order (L2 `drop_terms`, L3 `prefix`), L5 `fallback` and, `last_resort`, L6 `filters`. A level is built only
    /// while the hits found are fewer than `min_results`, and so is each of its steps. A step is held to the limit
    /// beside the earlier steps it excludes ([`Self::fit`]): as built, cut (`:cut`) or skipped (`:skipped`, not
    /// searched, excluded from none). A searched step's matches are the live documents it matches that no earlier
    /// step took.
    fn cascade(
        &self,
        root: &RefQuery,
        text: Option<&Text>,
        words: &[RelaxLevel],
        min_results: usize,
        last_resort: bool,
    ) -> Vec<RefStep> {
        let l0: BTreeMap<String, (f64, f64)> = self
            .live
            .keys()
            .filter_map(|id| (self.eval)(root, id).map(|s| (id.clone(), s)))
            .collect();
        let mut taken: BTreeSet<String> = l0.keys().cloned().collect();
        let mut steps = vec![RefStep {
            applied: String::new(),
            level: RelaxLevel::Exact,
            matches: Some(l0),
            effective: text.map(Text::render),
            corrected: Vec::new(),
        }];
        // What the later steps exclude (SPEC §7.6, the cascade), each with its level: every step searched, except the
        // earlier steps of a nested level (L2) that its latest uncut step stands for, and every step before an uncut
        // L5, which stands for them all.
        let mut excluded = vec![(RelaxLevel::Exact, root.clone())];
        let mut levels = words.to_vec();
        levels.push(RelaxLevel::Fallback);
        if last_resort {
            levels.push(RelaxLevel::Filters);
        }
        'levels: for level in levels {
            if taken.len() >= min_results {
                break;
            }
            for built in self.level(level, root, text, &steps) {
                if taken.len() >= min_results {
                    break 'levels;
                }
                let Some((query, cut)) = self.fit(&built.query, &excluded) else {
                    steps.push(RefStep {
                        applied: format!("{}:skipped", built.applied),
                        level,
                        matches: None,
                        effective: None,
                        corrected: Vec::new(),
                    });
                    continue;
                };
                let matches: BTreeMap<String, (f64, f64)> = self
                    .live
                    .keys()
                    .filter(|id| !taken.contains(*id))
                    .filter_map(|id| (self.eval)(&query, id).map(|s| (id.clone(), s)))
                    .collect();
                taken.extend(matches.keys().cloned());
                // A cut step matches less than its level built, so it stands for no earlier step (SPEC §7.1.3).
                if !cut && level == RelaxLevel::DropTerms {
                    excluded.retain(|(l, _)| *l != RelaxLevel::DropTerms);
                }
                if !cut && level == RelaxLevel::Fallback {
                    excluded.clear();
                }
                excluded.push((level, query));
                steps.push(RefStep {
                    applied: if cut {
                        format!("{}:cut", built.applied)
                    } else {
                        built.applied
                    },
                    level,
                    matches: Some(matches),
                    effective: built.effective,
                    corrected: built.corrected,
                });
            }
        }
        steps
    }

    /// The steps `level` builds for `root` once the cascade reaches it, after `steps` (SPEC §7.6).
    fn level(
        &self,
        level: RelaxLevel,
        root: &RefQuery,
        text: Option<&Text>,
        steps: &[RefStep],
    ) -> Vec<Built> {
        let l0_text = || text.map(Text::render);
        let built = |query, applied: &str, effective| Built {
            query,
            applied: applied.to_owned(),
            effective,
            corrected: Vec::new(),
        };
        match level {
            RelaxLevel::DropTerms => drop_terms_steps(root)
                .into_iter()
                .map(|(q, applied, k)| {
                    let effective = text.map(|t| self.drop_terms_effective(t, k));
                    built(q, &applied, effective)
                })
                .collect(),
            RelaxLevel::Prefix => {
                let prefix = |field: usize, term: &str| RefQuery::Prefix {
                    field,
                    prefix: term.to_owned(),
                    max_expansions: 50,
                    weight: self.params[field].0,
                };
                let every = |f: usize, t: &str, last: bool| {
                    (last || t.chars().count() >= PREFIX_LEN).then(|| prefix(f, t))
                };
                let Some(full) = self.rewrite_root(root, &every) else {
                    return Vec::new();
                };
                // The tier step `prefix:last` (ROADMAP 8.2.7): unless L2's last rung was built and not skipped.
                let dropped = steps
                    .iter()
                    .rfind(|s| s.level == RelaxLevel::DropTerms)
                    .is_some_and(|s| s.matches.is_some());
                let last = |f: usize, t: &str, last: bool| last.then(|| prefix(f, t));
                let mut out = Vec::new();
                if !dropped
                    && let Some(tier) = self.rewrite_root(root, &last)
                    && tier != full
                {
                    out.push(built(tier, "prefix:last", l0_text()));
                }
                out.push(built(full, "prefix", l0_text()));
                out
            }
            RelaxLevel::Fallback => {
                let excluded = text.map(|t| {
                    t.sources()
                        .into_iter()
                        .zip(&t.clauses)
                        .filter(|(_, c)| c.1)
                        .map(|(s, _)| s)
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                vec![built(reference_fallback(root), "fallback", excluded)]
            }
            RelaxLevel::Filters => reference_unfiltered(root)
                .map(|q| built(q, "filters", l0_text()))
                .into_iter()
                .collect(),
            other => panic!("no reference for {other:?}"),
        }
    }

    /// L2's `effective` for user text at count `k` (SPEC §7.6, ROADMAP 8.2.2.2): its `k` most important required
    /// words and every excluded one, in text order. A word no document holds is the least important, then (no
    /// stopwords here) a lower IDF, a higher `df` over its fields, then a later position.
    fn drop_terms_effective(&self, text: &Text, k: usize) -> String {
        let df = |w: &str| self.corpus.df(TITLE, w).max(self.corpus.df(BODY, w));
        let mut required: Vec<(bool, u64, usize)> = text
            .clauses
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.1)
            .map(|(i, c)| {
                let df = df(&c.0);
                (df == 0, df, i)
            })
            .collect();
        required.sort_unstable();
        let kept: BTreeSet<usize> = required.iter().take(k).map(|r| r.2).collect();
        text.sources()
            .into_iter()
            .enumerate()
            .filter(|(i, _)| text.clauses[*i].1 || kept.contains(i))
            .map(|(_, s)| s)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `step` held to the `max_expansions` limit beside the `excluded` steps (SPEC §7.1.3): every `max_expansions` of
    /// the step and of the steps it excludes is counted, those under a `must_not` too. Within the limit, the step as
    /// built (`false`). Over it, the step's own `prefix`s and `fuzzy`s outside a `must_not` are cut ([`share`]) to the
    /// expansions left (`true`); `None` (skipped) when fewer are left than it has such queries.
    fn fit(
        &self,
        step: &RefQuery,
        excluded: &[(RelaxLevel, RefQuery)],
    ) -> Option<(RefQuery, bool)> {
        let before: usize = excluded.iter().map(|(_, q)| expansions(q)).sum();
        let own = expansions(step);
        if before + own <= self.max_expansions {
            return Some((step.clone(), false));
        }
        let mut cut = step.clone();
        let mut caps = Vec::new();
        cuttable(&mut cut, &mut caps);
        let fixed = own - caps.iter().map(|c| **c).sum::<usize>();
        let left = self
            .max_expansions
            .checked_sub(before)?
            .checked_sub(fixed)?;
        if caps.is_empty() || caps.len() > left {
            return None;
        }
        share(&mut caps, left);
        Some((cut, true))
    }

    /// L3's and L4's step for `root` (SPEC §7.6): the terms of a root `bool`'s `must` clauses rewritten by `leaf`, the
    /// last clause's last term the last term, or of a word alone (under `boost`s); `None` when no term is rewritten
    /// or the root `bool` has `should` clauses or a `minimum_should_match`.
    fn rewrite_root(&self, root: &RefQuery, leaf: &Leaf<'_>) -> Option<RefQuery> {
        match root {
            RefQuery::Bool {
                should,
                minimum_should_match,
                ..
            } if !should.is_empty() || *minimum_should_match != 0 => None,
            RefQuery::Bool {
                must,
                should,
                must_not,
                filter,
                minimum_should_match,
            } => {
                let (must, changed) = self.rewrite_all(must, true, leaf);
                changed.then(|| RefQuery::Bool {
                    must,
                    should: should.clone(),
                    must_not: must_not.clone(),
                    filter: filter.clone(),
                    minimum_should_match: *minimum_should_match,
                })
            }
            other => self.rewrite(other, true, leaf),
        }
    }

    /// `must` rewritten, its last clause holding the last term when `last`; whether any changed.
    fn rewrite_all(&self, must: &[RefQuery], last: bool, leaf: &Leaf<'_>) -> (Vec<RefQuery>, bool) {
        let n = must.len();
        let mut changed = false;
        let out = must
            .iter()
            .enumerate()
            .map(|(i, c)| match self.rewrite(c, last && i + 1 == n, leaf) {
                Some(q) => {
                    changed = true;
                    q
                }
                None => c.clone(),
            })
            .collect();
        (out, changed)
    }

    /// `q` with its words' terms rewritten (SPEC §7.6 L3): a word's text fields by `leaf`, a `combined` then a `bool`
    /// of `should` clauses, its exact fields as one word and each rewritten field's query, under a `boost` of the
    /// field's weight over its schema boost when they differ; within a `bool` its `must` and `should` clauses (the
    /// last `must` clause and every `should` clause holding the last term of a last `q`), and under a `boost`.
    /// Phrases, `must_not`, filters, expansions, `match_all` and `constant_score` stay as they are.
    fn rewrite(&self, q: &RefQuery, last: bool, leaf: &Leaf<'_>) -> Option<RefQuery> {
        let open = |c: &Clause| {
            (c.field != TAG && !c.term.is_empty()).then(|| leaf(c.field, &c.term, last))?
        };
        match q {
            RefQuery::Word(clauses) if clauses.len() == 1 => open(&clauses[0]),
            RefQuery::Word(clauses) => {
                let mut exact = Vec::new();
                let mut should = Vec::new();
                for c in clauses {
                    match open(c) {
                        Some(q) => {
                            let ratio = (c.weight / self.params[c.field].0)
                                .clamp(f64::from_bits(1), 1000.0);
                            should.push(if ratio == 1.0 {
                                q
                            } else {
                                RefQuery::Boost(Box::new(q), ratio)
                            });
                        }
                        None => exact.push(c.clone()),
                    }
                }
                if should.is_empty() {
                    return None;
                }
                if !exact.is_empty() {
                    should.insert(0, RefQuery::Word(exact));
                }
                Some(if should.len() == 1 {
                    should.remove(0)
                } else {
                    RefQuery::Bool {
                        must: Vec::new(),
                        should,
                        must_not: Vec::new(),
                        filter: Vec::new(),
                        minimum_should_match: 0,
                    }
                })
            }
            RefQuery::Bool {
                must,
                should,
                must_not,
                filter,
                minimum_should_match,
            } => {
                let (must, mut changed) = self.rewrite_all(must, last, leaf);
                let should = should
                    .iter()
                    .map(|c| match self.rewrite(c, last, leaf) {
                        Some(q) => {
                            changed = true;
                            q
                        }
                        None => c.clone(),
                    })
                    .collect();
                changed.then(|| RefQuery::Bool {
                    must,
                    should,
                    must_not: must_not.clone(),
                    filter: filter.clone(),
                    minimum_should_match: *minimum_should_match,
                })
            }
            RefQuery::Boost(inner, boost) => self
                .rewrite(inner, last, leaf)
                .map(|q| RefQuery::Boost(Box::new(q), *boost)),
            _ => None,
        }
    }
}

/// The `max_expansions` of every `prefix` and `fuzzy` in `q`, under a `must_not` too, summed (SPEC §7.1.3).
fn expansions(q: &RefQuery) -> usize {
    match q {
        RefQuery::Prefix { max_expansions, .. } => *max_expansions,
        RefQuery::Bool {
            must,
            should,
            must_not,
            ..
        } => must
            .iter()
            .chain(should)
            .chain(must_not)
            .map(expansions)
            .sum(),
        RefQuery::Boost(inner, _) | RefQuery::Constant(inner, _) => expansions(inner),
        RefQuery::Word(_) | RefQuery::Phrase { .. } | RefQuery::All => 0,
    }
}

/// The `max_expansions` a cut may lower in `q` (SPEC §7.1.3), in query order: depth first, a `bool`'s `must` clauses
/// before its `should` clauses; not those under a `must_not`, which would exclude less.
fn cuttable<'q>(q: &'q mut RefQuery, caps: &mut Vec<&'q mut usize>) {
    match q {
        RefQuery::Prefix { max_expansions, .. } => caps.push(max_expansions),
        RefQuery::Bool { must, should, .. } => {
            for c in must.iter_mut().chain(should.iter_mut()) {
                cuttable(c, caps);
            }
        }
        RefQuery::Boost(inner, _) | RefQuery::Constant(inner, _) => cuttable(inner, caps),
        RefQuery::Word(_) | RefQuery::Phrase { .. } | RefQuery::All => {}
    }
}

/// SPEC §7.1.3's cut of `caps` to sum to `left`, at least 1 each: each becomes the smaller of its own and the highest
/// shared count whose sum fits, and the expansions that count leaves go one each, in query order, to those it lowered.
fn share(caps: &mut [&mut usize], left: usize) {
    let sum =
        |caps: &[&mut usize], level: usize| caps.iter().map(|c| (**c).min(level)).sum::<usize>();
    let mut level = caps.iter().map(|c| **c).max().unwrap_or(1);
    while sum(caps, level) > left {
        level -= 1;
    }
    let mut rest = left - sum(caps, level);
    for cap in caps.iter_mut() {
        if **cap > level {
            **cap = level;
            if rest > 0 {
                **cap += 1;
                rest -= 1;
            }
        }
    }
}

/// L2 `drop_terms`' steps for reference query `q` (SPEC §7.6), each with what it applied and its count: over the
/// `must` clauses of a root `bool` without `should` or a `minimum_should_match`, `minimum_should_match` of them at 75%
/// and 50% of the `n` terms (`max(1, ⌊p · n / 100⌋)`) and at 1, each count once and below L0's `n`, with the root's
/// `must_not` and `filter` (filters are not relaxed, ROADMAP 8.1.15). A query whose filter wraps it (`must: [query]`)
/// has one term. Written from the SPEC, not from the engine's ladder.
fn drop_terms_steps(q: &RefQuery) -> Vec<(RefQuery, String, usize)> {
    let mut steps = Vec::new();
    let RefQuery::Bool {
        must,
        should,
        must_not,
        filter,
        minimum_should_match: 0,
    } = q
    else {
        return steps;
    };
    if !should.is_empty() {
        return steps;
    }
    let n = must.len();
    let mut previous = n;
    for (label, count) in [
        ("75%", (n * 75 / 100).max(1)),
        ("50%", (n * 50 / 100).max(1)),
        ("1", 1),
    ] {
        if count == previous || n < 2 {
            continue;
        }
        previous = count;
        let step = RefQuery::Bool {
            must: Vec::new(),
            should: must.clone(),
            must_not: must_not.clone(),
            filter: filter.clone(),
            minimum_should_match: count,
        };
        steps.push((step, format!("drop_terms:{label}"), count));
    }
    steps
}

/// L5 `fallback`'s step as SPEC §7.6 builds it from `q`: `match_all` beside the root `bool`'s `must_not` and `filter`,
/// so excluded clauses stay excluded and filters are not relaxed; `match_all` alone without either, or for a root of
/// another kind. A deeper filter is not kept.
fn reference_fallback(q: &RefQuery) -> RefQuery {
    match q {
        RefQuery::Bool {
            must_not, filter, ..
        } if !must_not.is_empty() || !filter.is_empty() => RefQuery::Bool {
            must: vec![RefQuery::All],
            should: Vec::new(),
            must_not: must_not.clone(),
            filter: filter.clone(),
            minimum_should_match: 0,
        },
        _ => RefQuery::All,
    }
}

/// L6 `filters`' step as SPEC §7.6 builds it from a programmatic `q` (ROADMAP 8.2.6, 8.1.15): its root `bool` without
/// its filters, every other clause as it is; `None` when the root has no filter, is not a `bool`, or searches no word.
fn reference_unfiltered(q: &RefQuery) -> Option<RefQuery> {
    let RefQuery::Bool {
        must,
        should,
        must_not,
        filter,
        minimum_should_match,
    } = q
    else {
        return None;
    };
    let words = must.iter().any(searches_word) || should.iter().any(searches_word);
    (words && !filter.is_empty()).then(|| RefQuery::Bool {
        must: must.clone(),
        should: should.clone(),
        must_not: must_not.clone(),
        filter: Vec::new(),
        minimum_should_match: *minimum_should_match,
    })
}

/// Whether `q` searches a word (SPEC §7.6): a word, a phrase or an expansion, or a `bool`, `boost` or `constant_score` holding one in
/// `must` or `should`. `match_all`, an empty `bool` and exclusions alone do not.
fn searches_word(q: &RefQuery) -> bool {
    match q {
        RefQuery::Word(_) | RefQuery::Phrase { .. } | RefQuery::Prefix { .. } => true,
        RefQuery::All => false,
        RefQuery::Bool { must, should, .. } => {
            must.iter().any(searches_word) || should.iter().any(searches_word)
        }
        RefQuery::Boost(inner, _) | RefQuery::Constant(inner, _) => searches_word(inner),
    }
}

/// The schema's name of a word level.
fn level_of(level: RelaxLevel) -> Level {
    match level {
        RelaxLevel::DropTerms => Level::DropTerms,
        RelaxLevel::Prefix => Level::Prefix,
        RelaxLevel::Fuzzy => Level::Fuzzy,
        other => panic!("not a word level: {other:?}"),
    }
}
