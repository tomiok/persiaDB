//! The differential suite (ROADMAP 7.8.1; SPEC §7.1–§7.4, §7.7, §8.3): random corpora written through a `Database`
//! (puts, upserts, deletes, commits that flush the memtable into segments and drop the segments left with no live
//! document), then random queries with filters, searched over the whole collection, against the reference
//! (`tests/reference`), which scans the live documents and counts the statistics itself.
//!
//! Invariants, after every operation:
//! 1. A search returns exactly the live documents the reference matches and the filter selects, each once, with
//!    the reference's score within [`EPSILON`] of the query's scale. Statistics count every document of every
//!    segment, deleted or replaced ones too, plus the memtable's live documents (SPEC §7.2).
//! 2. Hits rank by score descending, then segment order with the memtable last, then docid (SPEC §7.3).
//! 3. Every pruning strategy, on the calling thread and on a pool, returns the same page, scores bit for bit; a page
//!    of `k` is the first `k` of the full ranking, and the page after its last cursor is the next `k`.
//! 4. Each hit's explanation scores the hit, bit for bit; each field of a word lists the frequency the reference
//!    counts, and the contributions sum to its `tf~` (SPEC §7.7). Explaining every docid of a view in one pass
//!    (`SearchPlan::explain_all`, ROADMAP 7.7.2), in descending order and one past the end, gives what a fresh tree
//!    gives each of them, matched or not.

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

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Arc;

use persia_engine::db::{
    CollectionSnapshot, Database, DatabaseOptions, Execution, Located, ManualClock, SearchHit,
    SearchRequest, SegmentIds, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::MemtableView;
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::query::{
    BoolQuery, BoostQuery, CombinedQuery, ConstantScoreQuery, Filter, FilterValue, PhraseQuery,
    PhraseTerm, Query, QueryLimits, TermQuery, WeightedTerm,
};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::search::{Explanation, Pruning, SearchPlan};
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
const VOCAB: [&str; 4] = ["ant", "bee", "cat", "dog"];
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

fn word() -> impl Strategy<Value = String> {
    prop::sample::select(&VOCAB[..]).prop_map(str::to_owned)
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
        4 => (0..2_usize, word()).prop_map(move |(field, term)| RefQuery::Word(vec![Clause {
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
    ]
}

fn query(params: Params) -> impl Strategy<Value = RefQuery> {
    leaf(params).prop_recursive(3, 16, 4, move |inner| {
        prop_oneof![
            4 => (
                prop::collection::vec(inner.clone(), 0..3),
                prop::collection::vec(inner.clone(), 0..3),
                prop::collection::vec(inner.clone(), 0..2),
            )
                .prop_map(|(must, should, must_not)| RefQuery::Bool { must, should, must_not }),
            // Wide: a disjunction past its few-clause scan, onto its heap (ROADMAP 7.4.7).
            1 => prop::collection::vec(leaf(params), 9..14)
                .prop_map(|should| RefQuery::Bool { must: Vec::new(), should, must_not: Vec::new() }),
            1 => (inner.clone(), 0.5..4.0_f64).prop_map(|(q, b)| RefQuery::Boost(Box::new(q), b)),
            1 => (inner, 0.0..5.0_f64).prop_map(|(q, s)| RefQuery::Constant(Box::new(q), s)),
        ]
    })
}

/// A filter over `tag` and `n`, by field id (the schema's ids do not depend on the parameters).
fn filter(tag: FieldId, n: FieldId) -> impl Strategy<Value = Filter> {
    let keyword = || prop::sample::select(&TAGS[..]).prop_map(|t| FilterValue::Keyword(t.into()));
    let leaf = prop_oneof![
        keyword().prop_map(move |value| Filter::Term { field: tag, value }),
        prop::collection::vec(keyword(), 0..3)
            .prop_map(move |values| Filter::Terms { field: tag, values }),
        (prop::sample::select(&NS[..]), prop::sample::select(&NS[..])).prop_map(move |(a, b)| {
            Filter::Range {
                field: n,
                lower: std::ops::Bound::Included(FilterValue::I64(a.min(b))),
                upper: std::ops::Bound::Included(FilterValue::I64(a.max(b))),
            }
        }),
        Just(Filter::Exists { field: n }),
        Just(Filter::Exists { field: tag }),
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
    k: usize,
}

fn case() -> impl Strategy<Value = Case> {
    let fields = ids(&schema(&[(1.0, 1.2, 0.75); 2]));
    let (tag, n) = (fields[TAG], fields[3]);
    params().prop_flat_map(move |p| {
        (
            Just(p),
            prop::collection::vec(op(), 1..20),
            prop::collection::vec(
                (
                    query(p),
                    prop::option::weighted(0.5, (filter(tag, n), any::<bool>())),
                ),
                1..4,
            ),
            1..6_usize,
        )
            .prop_map(|(params, ops, queries, k)| Case {
                params,
                ops,
                queries,
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
        };
        Corpus {
            fields: vec![
                text(params[0]),
                text(params[1]),
                RefField {
                    k1: 1.2,
                    b: 0.0,
                    keyword: true,
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
    for (q, filter) in &case.queries {
        let mut query = engine_query(q, &fields);
        match filter {
            Some((f, false)) => {
                query = Query::Bool(BoolQuery {
                    must: vec![query],
                    filter: vec![f.clone()],
                    ..BoolQuery::default()
                });
            }
            // No `must`: the filter alone selects, and the `should` only scores (SPEC §7.1).
            Some((f, true)) => {
                query = Query::Bool(BoolQuery {
                    should: vec![query],
                    filter: vec![f.clone()],
                    ..BoolQuery::default()
                });
            }
            None => {}
        }
        let optional = matches!(filter, Some((_, true)));
        query.validate(schema, &QueryLimits::default()).unwrap();
        let plan = SearchPlan::new(&query, schema, &stats).unwrap();

        // Invariant 1: the matches and their scores.
        let want: BTreeMap<&String, (f64, f64)> = live
            .iter()
            .filter(|(id, v)| {
                filter
                    .as_ref()
                    .is_none_or(|(f, _)| selects(f, &v.document(&fields, id), schema))
            })
            .filter_map(|(id, v)| match corpus.eval(q, &v.doc) {
                None if optional => Some((id, (0.0, 0.0))),
                s => s.map(|s| (id, s)),
            })
            .collect();
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

        // Invariant 2: the order.
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

/// Invariants 1–4 over random histories and queries.
#[test]
fn collection_searches_match_the_reference() {
    let cases = if cfg!(feature = "slow") { 512 } else { 48 };
    run(
        "collection_searches_match_the_reference",
        cases,
        case(),
        |case| run_case(&case),
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
