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
//!    8.1.2, 8.2.2, 8.2.5, 8.2.6, 8.1.15, 8.1.18, 8.1.19, 8.1.21): L0, then L1 `synonyms` for user text (below), then
//!    L2 `drop_terms`' `minimum_should_match` ladder over a root `bool`'s `must` clauses beside its `must_not` and
//!    `filter`, then L3 `prefix` (the last term of 2 characters or more and every other
//!    of 3 characters or more as a `prefix` of 50 expansions, after the tier step `prefix:last` unless L2's last rung
//!    came first), then L4 `fuzzy` (each term of 4 characters or more as a `fuzzy` of 50 expansions at 1 edit, 2
//!    from 8 characters, after the tier step `fuzzy:1` when a term takes 2), then L5 `fallback` (every live document
//!    the root's filters keep and its `must_not` does not match, by recency: memtable first, then segments and docids
//!    descending), and under `filters: "last_resort"` L6
//!    `filters` (the root without its filters, when it has some and searches a word), each step's matches the earlier
//!    steps lack, scored as the reference scores that step; L0 alone, ranked as a plain search, when it has
//!    `min_results` hits. Every step after L0 is held to the case's `max_expansions` and `max_terms` beside the steps it
//!    excludes (SPEC §7.1.3): `applied` lists it `:cut` or `:skipped` as the reference does. The response is flagged
//!    `fallback` exactly when L5 is the last step that found a hit, and `filters_dropped` exactly when L6 is. User
//!    text, compiled as SPEC §7.1.1 says (checked), also states its `original` and its hit step's `effective` and
//!    `corrected` (L4's corrections: each word no document holds, replaced by the most frequent term within its
//!    distance over the fields). A plan without L1 and L2 runs too, so L3 and L4 meet L0's misses alone.
//!    L1 (ROADMAP 8.1.18): the collection has random synonym groups (SPEC §6.2.1), and user text compiles to the runs
//!    `tests/reference/synonyms.rs` finds (SPEC §6.3.2: the longest term at each word, in each default field, the
//!    alternatives in group then term order, each once, at most 50, compiled as quoted phrases, and at most
//!    `max_terms − 2 · n` in all when compiled at the case's `max_terms`), checked against `Compiled::synonyms`. Its
//!    step requires each run or one of its alternatives; over the limits each run keeps its first alternatives at the
//!    highest shared count that fits, then one more each in text order where the step still fits (`synonyms:cut`),
//!    or the step is skipped (`synonyms:skipped`); a compilation that left some out lists it cut. The reference's step
//!    matches every document L0 matches at every count of alternatives per run (a self-check; the engine's step, which
//!    no search can see, is checked for it in-crate, `relax::synonyms`). Each text with a run is also searched at a
//!    `max_terms` inside its cut window, a different one at each operation.
//!    The default fields analyze apart (ROADMAP 8.1.24): `title` by `whitespace`, `body` by `standard` ([`analyze`],
//!    checked against both analyzers on the generated alphabet). User text and synonym terms have capitals and `-`
//!    at times, and the pool other writings of its terms, so a term matches in one field only, two writings are one
//!    term in `body` alone, and a word is one token in `title` and several in `body` (compiled as SPEC §7.1.1 says
//!    for fields that split it apart, also through L2's importance, L3's last term and L4's corrections). The runs
//!    are each field's own, merged as SPEC §6.3.2 says: the union in group then term order (a later field's term
//!    before an earlier one's), the later field's overlapping match dropped, an alternative compiled away as a
//!    duplicate over the fields; each is covered. Neither analyzer removes a token, so stopword gaps stay with
//!    `tests/relax_synonyms.rs`. A run whose alternatives all compile away is unreachable through `compile`, for any
//!    schema and analyzer (ROADMAP 8.1.26), so the compiler's unit test checks that guard alone: (a) a run's own query
//!    is built of `combined` leaves (each of its words, as SPEC §7.1.1 compiles a word); (b) an alternative of several
//!    tokens in some default field compiles, per field, to a `term` (one token), a `phrase` or `term`s required, never
//!    to a `combined`, so it is not the run's query; (c) an alternative of at most one token per field compiles as a
//!    word, equal to the run's query only when its token is the run's in every field, and then every field's synonym
//!    map reads it as the run's own term, so no field reports it as an alternative.
//! 6. A sort led by the score with a later key on the `n` column (each order and `missing` pair, with the proximity
//!    bonus and without, in turn) ranks as the score order re-sorted, among exact score ties, by the reference's `n`
//!    values, then location: every strategy, pages walked by cursor across ties, and on the pool a page of `k` and
//!    the one after its cursor (ROADMAP 7.4.11; SPEC §7.4).
//!
//! Queries hold `prefix` and `fuzzy` leaves of few expansions (SPEC §7.1: the terms of highest collection `df`, ties
//! by bytes, one shared `idf`, a `fuzzy` term's edit-distance weight), checked by invariants 1–4 like any leaf. The
//! vocabulary ([`VOCAB`], [`TYPOS`]) makes completions and one- and two-edit neighbours common, and the test counts the
//! steps the reference cascade builds, so the property does not pass vacuously. The rarest kinds of step are built
//! on purpose in every case, not left to the seed (ROADMAP 8.1.27): one synonym writing of each shape ([`synonyms`]),
//! compiled alone and once searched ([`shaped_text`]); texts built from the live documents ([`built_texts`]); and each
//! `fuzzy` step that finds a hit, searched again at the bound where its expansions are cut. So the coverage asserts
//! hold on any `PROPTEST_RNG_SEED`.

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

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::ops::{Bound, Range};
use std::sync::Arc;

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
    FilterValue, FuzzyQuery, PhraseQuery, PhraseTerm, PrefixQuery, Query, QueryCompiler,
    QueryError, QueryLimits, TermQuery, WeightedTerm,
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
/// Words of queries alone, which no document holds: prefixes of the documents' words (`c` shorter than L3 expands
/// even last, ROADMAP 8.1.20, `ca` shorter than it expands other than last, `ele` not) and typos (`crat` a swap from
/// `cart`, `elehpant` a swap from `elephant`, `elepant` one edit from both `elephant` and `elegant`, `bees` and `carts`
/// an insertion).
const TYPOS: [&str; 8] = [
    "c", "ca", "ele", "crat", "carts", "bees", "elepant", "elehpant",
];
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

fn schema(p: &Params, synonyms: &[Vec<String>]) -> CollectionSchema {
    // `title` by `whitespace` and `body` by `standard` (ROADMAP 8.1.24): on the documents' lowercase words both give
    // the words as they are, at positions 0, 1, 2... (the reference's tokens); user text and synonym terms analyze
    // apart in them ([`analyze`]).
    let text = |analyzer: &str, (boost, k1, b)| {
        FieldKind::Text(TextOptions {
            analyzer: AnalyzerRef::new(analyzer, 1).unwrap(),
            boost,
            k1,
            b,
            positions: true,
            ..TextOptions::default()
        })
    };
    SchemaBuilder::new("c")
        .field("title", text("whitespace", p[0]))
        .field("body", text("standard", p[1]))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .field("n", FieldKind::I64 { stored: false })
        .synonyms(synonyms.to_vec())
        .build()
        .unwrap()
}

/// `title`, `body`, `tag` (the reference's field indices), then `n`.
fn ids(schema: &CollectionSchema) -> [FieldId; 4] {
    ["title", "body", "tag", "n"].map(|n| schema.field(n).unwrap().id())
}

/// The default fields of user text, `title` and `body`, in ascending field id (SPEC §7.1.1).
fn default_fields(fields: &[FieldId; 4]) -> [usize; 2] {
    let mut order = [TITLE, BODY];
    order.sort_by_key(|&f| fields[f]);
    order
}

/// A token of a field's analysis: its text, its byte span in the analyzed text, and its position.
type Analyzed = (String, Range<usize>, u32);

