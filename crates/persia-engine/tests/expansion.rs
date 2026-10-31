//! `prefix` and `fuzzy` planned over a collection's terms (ROADMAP 7.4.12; SPEC §7.1).
//!
//! Invariants:
//! 1. An expansion keeps the terms of every view that it takes (a byte prefix, or the optimal string alignment
//!    distance over characters), at most `max_expansions` of them: the highest collection `df` first, then bytes.
//!    Every kept term scores with one `idf`, from the largest of those `df`s. A fuzzy term is then multiplied by
//!    `(distance + 1 − d) / (distance + 1)`. Hits match that sum, bit for bit, under every pruning strategy. The
//!    oracle enumerates the terms itself and evaluates SPEC §7.2 on the postings; it does not plan a `bool` of
//!    `term`s, which would give each term its own `idf`.
//! 2. The cap ranks by the collection `df`, not a view's own.
//! 3. An expansion that keeps no term matches nothing; `SearchPlan::new` refuses to expand; an empty prefix, a
//!    distance other than 1 or 2, and a field that is not `text` or `keyword` are errors.
//! 4. A rare fuzzy misspelling does not outrank the common exact term. A rare prefix completion scores the same as
//!    a common one at the same frequency and length, and a higher frequency still scores higher.
//! 5. The prefix and the fuzzy term are raw bytes. The analyzer is not applied.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::unreachable,
    clippy::cast_possible_truncation,
    clippy::needless_range_loop,
    clippy::many_single_char_names,
    clippy::type_complexity,
    missing_docs
)] // Tests: inputs are generated or fixed; the distance table is the textbook one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use fst::automaton::AlwaysMatch;
use persia_engine::db::{
    CollectionHit, CollectionSnapshot, Database, DatabaseOptions, Located, ManualClock, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::MemtableView;
use persia_engine::query::{BoolQuery, FuzzyQuery, PrefixQuery, Query, QueryError};
use persia_engine::schema::{
    Field, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::score::{FieldParams, ScoreError};
use persia_engine::search::{Pruning, SearchError, SearchPlan};
use persia_engine::stats::StatisticsProvider;
use persia_engine::view::{NormReader, PostingsIterator, SearchView, TermEnum};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;

/// Words close to each other, so prefixes and edits reach several; two spell `é`.
const VOCAB: [&str; 14] = [
    "run", "runs", "rune", "runner", "running", "ran", "rum", "urn", "shoe", "shoes", "shose",
    "cafe", "café", "caff",
];
const STRATEGIES: [Pruning; 3] = [
    Pruning::Exhaustive,
    Pruning::MaxScore,
    Pruning::BlockMaxWand,
];

struct Fields {
    body: FieldId,
    tag: FieldId,
    price: FieldId,
}

fn database() -> (Database<SimFile>, Fields) {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .field("price", FieldKind::I64 { stored: false })
        .build()
        .unwrap();
    let fields = Fields {
        body: schema.field("body").unwrap().id(),
        tag: schema.field("tag").unwrap().id(),
        price: schema.field("price").unwrap().id(),
    };
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [5; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    (db, fields)
}

fn put(db: &mut Database<SimFile>, f: &Fields, id: &str, body: &str, tag: &str) {
    let doc = Document::new(id)
        .with(f.body, Value::String(body.into()))
        .with(f.tag, Value::String(tag.into()));
    db.put("c", &doc, Durability::Fsync).unwrap();
}

/// Optimal string alignment distance over characters, by the textbook table.
fn osa(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0_usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[a.len()][b.len()]
}

/// Every term of `field` some view lists.
fn all_terms(c: &CollectionSnapshot, memtable: &MemtableView, field: FieldId) -> BTreeSet<Vec<u8>> {
    fn of(view: &impl SearchView, field: FieldId, out: &mut BTreeSet<Vec<u8>>) {
        use core::ops::Bound::Unbounded;
        let mut terms = view
            .terms(field, Unbounded, Unbounded, AlwaysMatch)
            .unwrap();
        while let Some((t, _)) = terms.next().unwrap() {
            out.insert(t.to_vec());
        }
    }
    let views = c.views(memtable).unwrap();
    let mut out = BTreeSet::new();
    for segment in views.segments() {
        of(&segment, field, &mut out);
    }
    of(views.memtable(), field, &mut out);
    out
}

/// A term the expansion keeps, and the weight SPEC §7.1 multiplies its score by.
struct Kept {
    term: String,
    weight: f64,
}

/// `(max_distance + 1 − edits) / (max_distance + 1)`, as SPEC §7.1 writes it.
fn edit_weight(max_distance: u8, edits: usize) -> f64 {
    let edits = u8::try_from(edits).unwrap_or(u8::MAX).min(max_distance);
    let span = f64::from(max_distance) + 1.0;
    (span - f64::from(edits)) / span
}

/// `idf` as SPEC §7.2 writes it.
#[allow(clippy::cast_precision_loss)] // counts far below 2^52, as `score::idf`
fn idf(n: u64, df: u64) -> f64 {
    let df = df.min(n);
    let rest = n.saturating_sub(df) as f64;
    let df = df as f64;
    ((rest + 0.5) / (df + 0.5)).ln_1p()
}

/// The length a norm byte stands for (SPEC §5.3.4): below 24 the byte itself, then the 4-bit code.
fn scored_len(norm: u8) -> f64 {
    let code = u32::from(norm);
    let len = match code.checked_sub(24) {
        None => code,
        Some(c) => {
            let bits = c & 0x07;
            let extra = match (c >> 3).checked_sub(1) {
                None => bits,
                Some(shift) => (bits | 0x08).checked_shl(shift).unwrap_or(u32::MAX),
            };
            24_u32.saturating_add(extra)
        }
    };
    f64::from(len)
}

/// SPEC §7.2's evaluated form, `idf·(k1+1) − idf·(k1+1)·k1 / (k1 + tf~)`, on one field.
fn bm25(idf: f64, k1: f64, weight: f64, b: f64, avglen: f64, tf: u32, norm: u8) -> f64 {
    if tf == 0 {
        return 0.0;
    }
    let denom = (1.0 - b + b * scored_len(norm) / avglen).max(f64::EPSILON);
    let tf_tilde = f64::from(tf) * (weight / denom);
    if tf_tilde.is_nan() || tf_tilde <= 0.0 {
        return 0.0;
    }
    let top = idf * (k1 + 1.0);
    (top - top * k1 / (k1 + tf_tilde)).max(0.0)
}

/// The terms SPEC §7.1 keeps, in byte order, each with its weight. The distance is the textbook table above,
/// not the engine's automaton.
fn kept_terms(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    query: &Query,
) -> (FieldId, Vec<Kept>) {
    let (field, cap, weight_of): (FieldId, u32, Box<dyn Fn(&str) -> Option<f64>>) = match query {
        Query::Prefix(q) => {
            let p = q.prefix.clone();
            (
                q.field,
                q.max_expansions,
                Box::new(move |t| t.starts_with(&p).then_some(1.0)),
            )
        }
        Query::Fuzzy(q) => {
            let (term, max_distance) = (q.term.clone(), q.distance);
            let limit = usize::from(max_distance);
            (
                q.field,
                q.max_expansions,
                Box::new(move |t| {
                    let edits = osa(&term, t);
                    (edits <= limit).then(|| edit_weight(max_distance, edits))
                }),
            )
        }
        _ => unreachable!(),
    };
    let stats = c.search_statistics(memtable);
    let mut ranked: Vec<(u64, String, f64)> = all_terms(c, memtable, field)
        .into_iter()
        .map(|t| String::from_utf8(t).unwrap())
        .filter_map(|t| weight_of(&t).map(|w| (t, w)))
        .map(|(t, w)| (stats.df(field, t.as_bytes()).unwrap(), t, w))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked.truncate(usize::try_from(cap).unwrap());
    let mut kept: Vec<Kept> = ranked
        .into_iter()
        .map(|(_, term, weight)| Kept { term, weight })
        .collect();
    kept.sort_by(|a, b| a.term.cmp(&b.term));
    (field, kept)
}

fn add_term(
    view: &impl SearchView,
    index: usize,
    field: FieldId,
    term: &[u8],
    text: bool,
    score_one: &impl Fn(u32, u8) -> f64,
    scores: &mut BTreeMap<(usize, u32), f64>,
) {
    let Some(mut postings) = view.postings(field, term).unwrap() else {
        return;
    };
    let mut norms = text.then(|| view.norms(field).unwrap());
    while let Some(docid) = postings.next().unwrap() {
        let tf = postings.freq().unwrap();
        let norm = norms.as_mut().map_or(0, |n| n.get(docid).unwrap());
        let add = score_one(tf, norm);
        scores
            .entry((index, docid))
            .and_modify(|s| *s += add)
            .or_insert(add);
    }
}

/// The hits SPEC §7.1 scores: one `idf` from the largest kept `df`, fuzzy weights, summed in byte order from
/// each document's frequency and norm. Not a `bool` of per-term `idf`s.
fn reference_hits(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    query: &Query,
    k: usize,
) -> Vec<CollectionHit> {
    let (field, kept) = kept_terms(c, memtable, query);
    if kept.is_empty() || k == 0 {
        return Vec::new();
    }
    let stats = c.search_statistics(memtable);
    let blended = kept
        .iter()
        .map(|t| stats.df(field, t.term.as_bytes()).unwrap())
        .max()
        .unwrap_or(0);
    let idf = idf(stats.doc_count(), blended);
    let params = FieldParams::of(c.schema(), field, None).unwrap();
    let text = matches!(
        c.schema().field_by_id(field).map(Field::kind),
        Some(FieldKind::Text(_))
    );
    let avglen = stats.field(field).avglen();
    let views = c.views(memtable).unwrap();
    let mut scores: BTreeMap<(usize, u32), f64> = BTreeMap::new();
    for term in &kept {
        let score_one = |tf, norm| {
            let raw = bm25(idf, params.k1, params.weight, params.b, avglen, tf, norm);
            if term.weight.to_bits() == 1.0_f64.to_bits() {
                raw
            } else {
                raw * term.weight
            }
        };
        for (index, segment) in views.segments().enumerate() {
            add_term(
                &segment,
                index,
                field,
                term.term.as_bytes(),
                text,
                &score_one,
                &mut scores,
            );
        }
        add_term(
            views.memtable(),
            usize::MAX,
            field,
            term.term.as_bytes(),
            text,
            &score_one,
            &mut scores,
        );
    }
    let mut hits: Vec<CollectionHit> = scores
        .into_iter()
        .map(|((index, docid), score)| CollectionHit {
            location: if index == usize::MAX {
                Located::Memtable(docid)
            } else {
                Located::Segment { index, docid }
            },
            score,
        })
        .collect();
    // Same order `CollectionViews::top_k` uses: location order already, then a stable sort by score.
    hits.sort_by(|a, b| (b.score + 0.0).total_cmp(&(a.score + 0.0)));
    hits.truncate(k);
    hits
}

fn bits(hits: &[CollectionHit]) -> Vec<(Located, u64)> {
    hits.iter()
        .map(|h| (h.location, h.score.to_bits()))
        .collect()
}

/// The hits of `query` expanded over the collection, the same under every strategy.
fn expanded_hits(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    query: &Query,
    k: usize,
) -> Vec<CollectionHit> {
    let views = c.views(memtable).unwrap();
    let stats = c.search_statistics(memtable);
    let plan = SearchPlan::with_terms(query, c.schema(), &stats, &views).unwrap();
    let exhaustive = views.top_k(&plan, k, Pruning::Exhaustive).unwrap();
    for pruning in STRATEGIES {
        let hits = views.top_k(&plan, k, pruning).unwrap();
        assert_eq!(bits(&hits), bits(&exhaustive), "{query:?} with {pruning:?}");
    }
    exhaustive
}

/// Invariant 1 for `query`.
fn check(c: &CollectionSnapshot, memtable: &MemtableView, query: &Query, k: usize) {
    let got = expanded_hits(c, memtable, query, k);
    let want = reference_hits(c, memtable, query, k);
    assert_eq!(bits(&got), bits(&want), "{query:?}");
}

#[derive(Debug, Clone)]
enum Op {
    Put(u8, Vec<u8>, u8),
    Delete(u8),
    Commit,
}

fn op() -> impl Strategy<Value = Op> {
    let words = prop::collection::vec(0..VOCAB.len() as u8, 1..5);
    prop_oneof![
        6 => (0..12_u8, words, 0..VOCAB.len() as u8).prop_map(|(id, w, t)| Op::Put(id, w, t)),
        1 => (0..12_u8).prop_map(Op::Delete),
        1 => Just(Op::Commit),
    ]
}

#[derive(Debug, Clone)]
enum Shape {
    Prefix(bool, u8, usize, u32),
    Fuzzy(bool, String, u8, u32),
}

fn shape() -> impl Strategy<Value = Shape> {
    let word = (0..VOCAB.len()).prop_map(|i| VOCAB[i]);
    // A vocabulary word, or one edited, so edits on both sides are reached.
    let fuzzy_term = prop_oneof![
        word.clone().prop_map(str::to_owned),
        (
            word.clone(),
            0..4_usize,
            prop::sample::select(vec!['u', 'é', 'x'])
        )
            .prop_map(|(w, at, c)| {
                let mut chars: Vec<char> = w.chars().collect();
                let at = at.min(chars.len() - 1);
                chars[at] = c;
                chars.into_iter().collect()
            }),
    ];
    prop_oneof![
        (any::<bool>(), 0..VOCAB.len() as u8, 1..5_usize, 1..6_u32)
            .prop_map(|(tag, w, len, cap)| Shape::Prefix(tag, w, len, cap)),
        (any::<bool>(), fuzzy_term, 1..=2_u8, 1..6_u32)
            .prop_map(|(tag, t, d, cap)| Shape::Fuzzy(tag, t, d, cap)),
    ]
}

impl Shape {
    fn query(&self, f: &Fields) -> Query {
        let field = |tag: bool| if tag { f.tag } else { f.body };
        match self {
            Self::Prefix(tag, w, len, cap) => {
                let word = VOCAB[usize::from(*w)];
                let end = word
                    .char_indices()
                    .map(|(i, _)| i)
                    .nth(*len)
                    .unwrap_or(word.len());
                Query::Prefix(PrefixQuery {
                    field: field(*tag),
                    prefix: word.get(..end).unwrap().to_owned(),
                    max_expansions: *cap,
                })
            }
            Self::Fuzzy(tag, term, distance, cap) => Query::Fuzzy(FuzzyQuery {
                field: field(*tag),
                term: term.clone(),
                distance: *distance,
                max_expansions: *cap,
            }),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 256 } else { 32 }))]

    /// Invariant 1, over segments, deletions and the memtable.
    #[test]
    fn expansions_score_with_one_shared_idf(
        ops in prop::collection::vec(op(), 1..40),
        shapes in prop::collection::vec(shape(), 1..6),
        k in 1..30_usize,
    ) {
        let (mut db, f) = database();
        for op in &ops {
            match op {
                Op::Put(id, words, tag) => {
                    let body: Vec<&str> = words.iter().map(|&w| VOCAB[usize::from(w)]).collect();
                    put(&mut db, &f, &format!("d{id}"), &body.join(" "), VOCAB[usize::from(*tag)]);
                }
                Op::Delete(id) => {
                    db.delete("c", &format!("d{id}"), Durability::Fsync).unwrap();
                }
                Op::Commit => {
                    db.commit().unwrap();
                }
            }
        }
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        for s in &shapes {
            check(c, &memtable, &s.query(&f), k);
        }
    }
}

/// The terms a plan of `query` keeps, by which ids it finds: each document holds one word.
fn found(db: &Database<SimFile>, query: &Query) -> Vec<String> {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let hits = expanded_hits(c, &memtable, query, 100);
    let mut ids: Vec<String> = hits
        .iter()
        .map(|h| match h.location {
            Located::Memtable(docid) => memtable.document(docid).unwrap().id().to_owned(),
            Located::Segment { index, docid } => c.segments()[index].id_of(docid).unwrap(),
        })
        .collect();
    ids.sort();
    ids
}

/// Invariant 2: `runa` is in two documents of one segment (its `df` there is 2), `runb` in one document of each of
/// three views (1 each, 3 in the collection). A cap of 1 keeps `runb`.
#[test]
fn the_cap_ranks_by_the_collection_df() {
    let (mut db, f) = database();
    put(&mut db, &f, "a1", "runa", "t");
    put(&mut db, &f, "a2", "runa", "t");
    db.commit().unwrap();
    put(&mut db, &f, "b1", "runb", "t");
    db.commit().unwrap();
    put(&mut db, &f, "b2", "runb", "t");
    db.commit().unwrap();
    put(&mut db, &f, "b3", "runb", "t");
    let prefix = |cap| {
        Query::Prefix(PrefixQuery {
            field: f.body,
            prefix: "run".into(),
            max_expansions: cap,
        })
    };
    assert_eq!(found(&db, &prefix(1)), ["b1", "b2", "b3"]);
    assert_eq!(found(&db, &prefix(2)), ["a1", "a2", "b1", "b2", "b3"]);
    // Equal `df`s: the smaller bytes win.
    put(&mut db, &f, "a3", "runa", "t");
    assert_eq!(found(&db, &prefix(1)), ["a1", "a2", "a3"]);
    // A swap is one edit, `runc` one substitution; `nura` is three away.
    put(&mut db, &f, "c1", "rnua", "t");
    put(&mut db, &f, "c2", "nura", "t");
    let fuzzy = Query::Fuzzy(FuzzyQuery {
        field: f.body,
        term: "runa".into(),
        distance: 1,
        max_expansions: 10,
    });
    assert_eq!(
        found(&db, &fuzzy),
        ["a1", "a2", "a3", "b1", "b2", "b3", "c1"]
    );
}

/// Invariant 3.
#[test]
fn nothing_kept_matches_nothing_and_bad_expansions_are_errors() {
    let (mut db, f) = database();
    put(&mut db, &f, "a", "running shoes", "red");
    // The first term after every one starting with `runnin`: not one of them.
    put(&mut db, &f, "b", "runnio", "blue");
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let stats = c.search_statistics(&memtable);
    let plan = |q: &Query| SearchPlan::with_terms(q, c.schema(), &stats, &views);
    let prefix = |field, p: &str| {
        Query::Prefix(PrefixQuery {
            field,
            prefix: p.into(),
            max_expansions: 10,
        })
    };
    let fuzzy = |field, t: &str, distance| {
        Query::Fuzzy(FuzzyQuery {
            field,
            term: t.into(),
            distance,
            max_expansions: 10,
        })
    };
    for q in [
        prefix(f.body, "zz"),
        fuzzy(f.body, "xyzzy", 2),
        prefix(f.tag, "run"),
    ] {
        let hits = views
            .top_k(&plan(&q).unwrap(), 10, Pruning::default())
            .unwrap();
        assert!(hits.is_empty(), "{q:?}");
    }
    let hits = views.top_k(
        &plan(&prefix(f.body, "runnin")).unwrap(),
        10,
        Pruning::default(),
    );
    assert_eq!(hits.unwrap().len(), 1);
    // A keyword expands its values, unanalyzed.
    let hits = views.top_k(
        &plan(&fuzzy(f.tag, "rod", 1)).unwrap(),
        10,
        Pruning::default(),
    );
    assert_eq!(hits.unwrap().len(), 1);
    // In a `bool`, beside a term.
    let both = Query::Bool(BoolQuery {
        must: vec![prefix(f.body, "sho"), fuzzy(f.body, "runing", 1)],
        ..BoolQuery::default()
    });
    assert_eq!(
        views
            .top_k(&plan(&both).unwrap(), 10, Pruning::default())
            .unwrap()
            .len(),
        1
    );

    assert!(matches!(
        SearchPlan::new(&both, c.schema(), &stats),
        Err(SearchError::NeedsTerms { what: "prefix" })
    ));
    assert!(matches!(
        SearchPlan::new(&fuzzy(f.body, "run", 1), c.schema(), &stats),
        Err(SearchError::NeedsTerms { what: "fuzzy" })
    ));
    assert!(matches!(
        plan(&prefix(f.body, "")),
        Err(SearchError::Query(QueryError::EmptyPrefix { .. }))
    ));
    for distance in [0, 3] {
        assert!(matches!(
            plan(&fuzzy(f.body, "run", distance)),
            Err(SearchError::Query(QueryError::InvalidDistance { .. }))
        ));
    }
    assert!(matches!(
        plan(&prefix(f.price, "1")),
        Err(SearchError::Score(ScoreError::Field { .. }))
    ));
}

fn ranked(db: &Database<SimFile>, query: &Query) -> Vec<(String, f64)> {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    expanded_hits(c, &memtable, query, 100)
        .into_iter()
        .map(|h| {
            let id = match h.location {
                Located::Memtable(docid) => memtable.document(docid).unwrap().id().to_owned(),
                Located::Segment { index, docid } => c.segments()[index].id_of(docid).unwrap(),
            };
            (id, h.score)
        })
        .collect()
}

fn open_body(text: TextOptions) -> (Database<SimFile>, FieldId) {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(text))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(7),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [9; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    (db, body)
}

fn put_body(db: &mut Database<SimFile>, body: FieldId, id: &str, text: &str) {
    let doc = Document::new(id).with(body, Value::String(text.into()));
    db.put("c", &doc, Durability::Fsync).unwrap();
}

/// Invariant 4. `b = 0` and one token make a `tf` of 1 score the shared `idf`, so the numbers are the formula.
/// The rare documents are committed first: a tie would rank them ahead of the memtable.
#[test]
fn a_rare_spelling_does_not_outrank_the_common_term() {
    let (mut db, body) = open_body(TextOptions {
        b: 0.0,
        ..TextOptions::default()
    });
    put_body(&mut db, body, "typo", "appel");
    put_body(&mut db, body, "rare-prefix", "runzqx");
    db.commit().unwrap();
    for i in 0..8 {
        put_body(&mut db, body, &format!("apple{i}"), "apple");
        put_body(&mut db, body, &format!("run{i}"), "running");
    }
    put_body(&mut db, body, "run-twice", "running running");

    let fuzzy = ranked(
        &db,
        &Query::Fuzzy(FuzzyQuery {
            field: body,
            term: "apple".into(),
            distance: 1,
            max_expansions: 10,
        }),
    );
    let typo = fuzzy.iter().find(|h| h.0 == "typo").unwrap().1;
    let apple = fuzzy.iter().find(|h| h.0 == "apple0").unwrap().1;
    // 2 committed documents, then 8 `apple`, 8 `running` and one `running running`.
    let n = 19;
    let shared = idf(n, 8);
    assert!(
        idf(n, 1) > shared,
        "each term's own idf would rank the misspelling first"
    );
    let at = |tf, edit: f64| {
        let raw = bm25(shared, 1.2, 1.0, 0.0, 1.0, tf, 0);
        if edit.to_bits() == 1.0_f64.to_bits() {
            raw
        } else {
            raw * edit
        }
    };
    assert_eq!(apple.to_bits(), at(1, 1.0).to_bits());
    assert_eq!(typo.to_bits(), at(1, 0.5).to_bits());
    assert!(apple > typo);
    assert_eq!(fuzzy[0].0, "apple0");

    let prefix = ranked(
        &db,
        &Query::Prefix(PrefixQuery {
            field: body,
            prefix: "run".into(),
            max_expansions: 10,
        }),
    );
    let rare = prefix.iter().find(|h| h.0 == "rare-prefix").unwrap().1;
    let once = prefix.iter().find(|h| h.0 == "run0").unwrap().1;
    let twice = prefix.iter().find(|h| h.0 == "run-twice").unwrap().1;
    let shared = idf(n, 9);
    assert!(
        idf(n, 1) > shared,
        "each term's own idf would boost the rare completion"
    );
    let at = |tf| bm25(shared, 1.2, 1.0, 0.0, 1.0, tf, 0);
    assert_eq!(rare.to_bits(), once.to_bits());
    assert_eq!(once.to_bits(), at(1).to_bits());
    assert_eq!(twice.to_bits(), at(2).to_bits());
    assert!(twice > once);
}

/// Invariant 5. `standard` lowercases at index time; the query bytes are used as given.
#[test]
fn prefix_and_fuzzy_match_raw_bytes() {
    let (mut db, f) = database();
    put(&mut db, &f, "a", "Running shoes", "Red");
    let prefix = |field, p: &str| {
        Query::Prefix(PrefixQuery {
            field,
            prefix: p.into(),
            max_expansions: 10,
        })
    };
    let fuzzy = |field, t: &str, distance| {
        Query::Fuzzy(FuzzyQuery {
            field,
            term: t.into(),
            distance,
            max_expansions: 10,
        })
    };
    assert_eq!(found(&db, &prefix(f.body, "Run")), Vec::<String>::new());
    assert_eq!(found(&db, &prefix(f.body, "run")), ["a"]);
    assert_eq!(
        found(&db, &fuzzy(f.body, "RUNNING", 2)),
        Vec::<String>::new()
    );
    // One substitution of the raw bytes, which the analyzer did not lowercase.
    assert_eq!(found(&db, &fuzzy(f.body, "Running", 1)), ["a"]);
    assert_eq!(found(&db, &fuzzy(f.body, "running", 1)), ["a"]);
    assert_eq!(found(&db, &prefix(f.tag, "red")), Vec::<String>::new());
    assert_eq!(found(&db, &prefix(f.tag, "Red")), ["a"]);
}
