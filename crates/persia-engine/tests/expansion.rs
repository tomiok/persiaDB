//! `prefix` and `fuzzy` planned over a collection's terms (ROADMAP 7.4.12; SPEC §7.1).
//!
//! Invariants:
//! 1. An expansion is the `bool` of `should` `term`s over the terms of every view that it takes (a prefix, or the
//!    optimal string alignment distance over characters), at most `max_expansions` of them: the highest collection
//!    `df` first, then bytes. Its hits are that `bool`'s, scores bit for bit, under every pruning strategy;
//!    differentially against such a `bool` built from each view's term list.
//! 2. The cap ranks by the collection `df`, not a view's own.
//! 3. An expansion that keeps no term matches nothing; `SearchPlan::new` refuses to expand; an empty prefix, a
//!    distance other than 1 or 2, and a field that is not `text` or `keyword` are errors.

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

use std::collections::BTreeSet;
use std::sync::Arc;

use fst::automaton::AlwaysMatch;
use persia_engine::db::{
    CollectionHit, CollectionSnapshot, Database, DatabaseOptions, Located, ManualClock, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::MemtableView;
use persia_engine::query::{BoolQuery, FuzzyQuery, PrefixQuery, Query, QueryError, TermQuery};
use persia_engine::schema::{FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
use persia_engine::score::ScoreError;
use persia_engine::search::{Pruning, SearchError, SearchPlan};
use persia_engine::stats::StatisticsProvider;
use persia_engine::view::{SearchView, TermEnum};
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

/// What SPEC §7.1 expands `query` to: a `bool` of `should` `term`s, or `None` for no term.
fn expected(c: &CollectionSnapshot, memtable: &MemtableView, query: &Query) -> Option<Query> {
    let (field, cap, takes): (FieldId, u32, Box<dyn Fn(&str) -> bool>) = match query {
        Query::Prefix(q) => {
            let p = q.prefix.clone();
            (
                q.field,
                q.max_expansions,
                Box::new(move |t| t.starts_with(&p)),
            )
        }
        Query::Fuzzy(q) => {
            let (term, d) = (q.term.clone(), usize::from(q.distance));
            (
                q.field,
                q.max_expansions,
                Box::new(move |t| osa(&term, t) <= d),
            )
        }
        _ => unreachable!(),
    };
    let stats = c.search_statistics(memtable);
    let mut ranked: Vec<(u64, String)> = all_terms(c, memtable, field)
        .into_iter()
        .map(|t| String::from_utf8(t).unwrap())
        .filter(|t| takes(t))
        .map(|t| (stats.df(field, t.as_bytes()).unwrap(), t))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked.truncate(usize::try_from(cap).unwrap());
    let mut kept: Vec<String> = ranked.into_iter().map(|(_, t)| t).collect();
    kept.sort();
    (!kept.is_empty()).then(|| {
        Query::Bool(BoolQuery {
            should: kept
                .into_iter()
                .map(|term| Query::Term(TermQuery { field, term }))
                .collect(),
            ..BoolQuery::default()
        })
    })
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
    let want = match expected(c, memtable, query) {
        None => Vec::new(),
        Some(q) => {
            let plan = SearchPlan::new(&q, c.schema(), &c.search_statistics(memtable)).unwrap();
            c.views(memtable)
                .unwrap()
                .top_k(&plan, k, Pruning::Exhaustive)
                .unwrap()
        }
    };
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
    fn expansions_are_the_bool_of_their_terms(
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