/// How a default field analyzes `text` at query time (SPEC §6.3.3, §6.3.4), on the alphabet the generators write
/// (letters, a `-` inside a word, spaces): `title` by `whitespace`, each run of other characters as written; `body` by
/// `standard`, whose UAX #29 words are there the runs of letters (`-` is a word break), NFKC (the letters are already
/// composed) and lowercased, so `Cat` and `cat` are one term in `body` alone, and `cat-bee` is two tokens there and
/// one in `title`. Positions 0, 1, 2... in order: neither analyzer removes a token, so neither leaves a gap.
fn analyze(field: usize, text: &str) -> Vec<Analyzed> {
    let breaks = |c: char| c.is_whitespace() || (field == BODY && c == '-');
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (start, breaks(c)) {
            (None, false) => start = Some(i),
            (Some(s), true) => {
                let word = text.get(s..i).unwrap();
                let term = if field == BODY {
                    word.to_lowercase()
                } else {
                    word.to_owned()
                };
                out.push((term, s..i, u32::try_from(out.len()).unwrap()));
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// [`analyze`] as `tests/reference/synonyms.rs` reads an analysis: the tokens and the positions they took.
fn tokens(field: usize, text: &str) -> (Vec<reference::synonyms::Token>, u32) {
    let tokens: Vec<_> = analyze(field, text)
        .into_iter()
        .map(|(t, _, p)| (t, p))
        .collect();
    let took = u32::try_from(tokens.len()).unwrap();
    (tokens, took)
}

/// A query matching any of `alternatives`: itself when there is one (SPEC §7.1.1: "one of the fields must match").
fn one_of(mut alternatives: Vec<RefQuery>) -> Option<RefQuery> {
    match alternatives.len() {
        0 => None,
        1 => alternatives.pop(),
        _ => Some(RefQuery::Bool {
            must: Vec::new(),
            should: alternatives,
            must_not: Vec::new(),
            filter: Vec::new(),
            minimum_should_match: 0,
        }),
    }
}

/// SPEC §7.1.1's parts of a word over the default fields `order`, each field at its schema boost: fields where it
/// analyzes to no token are left out. When the fields agree on how to split it (no two token spans overlap unless
/// equal), each span is one required `combined` over the fields that have it; otherwise each field requires its own
/// tokens (each a one-field `combined`, several in a `bool`), and one of the fields must match: one part.
fn compile_word(word: &str, order: &[usize; 2], params: &Params) -> Vec<RefQuery> {
    let per_field: Vec<(usize, Vec<Analyzed>)> = order
        .iter()
        .map(|&f| (f, analyze(f, word)))
        .filter(|(_, t)| !t.is_empty())
        .collect();
    let clause = |field: usize, term: &str| Clause {
        field,
        term: term.to_owned(),
        weight: params[field].0,
    };
    let mut spans: Vec<Range<usize>> = per_field
        .iter()
        .flat_map(|(_, t)| t.iter().map(|t| t.1.clone()))
        .collect();
    spans.sort_by_key(|s| (s.start, s.end));
    spans.dedup();
    if spans.windows(2).all(|w| w[0].end <= w[1].start) {
        return spans
            .iter()
            .map(|span| {
                RefQuery::Combined(
                    per_field
                        .iter()
                        .filter_map(|(f, t)| {
                            t.iter().find(|t| t.1 == *span).map(|t| clause(*f, &t.0))
                        })
                        .collect(),
                )
            })
            .collect();
    }
    let should = per_field
        .iter()
        .map(|(f, t)| {
            let mut must: Vec<RefQuery> = t
                .iter()
                .map(|t| RefQuery::Combined(vec![clause(*f, &t.0)]))
                .collect();
            if must.len() == 1 {
                return must.remove(0);
            }
            RefQuery::Bool {
                must,
                should: Vec::new(),
                must_not: Vec::new(),
                filter: Vec::new(),
                minimum_should_match: 0,
            }
        })
        .collect();
    one_of(should).into_iter().collect()
}

/// A synonym term compiled as the quoted phrase `"<term>"` would be (SPEC §6.3.2, §7.1.1): a word when it has at most
/// one token in every default field ([`compile_word`], its parts required together); else per field with a token a
/// `phrase` of its tokens at their relative positions (both fields have positions), a `term` where it has one, and
/// one of the fields must match. `None` when it has no token.
fn quoted(term: &str, order: &[usize; 2], params: &Params) -> Option<RefQuery> {
    let per_field: Vec<(usize, Vec<Analyzed>)> =
        order.iter().map(|&f| (f, analyze(f, term))).collect();
    if per_field.iter().all(|(_, t)| t.len() <= 1) {
        let mut parts = compile_word(term, order, params);
        // Several parts (fields that agree on a split) need an analyzer that splits like `standard` in both fields;
        // not reached with `whitespace` beside it.
        return match parts.len() {
            0 => None,
            1 => parts.pop(),
            _ => Some(RefQuery::Bool {
                must: parts,
                should: Vec::new(),
                must_not: Vec::new(),
                filter: Vec::new(),
                minimum_should_match: 0,
            }),
        };
    }
    let should = per_field
        .into_iter()
        .filter(|(_, t)| !t.is_empty())
        .map(|(field, t)| {
            let weight = params[field].0;
            if let [(term, _, _)] = t.as_slice() {
                return RefQuery::Word(vec![Clause {
                    field,
                    term: term.clone(),
                    weight,
                }]);
            }
            let first = t[0].2;
            RefQuery::Phrase {
                field,
                terms: t
                    .into_iter()
                    .map(|(term, _, p)| (term, p - first))
                    .collect(),
                weight,
            }
        })
        .collect();
    one_of(should)
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
        RefQuery::Word(clauses) | RefQuery::Combined(clauses) => Query::Combined(CombinedQuery {
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
        RefQuery::Fuzzy {
            field,
            term,
            distance,
            max_expansions,
            ..
        } => Query::Fuzzy(FuzzyQuery {
            field: fields[*field],
            term: term.clone(),
            distance: *distance,
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

/// A word of user text: typos as often as the documents' words, so L4 has words to correct (ROADMAP 8.1.21); at times
/// with capitals ([`cased`]), which `body` lowercases and `title` keeps, or two words joined by `-`, which `body`
/// splits and `title` keeps whole (ROADMAP 8.1.24).
fn text_word() -> impl Strategy<Value = String> {
    let plain = || {
        prop_oneof![
            2 => word(),
            2 => prop::sample::select(&TYPOS[..]).prop_map(str::to_owned),
            1 => query_word(),
        ]
    };
    let maybe_cased = move || {
        prop_oneof![
            3 => plain(),
            1 => (plain(), any::<bool>()).prop_map(|(w, all)| cased(&w, all)),
        ]
    };
    prop_oneof![
        8 => plain(),
        1 => (plain(), any::<bool>()).prop_map(|(w, all)| cased(&w, all)),
        1 => (maybe_cased(), maybe_cased()).prop_map(|(a, b)| format!("{a}-{b}")),
    ]
}

/// Another writing of a synonym term that `body` analyzes alike: `how` 0 its first letter a capital, 1 every letter,
/// 2 its words joined by `-`.
fn writing(term: &str, how: u8) -> String {
    match how {
        0 => cased(term, false),
        1 => cased(term, true),
        _ => term.split_whitespace().collect::<Vec<_>>().join("-"),
    }
}

/// `text` with its first letter a capital (`Cat bee`), or every letter (`CAT BEE`, `CAFÉ`).
fn cased(text: &str, all: bool) -> String {
    if all {
        return text.to_uppercase();
    }
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
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
        // A `fuzzy` of either distance (ROADMAP 8.1.21): the vocabulary holds words one and two edits apart.
        1 => (0..2_usize, query_word(), 1..=2_u8, max_expansions()).prop_map(move |(field, term, distance, max_expansions)| {
            RefQuery::Fuzzy { field, term, distance, max_expansions, weight: params[field].0 }
        }),
        // On the keyword field: `x`, `y` and `z` are one substitution apart, so its cap and ties decide.
        1 => (prop::sample::select(&TAGS[..]), max_expansions()).prop_map(|(t, max_expansions)| RefQuery::Fuzzy {
            field: TAG, term: t.to_owned(), distance: 1, max_expansions, weight: 1.0,
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
    /// The collection's synonym groups (SPEC §6.2.1), which L1 expands user text with (ROADMAP 8.1.18).
    synonyms: Vec<Vec<String>>,
    ops: Vec<Op>,
    /// Each with an optional filter, and whether the query is beside it as a `should` (optional: every selected
    /// document matches, scoring the query or 0) rather than as the `must`.
    queries: Vec<(RefQuery, Option<(Filter, bool)>)>,
    /// User text, compiled and relaxed (invariant 5): what a level states of a query from `q`.
    texts: Texts,
    /// The `max_expansions` limit the relaxed steps are held to (SPEC §7.1.3).
    max_expansions: usize,
    /// The `max_terms` limit the relaxed steps are held to (SPEC §7.1.3), which cuts L1's alternatives; the other
    /// limits the defaults'.
    max_terms: usize,
    /// Whether user text is compiled at `max_terms` too (when its query fits it), so the compilation bounds the
    /// alternatives (SPEC §6.3.2), rather than at the defaults.
    compile_at_limit: bool,
    k: usize,
}

/// A case's user text (invariant 5).
#[derive(Debug, Clone)]
struct Texts {
    /// Searched after every operation.
    random: Vec<Text>,
    /// Written as terms the synonym groups were shaped for ([`shaped_text`]): searched after the case's last operation
    /// only, with [`built_texts`] (ROADMAP 8.1.27).
    shaped: Option<Text>,
    /// Each term a group was shaped for, as its own text: compiled after the case's last operation, its runs checked
    /// against the reference's (SPEC §6.3.2), and not searched, so every shape meets the compiler in every case.
    shapes: Vec<Text>,
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

    /// Each clause's parts (SPEC §7.1.1, [`compile_word`]): the entries of the root's `must`, or `must_not` when it
    /// is excluded.
    fn parts(&self, params: &Params, fields: &[FieldId; 4]) -> Vec<Vec<RefQuery>> {
        let order = default_fields(fields);
        self.clauses
            .iter()
            .map(|c| compile_word(&c.0, &order, params))
            .collect()
    }

    /// Whether the query is one required part and nothing beside it, so that part is the query (SPEC §7.1.1).
    fn single(&self, params: &Params, fields: &[FieldId; 4]) -> bool {
        let parts = self.parts(params, fields);
        self.filter.is_none()
            && self.clauses.iter().all(|c| !c.1)
            && parts.iter().map(Vec::len).sum::<usize>() == 1
    }

    /// SPEC §7.1.1 for this text: each word's parts ([`compile_word`]) required; the excluded ones the root's
    /// `must_not`; one required part alone is the query, unless a filter joins it.
    fn root(&self, params: &Params, fields: &[FieldId; 4]) -> RefQuery {
        let parts = self.parts(params, fields);
        let mut must: Vec<RefQuery> = Vec::new();
        let mut must_not: Vec<RefQuery> = Vec::new();
        for (c, parts) in self.clauses.iter().zip(parts) {
            if c.1 {
                must_not.extend(parts);
            } else {
                must.extend(parts);
            }
        }
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

/// User text drawn from the synonym terms (`pool`, each split into its words, or at times at its `-` too, so the words
/// of a term `body` splits are clauses of their own) and from [`text_word`], so runs of one and several words are
/// common: at most 6 words (a term past the sixth is cut short, on purpose: a partial run), each excluded at times
/// (which ends a run, SPEC §6.3.2).
fn text(fields: [FieldId; 4], pool: Vec<String>) -> impl Strategy<Value = Text> {
    let split = |t: String, at: fn(char) -> bool| {
        t.split(at)
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    // Another writing of a term, which `body` analyzes as the term and `title` may not: lowercase, capitals, its
    // words split at `-` too, or joined by `-`; or the term with `w` joined to its last word or before its first, so
    // in `body` the term ends or starts inside a word, and must not hide a shorter term that ends or starts with one.
    let other = move |t: String, how: u8, w: String| {
        let mut words = split(t.clone(), char::is_whitespace);
        match how {
            0 => split(t.to_lowercase(), char::is_whitespace),
            1 => split(cased(&t, true), char::is_whitespace),
            2 => split(t, |c| c.is_whitespace() || c == '-'),
            3 => vec![words.join("-")],
            4 => {
                if let Some(last) = words.last_mut() {
                    *last = format!("{last}-{w}");
                }
                words
            }
            _ => {
                if let Some(first) = words.first_mut() {
                    *first = format!("{w}-{first}");
                }
                words
            }
        }
    };
    let chunk = prop_oneof![
        4 => prop::sample::select(pool.clone()).prop_map(move |t| split(t, char::is_whitespace)),
        2 => (prop::sample::select(pool), 0..6_u8, word()).prop_map(move |(t, how, w)| other(t, how, w)),
        3 => text_word().prop_map(|w| vec![w]),
    ];
    (
        prop::collection::vec(chunk, 1..4),
        prop::collection::vec(prop::bool::weighted(0.2), 6),
        prop::option::weighted(0.3, filter(fields)),
    )
        .prop_map(|(chunks, excluded, filter)| Text {
            clauses: chunks.into_iter().flatten().zip(excluded).collect(),
            filter,
        })
}

/// A collection's synonym groups (SPEC §6.2.1) and the terms they hold, which user text draws from: groups of 2 to
/// 4 terms picked from a few terms of 1 to 3 words, so a term is often in several groups and a longer term often
/// starts with a shorter one; at times two spaces between words (equal after analysis to one, so a group may shrink
/// to one term); and at times a word with more alternatives than the 50 a match reports (ROADMAP 8.1.18). The terms'
/// words come from [`text_word`], so they have capitals and `-` at times, and the pool also holds other writings of
/// some of its terms, with capitals or with `-` for spaces, which `body` analyzes alike and `title` does not, so a
/// group may shrink to one term in `body` alone, and a field may match where the other does not (ROADMAP 8.1.24).
/// Every case has one writing of each shape (ROADMAP 8.1.27), and also returns the term each shape is for, so text
/// written as it meets the shape whatever the seed draws.
fn synonyms() -> impl Strategy<Value = (Vec<String>, Vec<Vec<String>>, Vec<String>)> {
    let term = (
        prop_oneof![3 => 1..2_usize, 2 => 2..4_usize],
        prop::collection::vec(text_word(), 3),
        prop::bool::weighted(0.1),
    )
        .prop_map(|(n, words, wide)| words[..n].join(if wide { "  " } else { " " }));
    let pairs: Vec<String> = VOCAB
        .iter()
        .flat_map(|a| VOCAB.iter().map(move |b| format!("{a} {b}")))
        .collect();
    let many = (word(), prop::sample::subsequence(pairs, 52..64));
    (
        prop::collection::vec(term, 2..7),
        prop::collection::vec(
            prop::collection::vec(any::<prop::sample::Index>(), 2..5),
            0..5,
        ),
        prop::option::weighted(0.25, many),
        // One writing of each kind that builds a shape (1 to 5, ROADMAP 8.1.27), then up to 3 more of any kind.
        (
            prop::collection::vec(
                (prop::array::uniform3(any::<prop::sample::Index>()), 0..3_u8),
                5,
            ),
            prop::collection::vec(
                (
                    prop::array::uniform3(any::<prop::sample::Index>()),
                    0..3_u8,
                    0..6_u8,
                ),
                0..4,
            ),
        )
            .prop_map(|(built, more)| {
                let built = built
                    .into_iter()
                    .zip(1..=5_u8)
                    .map(|((t, how), kind)| (t, how, kind));
                built.chain(more).collect::<Vec<_>>()
            }),
    )
        .prop_map(|(mut pool, picks, many, writings)| {
            // Other writings of some terms (ROADMAP 8.1.24), each alone or in two groups with other terms `x` and
            // `y`: the writing with `x`, then the term (or its first word) with `y` (`x` again at times). For text
            // written as the term, `body` (the first default field) reads both groups and `title` the second only,
            // so `body` reports `x` beside the shared `y`, compiles `x` again when `y` is `x` (a duplicate over the
            // fields), or matches the longer term over clauses `title` matches part of (an overlap). Kind 4 also puts
            // the writing and the term in one group before those, a term of its own in `title` and one term in `body`
            // (which never matches it), so `title` reports a place before `body`'s: a union of both fields with the
            // later field's term first. Kind 5 puts them in that one group alone: a run `title` alone matches.
            let mut groups: Vec<Vec<String>> = Vec::new();
            // The term each writing's shape is for, in writing order (ROADMAP 8.1.27): user text written as it meets the shape.
            let mut shaped: Vec<String> = Vec::new();
            for ([t, x, y], how, kind) in writings {
                let (mut t, x, y) = (
                    t.get(&pool).clone(),
                    x.get(&pool).clone(),
                    y.get(&pool).clone(),
                );
                // For an overlap, a term of two words or more, whose first word is the second group's term.
                if kind == 3 && !t.contains(char::is_whitespace) {
                    t = format!("{t} {y}");
                }
                // For a run `title` alone matches, a term of no other group (ROADMAP 8.1.27): its words and `y`'s first, joined by
                // `-`, one clause no other group's match in `body` covers whole, so no `body` match overlaps it.
                if kind == 5 {
                    let words = t.split_whitespace().chain(y.split_whitespace().take(1));
                    t = words.collect::<Vec<_>>().join("-");
                }
                let Some(other) = (0..3).map(|k| writing(&t, (how + k) % 3)).find(|o| *o != t)
                else {
                    continue;
                };
                for term in [&t, &other] {
                    if !pool.contains(term) {
                        pool.push(term.clone());
                    }
                }
                let first = t.split_whitespace().next().unwrap_or_default().to_owned();
                if (1..=5).contains(&kind) {
                    shaped.push(t.clone());
                }
                let pair = match kind {
                    1 => Some((t, y)),
                    2 => Some((t, x.clone())),
                    3 => Some((first, y)),
                    4 => {
                        groups.push(vec![t.clone(), other.clone()]);
                        Some((t, y))
                    }
                    5 => {
                        groups.push(vec![t.clone(), other.clone()]);
                        None
                    }
                    _ => None,
                };
                if let Some((term, with)) = pair {
                    for group in [[other.clone(), x.clone()], [term, with]] {
                        if group[0] != group[1] {
                            groups.push(group.to_vec());
                        }
                    }
                }
            }
            for pick in picks {
                let mut group: Vec<String> = Vec::new();
                for i in pick {
                    let t = i.get(&pool);
                    if !group.contains(t) {
                        group.push(t.clone());
                    }
                }
                if group.len() >= 2 {
                    groups.push(group);
                }
            }
            if let Some((w, pairs)) = many {
                groups.push(std::iter::once(w.clone()).chain(pairs).collect());
                pool.push(w);
            }
            (pool, groups, shaped)
        })
}

/// `term` as user text: its words, none excluded, no filter.
fn words_of(term: &str) -> Text {
    Text {
        clauses: term
            .split_whitespace()
            .map(|w| (w.to_owned(), false))
            .collect(),
        filter: None,
    }
}

/// User text written as the terms the synonym groups were shaped for ([`synonyms`], ROADMAP 8.1.27): one term, or two in
/// a row (two runs, or one over both), so each shape meets text on every seed, not only where [`text`] happens to
/// draw its term. `None` when no writing built a shape.
fn shaped_text(shaped: Vec<String>) -> impl Strategy<Value = Option<Text>> {
    let words = |t: &String| {
        t.split_whitespace()
            .map(|w| (w.to_owned(), false))
            .collect::<Vec<_>>()
    };
    let n = shaped.len();
    (0..n.max(1), prop::option::of(0..n.max(1))).prop_map(move |(a, b)| {
        let first = shaped.get(a)?;
        let mut clauses = words(first);
        if let Some(second) = b.and_then(|b| shaped.get(b)) {
            clauses.extend(words(second));
        }
        Some(Text {
            clauses,
            filter: None,
        })
    })
}

fn case() -> impl Strategy<Value = Case> {
    let fields = ids(&schema(&[(1.0, 1.2, 0.75); 2], &[]));
    (params(), synonyms()).prop_flat_map(move |(p, (pool, synonyms, shaped))| {
        (
            Just(p),
            Just(synonyms),
            prop::collection::vec(op(), 1..20),
            prop::collection::vec(
                (
                    query(p, fields),
                    prop::option::weighted(0.5, (filter(fields), any::<bool>())),
                ),
                1..4,
            ),
            (
                prop::collection::vec(text(fields, pool), 1..4),
                shaped_text(shaped.clone()),
            )
                .prop_map(move |(random, shaped_text)| Texts {
                    random,
                    shaped: shaped_text,
                    shapes: shaped.iter().map(|t| words_of(t)).collect(),
                }),
            // The default, which the steps' 50-term expansions reach late in a cascade, or a limit that cuts them
            // and skips steps sooner (ROADMAP 8.1.19).
            prop_oneof![2 => Just(QueryLimits::default().max_expansions), 1 => 1..12_usize, 1 => 40..240_usize],
            // The default, or a limit that cuts L1's alternatives, or skips L1 and the steps that exclude it
            // (ROADMAP 8.1.18).
            prop_oneof![3 => Just(QueryLimits::default().max_terms), 1 => 8..60_usize, 1 => 60..300_usize],
            // Compiled at the case's `max_terms` most of the time, so the compilation's bound cuts runs (ROADMAP 8.1.24).
            prop::bool::weighted(0.7),
            1..6_usize,
        )
            .prop_map(
                |(
                    params,
                    synonyms,
                    ops,
                    queries,
                    texts,
                    max_expansions,
                    max_terms,
                    compile_at_limit,
                    k,
                )| Case {
                    params,
                    synonyms,
                    ops,
                    queries,
                    texts,
                    max_expansions,
                    max_terms,
                    compile_at_limit,
                    k,
                },
            )
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

/// Invariants 1 to 5 after an operation; `last` after the case's last one, which also searches the texts built from the
/// live documents ([`built_texts`]).
fn check(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    model: &Model,
    (case, last): (&Case, bool),
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
            max_terms: case.max_terms,
            runs: &[],
        };
        check_relaxed(
            &views,
            Engine::Query(&query),
            &reference,
            (&root, None),
            Some(&no_bonus),
            (case, false),
        )?;
    }
    // Invariant 5 for user text: compiled as SPEC §7.1.1 says (checked against the reference's own form), then
    // relaxed, each step's `effective` the reference's.
    let registry = AnalyzerRegistry::builtin();
    let text_compiler = QueryCompiler::new(schema, &registry).unwrap();
    let defaults = DefaultFields::all(schema);
    // After the last operation, the texts built on purpose (ROADMAP 8.1.27): searched, then compiled alone (`false`).
    let (built, shapes): (Vec<Text>, &[Text]) = if last {
        let shaped = case.texts.shaped.iter().cloned();
        (
            shaped.chain(built_texts(&live)).collect(),
            &case.texts.shapes,
        )
    } else {
        (Vec::new(), &[])
    };
    let texts = case.texts.random.iter().chain(&built).map(|t| (t, true));
    for (t, (text, searched)) in texts.chain(shapes.iter().map(|t| (t, false))).enumerate() {
        // Compiled at the case's `max_terms` when its query fits it (SPEC §6.3.2: the compilation then bounds the
        // alternatives), else at the defaults.
        let unfiltered = Text {
            filter: None,
            ..text.clone()
        }
        .root(&case.params, &fields);
        let compile_limits = if case.compile_at_limit && terms(&unfiltered) <= case.max_terms {
            QueryLimits {
                max_terms: case.max_terms,
                ..QueryLimits::default()
            }
        } else {
            QueryLimits::default()
        };
        let mut compiled = text_compiler.compile(&text.render(), &defaults, &compile_limits);
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
        // The runs and their alternatives, as SPEC §6.3.2 finds them (checked).
        let n = terms(&unfiltered);
        let same_runs = |compiled: &Compiled, runs: &[RefRun]| {
            let found: Vec<(Range<usize>, Vec<Query>, bool)> = compiled
                .synonyms
                .iter()
                .map(|s| (s.clauses.clone(), s.alternatives.clone(), s.cut))
                .collect();
            let want: Vec<(Range<usize>, Vec<Query>, bool)> = runs
                .iter()
                .map(|r| {
                    let alternatives = r.alternatives.iter().map(|a| engine_query(a, &fields));
                    (r.clauses.clone(), alternatives.collect(), r.cut)
                })
                .collect();
            prop_assert_eq!(found, want, "{} over {:?}", text.render(), case.synonyms);
            Ok(())
        };
        let runs_at =
            |max_terms| reference_runs(text, &case.synonyms, &case.params, &fields, (max_terms, n));
        let runs = runs_at(compile_limits.max_terms);
        same_runs(&compiled, &runs)?;
        // The compilation's bound where it binds (ROADMAP 8.1.24): one alternative fewer in all than the runs keep at
        // the defaults, when some run keeps more than one. The shares count the terms reported, so one that compiles
        // away may leave every run uncut even then.
        let at_defaults = runs_at(QueryLimits::default().max_terms);
        let kept: usize = at_defaults.iter().map(|r| r.alternatives.len()).sum();
        let mut bound_compiled: Option<(Compiled, Vec<RefRun>)> = None;
        if kept > at_defaults.len() {
            let max_terms = 2 * n + kept - 1;
            let limits = QueryLimits {
                max_terms,
                ..QueryLimits::default()
            };
            let bound = runs_at(max_terms);
            let mut at_bound = text_compiler.compile(&text.render(), &defaults, &limits);
            // The same query: the bound limits the alternatives, not the text (at least `n` terms fit).
            prop_assert_eq!(at_bound.query(), &engine_query(&unfiltered, &fields));
            same_runs(&at_bound, &bound)?;
            if bound.iter().any(|r| r.cut) {
                cover("synonyms compiled cut".to_owned());
                if let Some(f) = &text.filter {
                    at_bound = at_bound.with_filter([f.clone()]);
                }
                bound_compiled = Some((at_bound, bound));
            }
        }
        for run in &runs {
            if run.cut {
                cover("synonyms compiled cut".to_owned());
            }
            if run.alternatives.len() == reference::synonyms::CAP {
                cover("synonyms at the cap".to_owned());
            }
            if run.clauses.len() > 1 {
                cover("synonyms run of words".to_owned());
            }
        }
        if !searched {
            continue;
        }
        let reference = Reference {
            schema,
            corpus: &corpus,
            params: &case.params,
            live: &live,
            eval: &|q, id| eval(q, id, false),
            max_expansions: case.max_expansions,
            max_terms: case.max_terms,
            runs: &runs,
        };
        check_relaxed(
            &views,
            Engine::Compiled(&compiled),
            &reference,
            (&root, Some(text)),
            None,
            (case, false),
        )?;
        // L4 at the bound where it binds (ROADMAP 8.1.27), every fourth text and operation: a `fuzzy` step that found a hit
        // uncut (at the largest `min_results`, which reaches it most) is searched again at one expansion fewer than it
        // needs beside the steps it excludes, so it is cut and its hits checked (`fuzzy:cut hit`), however rarely the
        // case's own limit falls there. Every earlier step needs no more than the expansions it excludes, so it stays as it
        // was.
        let levels = [
            RelaxLevel::Synonyms,
            RelaxLevel::DropTerms,
            RelaxLevel::Prefix,
            RelaxLevel::Fuzzy,
        ];
        let steps = if (live.len() + t).is_multiple_of(4) {
            reference.cascade(&root, Some(text), &levels, 10_000, true)
        } else {
            Vec::new()
        };
        let found = |s: &&RefStep| s.matches.as_ref().is_some_and(|m| !m.is_empty());
        if let Some(step) = steps.iter().find(|s| s.applied == "fuzzy" && found(s)) {
            let tight = Case {
                max_expansions: step.needs - 1,
                ..case.clone()
            };
            let reference = Reference {
                max_expansions: tight.max_expansions,
                ..reference
            };
            check_relaxed(
                &views,
                Engine::Compiled(&compiled),
                &reference,
                (&root, Some(text)),
                None,
                (&tight, false),
            )?;
        }
        // L1's cut at a `max_terms` within its window (SPEC §7.1.3): from one term below L1's step with one
        // alternative per run beside L0 (skipped) to that step with every alternative the compilation kept (as it is,
        // listed cut when the compilation cut it), a different one at each operation (`t` and the live count), so the
        // cases meet every cut of their runs. A text whose compilation did not cut its runs takes, every other
        // operation, the compilation at its bound instead, and a compilation that cut its runs is searched at the
        // window's top every other time it comes, so the step it keeps whole is met listed cut (ROADMAP 8.1.24: those
        // compilations are rarer than the window is wide). The rest of `turn` walks the window's offsets.
        let mut turn = live.len() + t;
        let (compiled, runs) = match &bound_compiled {
            Some((c, r)) if !runs.iter().any(|r| r.cut) && !turn.is_multiple_of(2) => {
                (c, r.as_slice())
            }
            _ => (&compiled, runs.as_slice()),
        };
        if bound_compiled.is_some() {
            turn /= 2;
        }
        if !runs.is_empty() {
            let excluded = [(RelaxLevel::Exact, root.clone())];
            let at = |counts: &[usize]| {
                terms(&Reference::whole(
                    reference.synonyms_step(&root, text, counts),
                    &excluded,
                ))
            };
            let lens: Vec<usize> = runs.iter().map(|r| r.alternatives.len()).collect();
            let (lo, hi) = (at(&vec![1; runs.len()]), at(&lens));
            let max_terms = if !runs.iter().any(|r| r.cut) {
                lo - 1 + turn % (hi - lo + 2)
            } else if turn.is_multiple_of(2) {
                hi
            } else {
                lo - 1 + (turn / 2) % (hi - lo + 2)
            };
            cover("synonyms window".to_owned());
            let tight = Case {
                max_terms,
                ..case.clone()
            };
            let reference = Reference {
                max_terms,
                runs,
                ..reference
            };
            check_relaxed(
                &views,
                Engine::Compiled(compiled),
                &reference,
                (&root, Some(text)),
                None,
                (&tight, true),
            )?;
        }
    }
    Ok(())
}

/// User text built from the live documents (ROADMAP 8.1.27), so the steps that need a document of one shape meet one
/// whatever the seed, from the first live document that has the words: `<word> <prefix>`, a title or body word as it
/// is and the first 2 characters of another word of the same field (no document holds a 2-character word), which
/// L3's tier step `prefix:last` finds the document by when L2 does not run before it; `<word>-<prefix>` of two body
/// words, one clause that `body` splits in two and `title` keeps whole, a last word the fields split apart that L3
/// completes; and `<typo>-<typo>`, two body words of 4 characters or more with their second character a `q` (in no
/// word), split alike, which only L4 finds the document by, and does not correct (a word of several tokens). Searched
/// after the case's last operation only.
fn built_texts(live: &BTreeMap<String, Values>) -> Vec<Text> {
    let text = |words: Vec<String>| Text {
        clauses: words.into_iter().map(|w| (w, false)).collect(),
        filter: None,
    };
    let typo = |w: &String| {
        let mut chars: Vec<char> = w.chars().collect();
        chars[1] = 'q';
        chars.into_iter().collect::<String>()
    };
    let mut out = Vec::new();
    let prefixed = live.values().find_map(|v| {
        [TITLE, BODY].into_iter().find_map(|f| {
            let words = &v.doc[f];
            let (a, b) = (words.first()?, words.get(1)?);
            Some(text(vec![a.clone(), b.chars().take(2).collect()]))
        })
    });
    out.extend(prefixed);
    let split_prefix = live.values().find_map(|v| {
        let words = &v.doc[BODY];
        let (a, b) = (words.first()?, words.get(1)?);
        Some(text(vec![format!(
            "{a}-{}",
            b.chars().take(2).collect::<String>()
        )]))
    });
    out.extend(split_prefix);
    let split = live.values().find_map(|v| {
        let long: Vec<&String> = v.doc[BODY]
            .iter()
            .filter(|w| w.chars().count() >= 4)
            .collect();
        let (a, b) = (long.first()?, long.get(1)?);
        Some(text(vec![format!("{}-{}", typo(a), typo(b))]))
    });
    out.extend(split);
    out
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
    let schema = schema(&case.params, &case.synonyms);
    let fields = ids(&schema);
    let mut db = database(schema);
    let mut model = Model::default();
    for (i, op) in case.ops.iter().enumerate() {
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
        check(c, &memtable, &model, (case, i + 1 == case.ops.len()))?;
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
/// each kind of step listed is built, and finds hits, in some case (ROADMAP 8.1.19), on any seed (ROADMAP 8.1.27).
#[test]
fn collection_searches_match_the_reference() {
    let cases = if cfg!(feature = "slow") { 512 } else { 48 };
    run(
        "collection_searches_match_the_reference",
        cases,
        case(),
        |case| run_case(&case),
    );
    let coverage = COVERAGE.with_borrow(Clone::clone);
    eprintln!("steps the reference cascade built: {coverage:#?}");
    if std::env::var("PROPTEST_CASES").is_err() {
        for key in [
            "synonyms hit",
            "synonyms:cut hit",
            "synonyms:skipped",
            "synonyms compiled cut",
            "synonyms compiled cut that fits",
            "synonyms at the cap",
            "synonyms run of words",
            "synonyms window",
            "synonyms cut one more",
            // Default fields that analyze apart (ROADMAP 8.1.24).
            "synonyms match in the first field only",
            "synonyms match in the later field only",
            "synonyms union of both fields",
            "synonyms later field's term first",
            "synonyms overlap dropped",
            "synonyms duplicate across fields",
            "synonyms alike in one field only",
            "synonyms run over a word of several tokens",
            "drop_terms hit step over a word the fields split apart",
            "prefix hit over a last word the fields split apart",
            "fuzzy hit step beside a split word not corrected",
            "drop_terms:75% hit",
            "prefix:last hit",
            "prefix hit",
            "prefix:cut",
            "prefix:last:cut",
            "prefix:skipped",
            "prefix last term of one character kept",
            "fuzzy:1 hit",
            "fuzzy hit",
            "fuzzy:cut hit",
            "fuzzy:skipped",
            "corrected",
            "fallback hit",
            "filters hit",
        ] {
            assert!(coverage.get(key).is_some_and(|&n| n > 0), "no {key} step");
        }
    }
}

/// The reference's analysis ([`analyze`]) is `whitespace` and `standard`, written from SPEC §6.3.3 for the alphabet
/// the generators write (ROADMAP 8.1.24): a self-check of that premise on the words and synonym terms they write, and
/// on some by hand, against the built-in analyzers. It checks the suite's model, not the analyzers, which their own
/// tests pin.
#[test]
fn the_reference_analysis_is_the_analyzers_on_the_generated_alphabet() {
    let analyzers = [(TITLE, "whitespace"), (BODY, "standard")]
        .map(|(f, name)| (f, persia_analysis::builtin_analyzer(name, 1).unwrap()));
    let check = |text: &str| -> Result<(), TestCaseError> {
        for (field, analyzer) in &analyzers {
            let mut out = persia_analysis::TokenStream::new();
            analyzer.analyze_query(text, &mut out);
            let engine: Vec<Analyzed> = out
                .into_vec()
                .into_iter()
                .map(|t| (t.text, t.start..t.end, t.position))
                .collect();
            prop_assert_eq!(
                &analyze(*field, text),
                &engine,
                "{:?} in field {}",
                text,
                field
            );
        }
        Ok(())
    };
    for text in [
        "Cat-bee",
        "CAFÉ café",
        "cat  bee",
        "elephant-Cats-ca",
        "Café-bee cats",
    ] {
        check(text).unwrap();
    }
    let terms = synonyms().prop_map(|(pool, groups, _)| (pool, groups.concat()));
    run(
        "the_reference_analysis_is_the_analyzers_on_the_generated_alphabet",
        64,
        (prop::collection::vec(text_word(), 1..6), terms),
        |(words, (pool, terms))| {
            for text in words.iter().chain(&pool).chain(&terms) {
                check(text)?;
            }
            check(&words.join(" "))
        },
    );
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
    let schema = schema(&params, &[]);
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
    let schema = schema(&params, &[]);
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

/// A document of `title` and `body` words.
fn titled(title: &[&str], body: &[&str]) -> Values {
    let words = |ws: &[&str]| ws.iter().map(|&w| w.to_owned()).collect();
    Values {
        doc: vec![words(title), words(body), Vec::new()],
        n: None,
    }
}

/// A query a hand test relaxes: user text, or a programmatic query.
enum Input<'a> {
    Text(&'a str),
    Query(RefQuery),
}

/// `input` relaxed with the collection's levels over the collection `ops` leave, its steps held to `max_expansions`,
/// after every operation of the case is checked against the reference (invariants 1–5).
fn search_input(ops: &[Op], input: &Input<'_>, max_expansions: usize) -> RelaxedResults {
    let (queries, texts) = match input {
        Input::Text(text) => {
            let clauses = text.split(' ').map(|w| (w.to_owned(), false)).collect();
            (
                Vec::new(),
                vec![Text {
                    clauses,
                    filter: None,
                }],
            )
        }
        Input::Query(q) => (vec![(q.clone(), None)], Vec::new()),
    };
    let case = Case {
        params: [(1.0, 1.2, 0.75); 2],
        synonyms: Vec::new(),
        ops: ops.to_vec(),
        queries,
        texts: Texts {
            random: texts,
            shaped: None,
            shapes: Vec::new(),
        },
        max_expansions,
        max_terms: QueryLimits::default().max_terms,
        compile_at_limit: false,
        k: 2,
    };
    run_case(&case).unwrap();
    let schema = schema(&case.params, &case.synonyms);
    let fields = ids(&schema);
    let mut db = database(schema.clone());
    let mut model = Model::default();
    for op in ops {
        apply(&mut db, &mut model, &fields, op);
    }
    let relax = RelaxPlan::new(schema.relaxation(), &RelaxOptions::default())
        .unwrap()
        .with_limits(QueryLimits {
            max_expansions,
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
    match input {
        Input::Text(text) => {
            let registry = AnalyzerRegistry::builtin();
            let compiled = QueryCompiler::new(&schema, &registry).unwrap().compile(
                text,
                &DefaultFields::all(&schema),
                &QueryLimits::default(),
            );
            views.search_relaxed(&compiled, &request, &relax, &exec)
        }
        Input::Query(q) => {
            let query = engine_query(q, &fields);
            views.search_relaxed(&query, &request, &relax, &exec)
        }
    }
    .unwrap()
}

fn search_text(ops: &[Op], text: &str, max_expansions: usize) -> RelaxedResults {
    search_input(ops, &Input::Text(text), max_expansions)
}

fn hit_ids(out: &RelaxedResults) -> Vec<&str> {
    let mut hits: Vec<&str> = out.hits.iter().map(|h| h.hit.document.id()).collect();
    hits.sort_unstable();
    hits
}

/// SPEC §7.1.3 by hand (ROADMAP 8.1.19): L3's step for `ca` over `body` and `title` holds two `prefix`es of 50, and a
/// limit of 3 expansions cuts them to 1 each, the one left to the first in query order (`body`, the lower field id):
/// `body` keeps `cat` and `cart`, `title` its most frequent, `cafe`, not `card`.
#[test]
fn a_cut_gives_the_expansions_left_in_query_order() {
    let fields = ids(&schema(&[(1.0, 1.2, 0.75); 2], &[]));
    assert!(fields[BODY] < fields[TITLE], "body is first in query order");
    let ops = [
        Op::Put(0, titled(&[], &["cat"])),
        Op::Put(1, titled(&[], &["cat"])),
        Op::Put(2, titled(&[], &["cart"])),
        Op::Put(3, titled(&["cafe"], &[])),
        Op::Put(4, titled(&["cafe"], &[])),
        Op::Put(5, titled(&["card"], &[])),
        Op::Commit,
    ];
    let out = search_text(&ops, "ca", 3);
    assert_eq!(out.relaxation.applied, ["prefix:cut"]);
    assert_eq!(hit_ids(&out), ["d0", "d1", "d2", "d3", "d4"]);
}

/// SPEC §7.1.3 and §7.6 by hand (ROADMAP 8.1.19): a cut L2 rung stands for no earlier rung, so both stay excluded and
/// count in full. `bool { must: [prefix(ca, 50), cat, bee] }` under a limit of 120: L2's 75% rung (count 2) fits
/// beside L0 (50 + 50), its 50% rung does not beside L0 and the 75% rung, and is cut to the 20 left
/// (`drop_terms:50%:cut`). L3 then counts L0, the 75% rung and the cut rung (120): its step is skipped, where it would be
/// cut had the cut rung replaced the 75% one. Its tier step is not built: L2's last rung was built and searched. L5
/// finds the one document.
#[test]
fn a_cut_rung_leaves_the_earlier_rungs_excluded() {
    let word = |w: &str| {
        RefQuery::Word(vec![Clause {
            field: BODY,
            term: w.to_owned(),
            weight: 1.0,
        }])
    };
    let root = RefQuery::Bool {
        must: vec![
            RefQuery::Prefix {
                field: BODY,
                prefix: "ca".to_owned(),
                max_expansions: 50,
                weight: 1.0,
            },
            word("cat"),
            word("bee"),
        ],
        should: Vec::new(),
        must_not: Vec::new(),
        filter: Vec::new(),
        minimum_should_match: 0,
    };
    let ops = [Op::Put(0, titled(&[], &["elephant"]))];
    let out = search_input(&ops, &Input::Query(root), 120);
    assert_eq!(
        out.relaxation.applied,
        [
            "drop_terms:75%",
            "drop_terms:50%:cut",
            "prefix:skipped",
            "fallback"
        ]
    );
    assert_eq!(hit_ids(&out), ["d0"]);
}

/// SPEC §7.6 by hand (ROADMAP 8.1.21): a word no document holds is corrected to the term of highest `df` within its
/// distance over the default fields, ties to the field first in the `combined` (`body`), and `effective` reads it.
/// `crat` is one swap from `cart` and one deletion from `cat`; `elehpant`, of 8 characters, is one swap from
/// `elephant`, so the tier step `fuzzy:1` corrects it. L3 comes first and finds nothing.
#[test]
fn a_correction_is_the_highest_df_over_the_fields_ties_to_the_first() {
    let fields = ids(&schema(&[(1.0, 1.2, 0.75); 2], &[]));
    assert!(fields[BODY] < fields[TITLE], "body is first in query order");
    let correction = |out: &RelaxedResults| -> Vec<(String, String)> {
        let c = &out.relaxation.corrected;
        c.iter()
            .map(|c| (c.original.clone(), c.corrected.clone()))
            .collect()
    };
    let pair = |a: &str, b: &str| vec![(a.to_owned(), b.to_owned())];
    // `title` `cart` 2 over `body` `cat` 1.
    let ops = [
        Op::Put(0, titled(&["cart"], &[])),
        Op::Put(1, titled(&["cart"], &[])),
        Op::Put(2, titled(&[], &["cat"])),
    ];
    let out = search_text(&ops, "crat", 1024);
    assert_eq!(out.relaxation.applied, ["prefix", "fuzzy"]);
    assert_eq!(correction(&out), pair("crat", "cart"));
    assert_eq!(out.relaxation.effective.as_deref(), Some("cart"));
    // 1 each: the field first in query order, `body`.
    let out = search_text(&ops[1..], "crat", 1024);
    assert_eq!(correction(&out), pair("crat", "cat"));
    let ops = [
        Op::Put(0, titled(&["elephant"], &[])),
        Op::Put(1, titled(&["elegant"], &["elegant"])),
    ];
    let out = search_text(&ops, "elehpant", 1024);
    // The tier step finds `d0`, enough: `elegant`, two edits away, is not searched.
    assert_eq!(out.relaxation.applied, ["prefix", "fuzzy:1"]);
    assert_eq!(correction(&out), pair("elehpant", "elephant"));
    assert_eq!(hit_ids(&out), ["d0"]);
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

thread_local! {
    /// How often the reference cascade built each kind of step, over a test's cases (ROADMAP 8.1.19, 8.1.21): its
    /// `applied` (`:cut` and `:skipped` included), and `"<applied> hit"` when it found a hit. The property must not pass
    /// because the levels it checks never run. Per thread, as proptest runs a test's cases on its own thread: another test
    /// of the binary, run beside it by `cargo test`, does not count.
    static COVERAGE: RefCell<BTreeMap<String, usize>> = const { RefCell::new(BTreeMap::new()) };
}

fn cover(key: String) {
    COVERAGE.with_borrow_mut(|c| *c.entry(key).or_default() += 1);
}

/// Invariant 5: relaxed with the collection's levels (ROADMAP 8.1.19), held to the case's `max_expansions` and
/// `max_terms` (SPEC §7.1.3), a search returns what the reference cascade ([`Reference::cascade`]) returns: the same
/// steps, in order, with what each applied (`:cut` and `:skipped` included) and its level; each step's matches the
/// earlier steps lack, scored and ranked as the reference scores the step's query (L5: by recency); the response's
/// `level`, its `fallback` and `filters_dropped` flags, and for user text its `original` and its hit step's
/// `effective`. L0 alone ranks as the `plain` search when it has `min_results` hits, and pages are slices of the one
/// ranking. A `window` pass (L1's cut window, ROADMAP 8.1.18) runs only the plans that hold L1.
fn check_relaxed(
    views: &CollectionViews<'_>,
    engine: Engine<'_>,
    reference: &Reference<'_>,
    (root, text): (&RefQuery, Option<&Text>),
    plain: Option<&[SearchHit]>,
    (case, window): (&Case, bool),
) -> Result<(), TestCaseError> {
    let schema = reference.schema;
    let request = SearchRequest {
        limit: reference.live.len().max(1),
        explain: true,
        ..SearchRequest::default()
    };
    let limits = QueryLimits {
        max_expansions: case.max_expansions,
        max_terms: case.max_terms,
        ..QueryLimits::default()
    };
    let exec = Execution::default()
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)));
    // The collection's levels (L1 builds a step for user text with a run of synonym terms, never for a programmatic
    // query, which has no text), then a plan without L1 and L2, whose L3 and L4 run on L0's misses alone, and whose
    // L3 builds the tier step `prefix:last` whenever it expands another term beside the last (SPEC §7.6).
    prop_assert_eq!(
        schema.relaxation().levels.resolve(),
        [
            Level::Synonyms,
            Level::DropTerms,
            Level::Prefix,
            Level::Fuzzy
        ]
    );
    let collection = [
        RelaxLevel::Synonyms,
        RelaxLevel::DropTerms,
        RelaxLevel::Prefix,
        RelaxLevel::Fuzzy,
    ];
    let without_l2 = [RelaxLevel::Prefix, RelaxLevel::Fuzzy];
    // `"last_resort"` builds what `"never"` does, and L6 after it, so `"never"` runs once, on the plan without L2.
    for (min_results, filters, words, levels) in [
        (1, FilterRelaxation::LastResort, &collection[..], None),
        (10_000, FilterRelaxation::LastResort, &collection[..], None),
        (10_000, FilterRelaxation::Never, &without_l2[..], Some(())),
    ] {
        // The window pass checks L1 alone: the plan without L1 builds what the case already checked.
        if window && levels.is_some() {
            continue;
        }
        let levels = levels.map(|()| Levels::Custom(words.iter().map(|&l| level_of(l)).collect()));
        let relax = RelaxPlan::new(
            schema.relaxation(),
            &RelaxOptions {
                min_results: Some(min_results),
                levels,
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
        if !want.is_empty() {
            cover("corrected".to_owned());
        }
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
        if let Some(t) = text {
            reference.cover_split(t, &steps, hit_step);
        }
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
/// scores a query for one of them (filters selecting by its values), the `max_expansions` and `max_terms` limits the
/// steps after L0 are held to (SPEC §7.1.3; the other limits the defaults), and the runs of user text L1 expands.
struct Reference<'a> {
    schema: &'a CollectionSchema,
    corpus: &'a Corpus,
    params: &'a Params,
    live: &'a BTreeMap<String, Values>,
    eval: &'a Eval<'a>,
    max_expansions: usize,
    max_terms: usize,
    runs: &'a [RefRun],
}

/// A run of user text that is a synonym term (SPEC §6.3.2): the clauses it covers (indexes into [`Text::clauses`]),
/// its alternatives compiled, and whether the compilation's bound left some out.
#[derive(Debug)]
struct RefRun {
    clauses: Range<usize>,
    alternatives: Vec<RefQuery>,
    cut: bool,
}

/// SPEC §6.3.2's runs of `text` over the default fields `title` and `body`, in each maximal stretch of required words
/// (which an excluded word ends). Each field scans its own analysis ([`analyze`]: `whitespace`, `standard`) of the
/// groups and of the words ([`reference::synonyms::field_matches`]), the fields' matches merge
/// ([`reference::synonyms::merge`]: the union of the same clauses' alternatives in group then term order, a later
/// field's match that overlaps them otherwise dropped), each alternative compiles as the quoted phrase `"<term>"` would
/// ([`quoted`]), each once and none the run's own query (its clauses' parts), and the compilation keeps at most
/// `max_terms − 2 · n` of them (`(max_terms, n)` its limit and its query's terms, [`reference::synonyms::bound`]).
/// Covers each way the two analyses tell the runs apart ([`cover_fields`]).
fn reference_runs(
    text: &Text,
    groups: &[Vec<String>],
    params: &Params,
    fields: &[FieldId; 4],
    (max_terms, n): (usize, usize),
) -> Vec<RefRun> {
    let order = default_fields(fields);
    let synonyms = order.map(|f| reference::synonyms::Synonyms::new(groups, &|s| tokens(f, s)));
    let parts = text.parts(params, fields);
    let mut runs: Vec<(Range<usize>, usize, Vec<RefQuery>)> = Vec::new();
    let mut start = 0;
    while start < text.clauses.len() {
        if text.clauses[start].1 {
            start += 1;
            continue;
        }
        let end = (start..text.clauses.len())
            .find(|&i| text.clauses[i].1)
            .unwrap_or(text.clauses.len());
        let words: Vec<&str> = text.clauses[start..end]
            .iter()
            .map(|c| c.0.as_str())
            .collect();
        let per_field: Vec<_> = order
            .iter()
            .zip(&synonyms)
            .map(|(&f, s)| reference::synonyms::field_matches(s, &words, &|w| tokens(f, w)))
            .collect();
        cover_fields(&per_field);
        for (covered, terms) in reference::synonyms::merge(per_field) {
            let clauses = start + covered.start..start + covered.end;
            let mut own: Vec<RefQuery> = parts[clauses.clone()].concat();
            let own = if own.len() == 1 {
                own.remove(0)
            } else {
                RefQuery::Bool {
                    must: own,
                    should: Vec::new(),
                    must_not: Vec::new(),
                    filter: Vec::new(),
                    minimum_should_match: 0,
                }
            };
            if words[covered.clone()]
                .iter()
                .any(|w| analyze(BODY, w).len() > 1)
            {
                cover("synonyms run over a word of several tokens".to_owned());
            }
            let run_text = words[covered].join(" ");
            // Every term the fields report compiled, each once, none the run's own query; the run's share is of the
            // terms reported (at most 50), so one that compiles away still counts.
            let reported = terms.len().min(reference::synonyms::CAP);
            let mut alternatives: Vec<RefQuery> = Vec::new();
            for (_, t) in terms {
                // A term one field analyzes as it does the run's words and the other does not: the other field
                // reported it, as the field that reads it as the run cannot.
                let alike = order.map(|f| tokens(f, &t).0 == tokens(f, &run_text).0);
                if alike[0] != alike[1] {
                    cover("synonyms alike in one field only".to_owned());
                }
                let Some(q) = quoted(&t, &order, params) else {
                    continue;
                };
                if q == own {
                    continue;
                }
                // Each field reports a term once, and terms a field tells apart compile apart: a duplicate is a term
                // of one field's report compiled as another of the other field's.
                if alternatives.contains(&q) {
                    cover("synonyms duplicate across fields".to_owned());
                    continue;
                }
                alternatives.push(q);
            }
            // A run whose terms all compile away still takes its share, and is then no run (SPEC §6.3.2). Not reached
            // here: a term that compiles to the run's own query analyzes as the run in every field, so no field
            // reports it, and dropping duplicates keeps the first (`relax_synonyms` and the compiler's unit tests).
            runs.push((clauses, reported, alternatives));
        }
        start = end;
    }
    let lens: Vec<usize> = runs.iter().map(|r| r.1).collect();
    runs.into_iter()
        .zip(reference::synonyms::bound(&lens, max_terms, n))
        .filter(|((_, _, alternatives), _)| !alternatives.is_empty())
        .map(|((clauses, _, mut alternatives), keep)| {
            // Cut when the share left out an alternative the 50-cap keeps.
            let keep = keep.min(reference::synonyms::CAP);
            let cut = alternatives.len() > keep && keep < reference::synonyms::CAP;
            alternatives.truncate(keep);
            RefRun {
                clauses,
                alternatives,
                cut,
            }
        })
        .collect()
}

/// Coverage of how the default fields' matches (`per_field`, in field order) meet (SPEC §6.3.2): a match in one field
/// only, and kept; the same clauses matched in both, each reporting a place the other does not, and a later field's place
/// before an earlier field's; and a later field's match dropped for overlapping an earlier one's otherwise.
fn cover_fields(per_field: &[Vec<(Range<usize>, Vec<reference::synonyms::Alternative>)>]) {
    let [first, later] = per_field else {
        panic!("two default fields");
    };
    // A match the other field lacks and that is kept: a later field's overlapping an earlier one's is dropped.
    for (i, (mine, theirs)) in [(first, later), (later, first)].into_iter().enumerate() {
        for (range, _) in mine {
            let overlaps = |t: &&(Range<usize>, _)| t.0.start < range.end && range.start < t.0.end;
            if !theirs.iter().any(|t| t.0 == *range)
                && (i == 0 || !theirs.iter().any(|t| overlaps(&t)))
            {
                cover(format!(
                    "synonyms match in the {} field only",
                    ["first", "later"][i]
                ));
            }
        }
    }
    for (range, places) in later {
        let place = |a: &reference::synonyms::Alternative| a.0;
        if let Some((_, earlier)) = first.iter().find(|f| f.0 == *range) {
            let own: Vec<_> = places
                .iter()
                .map(place)
                .filter(|p| !earlier.iter().any(|e| e.0 == *p))
                .collect();
            if !own.is_empty() && earlier.iter().any(|e| !places.iter().any(|p| p.0 == e.0)) {
                cover("synonyms union of both fields".to_owned());
            }
            if own.iter().any(|p| earlier.iter().any(|e| *p < e.0)) {
                cover("synonyms later field's term first".to_owned());
            }
        } else if first
            .iter()
            .any(|f| f.0.start < range.end && range.start < f.0.end)
        {
            cover("synonyms overlap dropped".to_owned());
        }
    }
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
    /// The expansions the step as built needs beside the steps it excludes (SPEC §7.1.3): the `max_expansions` it fits
    /// uncut from (0 for L0).
    needs: usize,
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
/// The fewest characters of the last term L3 expands (SPEC §7.6, ROADMAP 8.1.20).
const LAST_PREFIX_LEN: usize = 2;

impl Reference<'_> {
    /// SPEC §7.6's cascade for `root` (of user `text`, or programmatic): L0, then the plan's `words` levels in cascade
    /// order (L1 `synonyms`, L2 `drop_terms`, L3 `prefix`, L4 `fuzzy`), L5 `fallback` and, `last_resort`, L6 `filters`. A level is built only
    /// while the hits found are fewer than `min_results`, and so is each of its steps. A step is held to the limit
    /// beside the earlier steps it excludes ([`Self::fit`], [`Self::fit_synonyms`]): as built, cut (`:cut`) or skipped (`:skipped`, not
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
            needs: 0,
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
                let needs = excluded.iter().map(|(_, q)| expansions(q)).sum::<usize>()
                    + expansions(&built.query);
                let fitted = if level == RelaxLevel::Synonyms {
                    self.fit_synonyms(root, text, &excluded, &steps[0])
                } else {
                    self.fit(&built.query, &excluded)
                };
                let Some((query, cut)) = fitted else {
                    steps.push(RefStep {
                        applied: format!("{}:skipped", built.applied),
                        level,
                        matches: None,
                        effective: None,
                        corrected: Vec::new(),
                        needs,
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
                    needs,
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
            // One step of user text with a run (SPEC §7.6 L1), its `effective` L0's; the limits hold it apart
            // ([`Self::fit_synonyms`]).
            RelaxLevel::Synonyms => match text {
                Some(t) if !self.runs.is_empty() => {
                    let all: Vec<usize> = self.runs.iter().map(|r| r.alternatives.len()).collect();
                    vec![built(
                        self.synonyms_step(root, t, &all),
                        "synonyms",
                        l0_text(),
                    )]
                }
                _ => Vec::new(),
            },
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
                    let len = t.chars().count();
                    if last && len < LAST_PREFIX_LEN {
                        cover("prefix last term of one character kept".to_owned());
                    }
                    (last && len >= LAST_PREFIX_LEN || len >= PREFIX_LEN).then(|| prefix(f, t))
                };
                let Some(full) = self.rewrite_root(root, text, &every) else {
                    return Vec::new();
                };
                // The tier step `prefix:last` (ROADMAP 8.2.7): unless L2's last rung was built and not skipped.
                let dropped = steps
                    .iter()
                    .rfind(|s| s.level == RelaxLevel::DropTerms)
                    .is_some_and(|s| s.matches.is_some());
                let last = |f: usize, t: &str, last: bool| {
                    (last && t.chars().count() >= LAST_PREFIX_LEN).then(|| prefix(f, t))
                };
                let mut out = Vec::new();
                if !dropped
                    && let Some(tier) = self.rewrite_root(root, text, &last)
                    && tier != full
                {
                    out.push(built(tier, "prefix:last", l0_text()));
                }
                out.push(built(full, "prefix", l0_text()));
                out
            }
            RelaxLevel::Fuzzy => {
                let Some(full) = self.rewrite_root(root, text, &|f, t, _| self.fuzzy(f, t, 2))
                else {
                    return Vec::new();
                };
                // The tier step `fuzzy:1` (ROADMAP 8.2.7) when a term may take two edits.
                let mut out = Vec::new();
                if let Some(near) = self.rewrite_root(root, text, &|f, t, _| self.fuzzy(f, t, 1))
                    && near != full
                {
                    out.push(self.fuzzy_step(near, "fuzzy:1", text, 1));
                }
                out.push(self.fuzzy_step(full, "fuzzy", text, 2));
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

    /// L4's `fuzzy` for `term` of `field` (SPEC §7.6): none below 4 characters, distance 1 up to 7 and 2 from 8, capped
    /// at `max`.
    fn fuzzy(&self, field: usize, term: &str, max: u8) -> Option<RefQuery> {
        let distance = match term.chars().count() {
            0..=3 => return None,
            4..=7 => 1,
            _ => 2,
        };
        Some(RefQuery::Fuzzy {
            field,
            term: term.to_owned(),
            distance: max.min(distance),
            max_expansions: 50,
            weight: self.params[field].0,
        })
    }

    /// An L4 step of user `text` (SPEC §7.6, ROADMAP 8.2.4.3): its corrections within distances capped at `max`, and
    /// `effective` L0's text with each corrected word replaced by its correction.
    fn fuzzy_step(&self, query: RefQuery, applied: &str, text: Option<&Text>, max: u8) -> Built {
        let Some(text) = text else {
            return Built {
                query,
                applied: applied.to_owned(),
                effective: None,
                corrected: Vec::new(),
            };
        };
        let corrections = self.corrections(text, max);
        let effective = text
            .sources()
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                corrections
                    .iter()
                    .find(|c| c.0 == i)
                    .map_or(s, |c| c.2.clone())
            })
            .collect::<Vec<_>>()
            .join(" ");
        Built {
            query,
            applied: applied.to_owned(),
            effective: Some(effective),
            corrected: corrections.into_iter().map(|(_, o, c)| (o, c)).collect(),
        }
    }

    /// The corrections of `text`'s words at distances capped at `max` (SPEC §7.6), each with its clause's index, in
    /// text order: a required word of one part, one token over the default fields (not one they split apart), no field
    /// of which holds its analyzed term (collection `df` 0), whose rewritten fields hold a term within its distance;
    /// its correction the term that field's `fuzzy` keeps first (the highest `df`, ties by bytes), the
    /// highest `df` over the fields, ties to the field first in the `combined` (the schema's order).
    fn corrections(&self, text: &Text, max: u8) -> Vec<(usize, String, String)> {
        let parts = text.parts(self.params, &ids(self.schema));
        let mut out = Vec::new();
        for (i, (word, excluded)) in text.clauses.iter().enumerate() {
            // A required word of one part, one token over the default fields: its `combined` lists each field's term.
            let [RefQuery::Combined(clauses)] = parts[i].as_slice() else {
                continue;
            };
            if *excluded || clauses.iter().any(|c| self.corpus.df(c.field, &c.term) > 0) {
                continue;
            }
            let mut best: Option<(u64, String)> = None;
            for Clause { field, term, .. } in clauses {
                let Some(RefQuery::Fuzzy { distance, .. }) = self.fuzzy(*field, term, max) else {
                    continue;
                };
                if let Some((term, df, _)) =
                    self.corpus.fuzzy_terms(*field, term, distance, 1).pop()
                    && best.as_ref().is_none_or(|(top, _)| df > *top)
                {
                    best = Some((df, term));
                }
            }
            if let Some((_, term)) = best {
                out.push((i, word.clone(), term));
            }
        }
        out
    }

    /// L2's `effective` for user text at count `k` (SPEC §7.6, ROADMAP 8.2.2.2): its `k` most important required
    /// clauses and every excluded one, in text order. A clause's term is its parts (several required together), and a
    /// term no document holds is the least important ([`Self::weigh`]), then (no stopwords here: neither analyzer
    /// removes a token) a lower IDF, then a later position.
    fn drop_terms_effective(&self, text: &Text, k: usize) -> String {
        let parts = text.parts(self.params, &ids(self.schema));
        let mut required: Vec<(bool, f64, usize)> = text
            .clauses
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.1)
            .map(|(i, _)| {
                // A clause of several parts is not reached here (see [`quoted`]): `drop_terms_steps` reads the root's
                // `must` entries as the terms, which are the clauses only while each has one part.
                let (none, idf) = match parts[i].as_slice() {
                    [one] => self.weigh(one),
                    many => self.weigh(&RefQuery::Bool {
                        must: many.to_vec(),
                        should: Vec::new(),
                        must_not: Vec::new(),
                        filter: Vec::new(),
                        minimum_should_match: 0,
                    }),
                };
                (none, idf, i)
            })
            .collect();
        // Most important first.
        required.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)));
        let kept: BTreeSet<usize> = required.iter().take(k).map(|r| r.2).collect();
        text.sources()
            .into_iter()
            .enumerate()
            .filter(|(i, _)| text.clauses[*i].1 || kept.contains(i))
            .map(|(_, s)| s)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Coverage of a word the default fields split apart (SPEC §7.1.1) where the cascade's compared output reads it
    /// (ROADMAP 8.1.24): L2's hit step, whose `effective` the words' importance decides; an L3 step with hits whose
    /// last required clause is one (each field's last token its last term); and L4's hit step beside one whose tokens
    /// no document holds though one of them has a correction, which a word of one token would get (SPEC §7.6: a word
    /// of several tokens is neither corrected nor rewritten in `effective`).
    fn cover_split(&self, text: &Text, steps: &[RefStep], hit_step: usize) {
        let parts = text.parts(self.params, &ids(self.schema));
        let required: Vec<usize> = (0..text.clauses.len())
            .filter(|&i| !text.clauses[i].1)
            .collect();
        let split = |i: usize| matches!(parts[i].as_slice(), [RefQuery::Bool { .. }]);
        let hit = &steps[hit_step];
        if hit.level == RelaxLevel::DropTerms && required.iter().any(|&i| split(i)) {
            cover("drop_terms hit step over a word the fields split apart".to_owned());
        }
        let found = |s: &RefStep| s.matches.as_ref().is_some_and(|m| !m.is_empty());
        if required.last().is_some_and(|&i| split(i))
            && steps
                .iter()
                .any(|s| s.level == RelaxLevel::Prefix && found(s))
        {
            cover("prefix hit over a last word the fields split apart".to_owned());
        }
        if hit.level == RelaxLevel::Fuzzy
            && required
                .iter()
                .any(|&i| split(i) && self.uncorrected(&parts[i][0]))
        {
            cover("fuzzy hit step beside a split word not corrected".to_owned());
        }
    }

    /// Whether no document holds any of `q`'s one-field terms and one of them has a term within its distance that
    /// some document holds: what a word of one token would be corrected to.
    fn uncorrected(&self, q: &RefQuery) -> bool {
        fn leaves<'q>(q: &'q RefQuery, out: &mut Vec<&'q Clause>) {
            match q {
                RefQuery::Combined(clauses) | RefQuery::Word(clauses) => out.extend(clauses),
                RefQuery::Bool { must, should, .. } => {
                    for c in must.iter().chain(should) {
                        leaves(c, out);
                    }
                }
                _ => {}
            }
        }
        let mut terms = Vec::new();
        leaves(q, &mut terms);
        terms.iter().all(|c| self.corpus.df(c.field, &c.term) == 0)
            && terms.iter().any(|c| {
                let Some(RefQuery::Fuzzy { distance, .. }) = self.fuzzy(c.field, &c.term, 2) else {
                    return false;
                };
                !self
                    .corpus
                    .fuzzy_terms(c.field, &c.term, distance, 1)
                    .is_empty()
            })
    }

    /// SPEC §7.6 L2's reading of a term `q`: whether it holds no document by the `df`s its IDF reads, and that IDF,
    /// over the search's statistics. A `term` or a `combined` holds none when every field's `df` is 0, and its IDF is of
    /// its largest `df`; a `phrase` holds none with a term of `df` 0, and its IDF is its terms' summed; a `bool` holds
    /// none with such a `must` clause or fewer other `should` clauses than it requires (its `minimum_should_match`, and
    /// at least one with no `must` or `filter` clause), and its IDF is its `must` clauses' summed plus the largest of
    /// its `should` clauses'; a `boost` or `constant_score` is its query's; an expansion and `match_all` have IDF 0.
    fn weigh(&self, q: &RefQuery) -> (bool, f64) {
        let n = self.corpus.n();
        match q {
            RefQuery::Word(clauses) | RefQuery::Combined(clauses) => {
                let df = clauses
                    .iter()
                    .map(|c| self.corpus.df(c.field, &c.term))
                    .max()
                    .unwrap_or(0);
                (df == 0, reference::search::idf(n, df))
            }
            RefQuery::Phrase { field, terms, .. } => {
                let dfs: Vec<u64> = terms.iter().map(|t| self.corpus.df(*field, &t.0)).collect();
                let idf = dfs.iter().map(|&df| reference::search::idf(n, df)).sum();
                (dfs.contains(&0), idf)
            }
            RefQuery::Bool {
                must,
                should,
                filter,
                minimum_should_match,
                ..
            } => {
                let must: Vec<(bool, f64)> = must.iter().map(|c| self.weigh(c)).collect();
                let should: Vec<(bool, f64)> = should.iter().map(|c| self.weigh(c)).collect();
                let requires = match *minimum_should_match {
                    0 if must.is_empty() && filter.is_empty() && !should.is_empty() => 1,
                    m => m,
                };
                let holding = should.iter().filter(|s| !s.0).count();
                let none = must.iter().any(|m| m.0) || holding < requires;
                let idf = must.iter().map(|m| m.1).sum::<f64>()
                    + should.iter().map(|s| s.1).fold(0.0, f64::max);
                (none, idf)
            }
            RefQuery::Boost(inner, _) | RefQuery::Constant(inner, _) => self.weigh(inner),
            RefQuery::Prefix { .. } | RefQuery::Fuzzy { .. } | RefQuery::All => (false, 0.0),
        }
    }

    /// Whether `whole` (a step beside the steps it excludes) is within the limits other than `max_expansions` (SPEC
    /// §7.1.3): at most `max_terms` leaf terms ([`terms`]), and the default `max_clauses` and `max_depth`, which the
    /// engine's validation judges (the reference counts no nodes). The term count must be the engine's: its
    /// validation accepts exactly that many.
    fn within(&self, whole: &RefQuery) -> bool {
        let fields = ids(self.schema);
        let query = engine_query(whole, &fields);
        let at = |max_terms| {
            let limits = QueryLimits {
                max_terms,
                max_expansions: usize::MAX,
                ..QueryLimits::default()
            };
            query.validate(self.schema, &limits)
        };
        // Over the clauses or the depth, the step is skipped; invalid for another reason, it is an error (SPEC
        // §7.1.3), which no generated step is.
        match at(usize::MAX) {
            Ok(()) => {}
            Err(QueryError::TooManyClauses { .. } | QueryError::TooDeep { .. }) => return false,
            Err(e) => panic!("an invalid step: {e:?} in {whole:?}"),
        }
        let n = terms(whole);
        assert!(
            at(n).is_ok() && (n == 0 || at(n - 1).is_err()),
            "the reference counts {n} terms in {whole:?}"
        );
        n <= self.max_terms
    }

    /// `step` beside the steps it excludes, as the limits count it (SPEC §7.1.3).
    fn whole(step: RefQuery, excluded: &[(RelaxLevel, RefQuery)]) -> RefQuery {
        RefQuery::Bool {
            must: vec![step],
            should: Vec::new(),
            must_not: excluded.iter().map(|(_, q)| q.clone()).collect(),
            filter: Vec::new(),
            minimum_should_match: 0,
        }
    }

    /// `step` held to the limits beside the `excluded` steps (SPEC §7.1.3). Over the terms, clauses or depth, `None`
    /// (skipped): cutting expansions removes no term. Otherwise every `max_expansions` of the step and of the steps it
    /// excludes is counted, those under a `must_not` too. Within the limit, the step as built (`false`). Over it, the
    /// step's own `prefix`s and `fuzzy`s outside a `must_not` are cut ([`share`]) to the expansions left (`true`);
    /// `None` (skipped) when fewer are left than it has such queries.
    fn fit(
        &self,
        step: &RefQuery,
        excluded: &[(RelaxLevel, RefQuery)],
    ) -> Option<(RefQuery, bool)> {
        if !self.within(&Self::whole(step.clone(), excluded)) {
            return None;
        }
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

    /// L1's step held to the limits beside the `excluded` steps (SPEC §7.1.3, ROADMAP 8.1.13): with every alternative
    /// the compilation kept when it fits (cut, `true`, when the compilation left some out), else with each run's
    /// first alternatives as [`reference::synonyms::cut`] counts them (`true`), else `None` (skipped). The reference's
    /// own step, at every shared count of alternatives per run and at the cut's counts, matches each document `l0`
    /// matched (ROADMAP 8.1.18): a self-check of how it puts the runs in place. The engine's step is checked for the
    /// same by `relax::synonyms` in-crate, as no search can see it (L0's documents are excluded from L1's hits).
    fn fit_synonyms(
        &self,
        root: &RefQuery,
        text: Option<&Text>,
        excluded: &[(RelaxLevel, RefQuery)],
        l0: &RefStep,
    ) -> Option<(RefQuery, bool)> {
        let Some(text) = text else {
            panic!("L1 builds from user text alone");
        };
        let lens: Vec<usize> = self.runs.iter().map(|r| r.alternatives.len()).collect();
        let step = |counts: &[usize]| self.synonyms_step(root, text, counts);
        let l0 = l0.matches.as_ref().unwrap();
        let holds_l0 = |counts: &[usize]| {
            let q = step(counts);
            for id in l0.keys() {
                assert!(
                    (self.eval)(&q, id).is_some(),
                    "L1's step with {counts:?} alternatives misses {id}, which L0 matches: {q:?}"
                );
            }
        };
        let top = lens.iter().copied().max().unwrap_or(0);
        for c in 1..=top {
            holds_l0(&lens.iter().map(|&l| l.min(c)).collect::<Vec<_>>());
        }
        let fits = |counts: &[usize]| {
            let whole = Self::whole(step(counts), excluded);
            self.within(&whole) && expansions(&whole) <= self.max_expansions
        };
        if fits(&lens) {
            let compiled_cut = self.runs.iter().any(|r| r.cut);
            if compiled_cut {
                cover("synonyms compiled cut that fits".to_owned());
            }
            return Some((step(&lens), compiled_cut));
        }
        let (shared, counts) = reference::synonyms::cut(&lens, &fits)?;
        holds_l0(&counts);
        // The cut's second phase gave a list one more than the shared count.
        if counts.iter().any(|&c| c > shared) {
            cover("synonyms cut one more".to_owned());
        }
        Some((step(&counts), true))
    }

    /// L1's step for `root` of user `text` (SPEC §7.6): each run, with its first `counts` alternatives, required as
    /// `bool { should: [the run, its alternatives...] }` (the run its word, or a `bool` requiring its words) in place
    /// of its words in the root's `must`; L0's other required words, its excluded words and its filter as they are.
    /// A root of one word is that `bool` alone.
    fn synonyms_step(&self, root: &RefQuery, text: &Text, counts: &[usize]) -> RefQuery {
        let (must_not, filter) = match root {
            RefQuery::Bool {
                must_not, filter, ..
            } if !text.single(self.params, &ids(self.schema)) => (must_not.clone(), filter.clone()),
            _ => (Vec::new(), Vec::new()),
        };
        // Each clause's parts in the root.
        let parts = text.parts(self.params, &ids(self.schema));
        let mut out = Vec::new();
        let mut i = 0;
        while i < text.clauses.len() {
            if text.clauses[i].1 {
                i += 1;
                continue;
            }
            let Some((run, &count)) = self
                .runs
                .iter()
                .zip(counts)
                .find(|(r, _)| r.clauses.start == i)
            else {
                out.extend(parts[i].iter().cloned());
                i += 1;
                continue;
            };
            let mut own: Vec<RefQuery> = parts[run.clauses.clone()].concat();
            let own = if own.len() == 1 {
                own.remove(0)
            } else {
                RefQuery::Bool {
                    must: own,
                    should: Vec::new(),
                    must_not: Vec::new(),
                    filter: Vec::new(),
                    minimum_should_match: 0,
                }
            };
            out.push(RefQuery::Bool {
                must: Vec::new(),
                should: std::iter::once(own)
                    .chain(run.alternatives[..count].iter().cloned())
                    .collect(),
                must_not: Vec::new(),
                filter: Vec::new(),
                minimum_should_match: 0,
            });
            i = run.clauses.end;
        }
        if out.len() == 1 && must_not.is_empty() && filter.is_empty() {
            return out.remove(0);
        }
        RefQuery::Bool {
            must: out,
            should: Vec::new(),
            must_not,
            filter,
            minimum_should_match: 0,
        }
    }

    /// L3's and L4's step for `root` (SPEC §7.6): the terms of a root `bool`'s `must` clauses rewritten by `leaf`, the
    /// last clause's last term the last term, or of a word alone (under `boost`s); `None` when no term is rewritten
    /// or a programmatic root `bool` has `should` clauses or a `minimum_should_match`. User `text` of one part is that
    /// part, whatever it is (a word the fields split apart is a `bool` of `should` clauses, one per field).
    fn rewrite_root(
        &self,
        root: &RefQuery,
        text: Option<&Text>,
        leaf: &Leaf<'_>,
    ) -> Option<RefQuery> {
        if text.is_some_and(|t| t.single(self.params, &ids(self.schema))) {
            return self.rewrite(root, true, leaf);
        }
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
    ///
    /// A `combined` left with one `should` clause is that clause (SPEC §7.6 L3: a one-field `combined`, as of a word the
    /// fields split apart, becomes its expansion). One detail follows the engine (`relax/words.rs`), not a SPEC rule,
    /// and these generators never reach it: an empty term is kept exact.
    fn rewrite(&self, q: &RefQuery, last: bool, leaf: &Leaf<'_>) -> Option<RefQuery> {
        let open = |c: &Clause| {
            (c.field != TAG && !c.term.is_empty()).then(|| leaf(c.field, &c.term, last))?
        };
        match q {
            RefQuery::Word(clauses) | RefQuery::Combined(clauses) if clauses.len() == 1 => {
                open(&clauses[0])
            }
            RefQuery::Word(clauses) | RefQuery::Combined(clauses) => {
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
        RefQuery::Prefix { max_expansions, .. } | RefQuery::Fuzzy { max_expansions, .. } => {
            *max_expansions
        }
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
        RefQuery::Word(_) | RefQuery::Combined(_) | RefQuery::Phrase { .. } | RefQuery::All => 0,
    }
}

/// SPEC §7.1.3's `max_terms` count of `q`: per field of a word (a `term` or `combined`), per `prefix` and `fuzzy`, per
/// `phrase` term, and per filter value ([`filter_terms`]).
fn terms(q: &RefQuery) -> usize {
    match q {
        RefQuery::Word(clauses) | RefQuery::Combined(clauses) => clauses.len(),
        RefQuery::Phrase { terms, .. } => terms.len(),
        RefQuery::Prefix { .. } | RefQuery::Fuzzy { .. } => 1,
        RefQuery::Bool {
            must,
            should,
            must_not,
            filter,
            ..
        } => {
            must.iter()
                .chain(should)
                .chain(must_not)
                .map(terms)
                .sum::<usize>()
                + filter.iter().map(filter_terms).sum::<usize>()
        }
        RefQuery::Boost(inner, _) | RefQuery::Constant(inner, _) => terms(inner),
        RefQuery::All => 0,
    }
}

/// A filter's values as `max_terms` counts them (SPEC §7.1.3): a `term`'s one, a `terms`' each, a `range`'s bounds.
fn filter_terms(f: &Filter) -> usize {
    match f {
        Filter::Term { .. } => 1,
        Filter::Terms { values, .. } => values.len(),
        Filter::Range { lower, upper, .. } => [lower, upper]
            .iter()
            .filter(|b| !matches!(b, Bound::Unbounded))
            .count(),
        Filter::Exists { .. } => 0,
        Filter::And(fs) | Filter::Or(fs) => fs.iter().map(filter_terms).sum(),
        Filter::Not(inner) => filter_terms(inner),
        other => panic!("not a filter the suite generates: {other:?}"),
    }
}

/// The `max_expansions` a cut may lower in `q` (SPEC §7.1.3), in query order: depth first, a `bool`'s `must` clauses
/// before its `should` clauses; not those under a `must_not`, which would exclude less.
fn cuttable<'q>(q: &'q mut RefQuery, caps: &mut Vec<&'q mut usize>) {
    match q {
        RefQuery::Prefix { max_expansions, .. } | RefQuery::Fuzzy { max_expansions, .. } => {
            caps.push(max_expansions);
        }
        RefQuery::Bool { must, should, .. } => {
            for c in must.iter_mut().chain(should.iter_mut()) {
                cuttable(c, caps);
            }
        }
        RefQuery::Boost(inner, _) | RefQuery::Constant(inner, _) => cuttable(inner, caps),
        RefQuery::Word(_) | RefQuery::Combined(_) | RefQuery::Phrase { .. } | RefQuery::All => {}
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
        RefQuery::Word(_)
        | RefQuery::Combined(_)
        | RefQuery::Phrase { .. }
        | RefQuery::Prefix { .. }
        | RefQuery::Fuzzy { .. } => true,
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
        RelaxLevel::Synonyms => Level::Synonyms,
        RelaxLevel::DropTerms => Level::DropTerms,
        RelaxLevel::Prefix => Level::Prefix,
        RelaxLevel::Fuzzy => Level::Fuzzy,
        other => panic!("not a word level: {other:?}"),
    }
}
