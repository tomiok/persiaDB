//! `bool` filters over a collection's segments and memtable (ROADMAP 7.4.5; SPEC §7.1, §7.1.2, §7.3).
//!
//! Invariants:
//! 1. A filter selects exactly the live documents the reference (`reference::filter`, over the documents' values)
//!    selects, in segments and the memtable alike, deleted and hidden copies never.
//! 2. A filter scores 0: a filtered query scores each document it matches as the same query without the filter.
//! 3. With a filter, `should` clauses are optional (SPEC §7.1): every selected document matches.
//! 4. Pruned and exhaustive top-k are the same hits, scores bit for bit (SPEC §7.3).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated or fixed.

mod reference;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::ops::Bound;
use std::sync::{Arc, LazyLock};

use persia_engine::db::{
    CollectionHit, CollectionSnapshot, Database, DatabaseOptions, Located, ManualClock, SegmentIds,
};
use persia_engine::document::{Document, MAX_DATETIME, MIN_DATETIME, Value};
use persia_engine::memtable::MemtableView;
use persia_engine::query::{BoolQuery, Filter, FilterValue, Query, QueryLimits, TermQuery};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::search::{Pruning, SearchError, SearchPlan};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestCaseError, TestRunner};
use reference::filter::selects;

const WORDS: [&str; 3] = ["alpha", "beta", "common"];
const TAGS: [&str; 3] = ["a", "b", "c"];
const NS: [i64; 6] = [i64::MIN, -1, 0, 1, 2, i64::MAX];
/// Document values; `-0.0` is stored as `0.0`.
const XS: [f64; 5] = [-1.5, -0.0, 0.0, 0.5, f64::MAX];
/// Filter values: the document values and the infinities.
const FILTER_XS: [f64; 7] = [
    f64::NEG_INFINITY,
    -1.5,
    -0.0,
    0.0,
    0.5,
    f64::MAX,
    f64::INFINITY,
];
const ATS: [i64; 4] = [MIN_DATETIME, 0, 1, MAX_DATETIME];
const IDS: u8 = 8;
const PROPTEST_SEED: u64 = 0x7405_0000_0001;
const STRATEGIES: [Pruning; 3] = [
    Pruning::Exhaustive,
    Pruning::MaxScore,
    Pruning::BlockMaxWand,
];

struct Fields {
    body: FieldId,
    tag: FieldId,
    n: FieldId,
    x: FieldId,
    at: FieldId,
    flag: FieldId,
}

static SCHEMA: LazyLock<CollectionSchema> = LazyLock::new(|| {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .field("n", FieldKind::I64 { stored: true })
        .field("x", FieldKind::F64 { stored: false })
        .field("at", FieldKind::Datetime { stored: true })
        .field("flag", FieldKind::Bool { stored: false })
        .build()
        .unwrap()
});

static FIELDS: LazyLock<Fields> = LazyLock::new(|| {
    let id = |name: &str| SCHEMA.field(name).unwrap().id();
    Fields {
        body: id("body"),
        tag: id("tag"),
        n: id("n"),
        x: id("x"),
        at: id("at"),
        flag: id("flag"),
    }
});

fn database() -> Database<SimFile> {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [5; 16], options).unwrap();
    db.create_collection(SCHEMA.clone()).unwrap();
    db
}

/// A document's values, each optional, by index into the value tables.
#[derive(Debug, Clone)]
struct Values {
    /// `Some(empty)`: a body without a token.
    words: Option<Vec<u8>>,
    tags: Vec<u8>,
    n: Option<u8>,
    x: Option<u8>,
    at: Option<u8>,
    flag: Option<bool>,
}

impl Values {
    fn document(&self, id: &str) -> Document {
        let f = &*FIELDS;
        let mut doc = Document::new(id);
        let mut set = |field, value| drop(doc.insert(field, value));
        if let Some(words) = &self.words {
            let body: Vec<_> = words.iter().map(|&w| WORDS[w as usize]).collect();
            set(f.body, Value::String(body.join(" ")));
        }
        match self.tags.as_slice() {
            [] => {}
            [one] => set(f.tag, Value::String(TAGS[*one as usize].into())),
            many => set(
                f.tag,
                Value::Strings(many.iter().map(|&t| TAGS[t as usize].into()).collect()),
            ),
        }
        if let Some(n) = self.n {
            set(f.n, Value::I64(NS[n as usize]));
        }
        if let Some(x) = self.x {
            set(f.x, Value::F64(XS[x as usize]));
        }
        if let Some(at) = self.at {
            set(f.at, Value::Datetime(ATS[at as usize]));
        }
        if let Some(flag) = self.flag {
            set(f.flag, Value::Bool(flag));
        }
        doc
    }

    fn holds(&self, word: &str) -> bool {
        self.words
            .as_ref()
            .is_some_and(|w| w.iter().any(|&i| WORDS[i as usize] == word))
    }
}

fn values() -> impl Strategy<Value = Values> {
    (
        prop::option::of(prop::collection::vec(0..WORDS.len() as u8, 0..4)),
        prop::collection::vec(0..TAGS.len() as u8, 0..3),
        prop::option::of(0..NS.len() as u8),
        prop::option::of(0..XS.len() as u8),
        prop::option::of(0..ATS.len() as u8),
        prop::option::of(any::<bool>()),
    )
        .prop_map(|(words, tags, n, x, at, flag)| Values {
            words,
            tags,
            n,
            x,
            at,
            flag,
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
        1 => Just(Op::Commit),
    ]
}

fn bound(kind: u8, value: FilterValue) -> Bound<FilterValue> {
    match kind {
        0 => Bound::Included(value),
        1 => Bound::Excluded(value),
        _ => Bound::Unbounded,
    }
}

/// A range on `field`, or `exists` when the bounds make no valid range (none, or empty).
fn range(field: FieldId, lower: Bound<FilterValue>, upper: Bound<FilterValue>) -> Filter {
    let range = Filter::Range {
        field,
        lower,
        upper,
    };
    if range.validate(&SCHEMA, &QueryLimits::default()).is_ok() {
        range
    } else {
        Filter::Exists { field }
    }
}

fn leaf() -> impl Strategy<Value = Filter> {
    let f = &*FIELDS;
    let (tag, n, x, at, flag) = (f.tag, f.n, f.x, f.at, f.flag);
    let keyword = || (0..TAGS.len()).prop_map(|t| FilterValue::Keyword(TAGS[t].into()));
    let int = || (0..NS.len()).prop_map(|i| FilterValue::I64(NS[i]));
    let float = || (0..FILTER_XS.len()).prop_map(|i| FilterValue::F64(FILTER_XS[i]));
    let datetime = || (0..ATS.len()).prop_map(|i| FilterValue::Datetime(ATS[i]));
    let fields = [f.body, f.tag, f.n, f.x, f.at, f.flag];
    prop_oneof![
        keyword().prop_map(move |value| Filter::Term { field: tag, value }),
        prop::collection::vec(keyword(), 0..3)
            .prop_map(move |values| Filter::Terms { field: tag, values }),
        int().prop_map(move |value| Filter::Term { field: n, value }),
        prop::collection::vec(int(), 0..3)
            .prop_map(move |values| Filter::Terms { field: n, values }),
        float().prop_map(move |value| Filter::Term { field: x, value }),
        datetime().prop_map(move |value| Filter::Term { field: at, value }),
        any::<bool>().prop_map(move |b| Filter::Term {
            field: flag,
            value: FilterValue::Bool(b)
        }),
        (0..3_u8, int(), 0..3_u8, int()).prop_map(move |(lk, lo, hk, hi)| range(
            n,
            bound(lk, lo),
            bound(hk, hi)
        )),
        (0..3_u8, float(), 0..3_u8, float()).prop_map(move |(lk, lo, hk, hi)| range(
            x,
            bound(lk, lo),
            bound(hk, hi)
        )),
        (0..3_u8, datetime(), 0..3_u8, datetime()).prop_map(move |(lk, lo, hk, hi)| range(
            at,
            bound(lk, lo),
            bound(hk, hi)
        )),
        (0..fields.len()).prop_map(move |i| Filter::Exists { field: fields[i] }),
    ]
}

fn filter() -> impl Strategy<Value = Filter> {
    leaf().prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Filter::And),
            prop::collection::vec(inner.clone(), 0..3).prop_map(Filter::Or),
            inner.prop_map(|f| Filter::Not(Box::new(f))),
        ]
    })
}

fn term(word: &str) -> Query {
    Query::Term(TermQuery {
        field: FIELDS.body,
        term: word.into(),
    })
}

fn with_filter(q: BoolQuery, filter: &Filter) -> Query {
    Query::Bool(BoolQuery {
        filter: vec![filter.clone()],
        ..q
    })
}

/// The best `k` hits of `query`, the same under every pruning strategy.
fn search(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    query: &Query,
    k: usize,
) -> Vec<CollectionHit> {
    let plan = SearchPlan::new(query, c.schema(), &c.search_statistics(memtable)).unwrap();
    let views = c.views(memtable).unwrap();
    let bits = |hits: &[CollectionHit]| -> Vec<(Located, u64)> {
        hits.iter()
            .map(|h| (h.location, h.score.to_bits()))
            .collect()
    };
    let exhaustive = views.top_k(&plan, k, Pruning::Exhaustive).unwrap();
    for pruning in STRATEGIES {
        let hits = views.top_k(&plan, k, pruning).unwrap();
        assert_eq!(bits(&hits), bits(&exhaustive), "{pruning:?}, k = {k}");
    }
    exhaustive
}

/// Each hit's id and score bits, by id; an id twice fails.
fn scored(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    hits: &[CollectionHit],
) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for h in hits {
        let id = match h.location {
            Located::Memtable(docid) => memtable.document(docid).unwrap().id().to_owned(),
            Located::Segment { index, docid } => c.segments()[index].id_of(docid).unwrap(),
        };
        assert!(out.insert(id, h.score.to_bits()).is_none(), "an id twice");
    }
    out
}

/// Checks invariants 1–4 for `filter` against `model` (each live id and its values).
fn check(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    model: &BTreeMap<String, Values>,
    filter: &Filter,
) -> Result<(), TestCaseError> {
    let all = 1000;
    let selected: BTreeSet<&String> = model
        .iter()
        .filter(|(id, v)| selects(filter, &v.document(id), &SCHEMA))
        .map(|(id, _)| id)
        .collect();
    let with = |word: &str, set: &BTreeSet<&String>| -> BTreeSet<String> {
        set.iter()
            .filter(|id| model[id.as_str()].holds(word))
            .map(|id| (*id).clone())
            .collect()
    };
    let owned = |set: &BTreeSet<&String>| -> BTreeSet<String> {
        set.iter().map(|s| (*s).clone()).collect()
    };

    // Only filters.
    let only = scored(
        c,
        memtable,
        &search(c, memtable, &with_filter(BoolQuery::default(), filter), all),
    );
    prop_assert_eq!(
        only.keys().cloned().collect::<BTreeSet<_>>(),
        owned(&selected),
        "{:?}",
        filter
    );
    prop_assert!(only.values().all(|&s| s == 0.0_f64.to_bits()));

    // `must` with filters: the `must` scores, filtered.
    let must = BoolQuery {
        must: vec![term("common")],
        ..BoolQuery::default()
    };
    let unfiltered = scored(
        c,
        memtable,
        &search(c, memtable, &Query::Bool(must.clone()), all),
    );
    let filtered_q = with_filter(must, filter);
    let filtered = scored(c, memtable, &search(c, memtable, &filtered_q, all));
    prop_assert_eq!(
        filtered.keys().cloned().collect::<BTreeSet<_>>(),
        with("common", &selected)
    );
    for (id, s) in &filtered {
        prop_assert_eq!(*s, unfiltered[id], "{}", id);
    }

    // `should` with filters: optional, scored where they match.
    let should = BoolQuery {
        should: vec![term("alpha"), term("beta")],
        ..BoolQuery::default()
    };
    let unfiltered = scored(
        c,
        memtable,
        &search(c, memtable, &Query::Bool(should.clone()), all),
    );
    let should_q = with_filter(should, filter);
    let hits = scored(c, memtable, &search(c, memtable, &should_q, all));
    prop_assert_eq!(
        hits.keys().cloned().collect::<BTreeSet<_>>(),
        owned(&selected)
    );
    for (id, s) in &hits {
        let want = unfiltered.get(id).copied().unwrap_or(0.0_f64.to_bits());
        prop_assert_eq!(*s, want, "{}", id);
    }

    // `must_not` with filters.
    let not = with_filter(
        BoolQuery {
            must_not: vec![term("alpha")],
            ..BoolQuery::default()
        },
        filter,
    );
    let hits = scored(c, memtable, &search(c, memtable, &not, all));
    let alpha = with("alpha", &selected);
    let want: BTreeSet<String> = owned(&selected)
        .into_iter()
        .filter(|id| !alpha.contains(id))
        .collect();
    prop_assert_eq!(hits.keys().cloned().collect::<BTreeSet<_>>(), want);

    // A filtered `bool` as a clause of a pruned disjunction: its bounds are what the disjunction skips by.
    let nested = Query::Bool(BoolQuery {
        should: vec![filtered_q.clone(), term("beta")],
        ..BoolQuery::default()
    });
    let hits = scored(c, memtable, &search(c, memtable, &nested, all));
    let mut want = with("common", &selected);
    want.extend(
        model
            .iter()
            .filter(|(_, v)| v.holds("beta"))
            .map(|(id, _)| id.clone()),
    );
    prop_assert_eq!(hits.keys().cloned().collect::<BTreeSet<_>>(), want);

    // Top-k is a prefix of the full ranking.
    for query in [&filtered_q, &should_q, &nested] {
        let full = search(c, memtable, query, all);
        for k in [1, 3] {
            prop_assert_eq!(
                &search(c, memtable, query, k)[..],
                &full[..k.min(full.len())]
            );
        }
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

/// After every operation (puts and upserts, deletes, commits that flush the memtable into segments), every filter
/// selects what the reference does, in every query shape, with unchanged scores and the same hits under pruning.
#[test]
fn filters_select_what_the_reference_selects() {
    let cases = if cfg!(feature = "slow") { 256 } else { 32 };
    run(
        "filters_select_what_the_reference_selects",
        cases,
        (
            prop::collection::vec(op(), 1..24),
            prop::collection::vec(filter(), 1..4),
        ),
        |(ops, filters)| {
            let mut db = database();
            let mut model = BTreeMap::<String, Values>::new();
            for op in &ops {
                match op {
                    Op::Put(id, v) => {
                        let id = format!("d{id}");
                        db.put("c", &v.document(&id), Durability::Buffered).unwrap();
                        model.insert(id, v.clone());
                    }
                    Op::Delete(id) => {
                        let id = format!("d{id}");
                        db.delete("c", &id, Durability::Buffered).unwrap();
                        model.remove(&id);
                    }
                    Op::Commit => drop(db.commit().unwrap()),
                }
                let snapshot = db.snapshot();
                let c = snapshot.collection("c").unwrap();
                let memtable = c.memtable();
                for filter in &filters {
                    check(c, &memtable, &model, filter)?;
                }
            }
            Ok(())
        },
    );
}

/// Edge values: exclusive bounds at the ends of the key space, `-0.0`, the infinities, and the empty `terms`,
/// `and` and `or`.
#[test]
fn edge_filters() {
    use Bound::{Excluded, Included, Unbounded};
    let f = &*FIELDS;
    let mut db = database();
    let mut model = BTreeMap::new();
    for (i, (n, x)) in [(0_u8, 0_u8), (5, 1), (2, 2), (3, 4)]
        .into_iter()
        .enumerate()
    {
        let v = Values {
            words: Some(vec![2]),
            tags: vec![],
            n: Some(n),
            x: Some(x),
            at: None,
            // In every document: the committed segment's columns have no presence bitmap (SPEC §5.3.5).
            flag: Some(i % 2 == 0),
        };
        let id = format!("d{i}");
        db.put("c", &v.document(&id), Durability::Buffered).unwrap();
        model.insert(id, v);
        if i == 1 {
            db.commit().unwrap();
        }
    }
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let ids = |filter: &Filter| -> Vec<String> {
        let q = with_filter(BoolQuery::default(), filter);
        scored(c, &memtable, &search(c, &memtable, &q, 100))
            .into_keys()
            .collect()
    };
    let i64s = |lower, upper| Filter::Range {
        field: f.n,
        lower,
        upper,
    };
    let f64s = |lower, upper| Filter::Range {
        field: f.x,
        lower,
        upper,
    };
    let int = FilterValue::I64;
    let float = FilterValue::F64;
    assert_eq!(
        ids(&i64s(Excluded(int(i64::MAX)), Unbounded)),
        Vec::<String>::new()
    );
    assert_eq!(
        ids(&i64s(Unbounded, Excluded(int(i64::MIN)))),
        Vec::<String>::new()
    );
    assert_eq!(ids(&i64s(Included(int(i64::MAX)), Unbounded)), ["d1"]);
    assert_eq!(ids(&i64s(Unbounded, Included(int(i64::MIN)))), ["d0"]);
    // `-0.0` is `0.0`: the stored `-0.0` (d1) and `0.0` (d2) both hold it, and `gt -0.0` holds neither.
    assert_eq!(
        ids(&Filter::Term {
            field: f.x,
            value: float(-0.0)
        }),
        ["d1", "d2"]
    );
    assert_eq!(ids(&f64s(Excluded(float(-0.0)), Unbounded)), ["d3"]);
    assert_eq!(ids(&f64s(Unbounded, Excluded(float(0.0)))), ["d0"]);
    assert_eq!(
        ids(&f64s(
            Excluded(float(f64::NEG_INFINITY)),
            Excluded(float(f64::INFINITY))
        ))
        .len(),
        4
    );
    assert_eq!(
        ids(&f64s(Excluded(float(f64::MAX)), Unbounded)),
        Vec::<String>::new()
    );
    assert_eq!(
        ids(&Filter::Terms {
            field: f.n,
            values: vec![]
        }),
        Vec::<String>::new()
    );
    assert_eq!(ids(&Filter::And(vec![])).len(), 4);
    assert_eq!(ids(&Filter::Or(vec![])), Vec::<String>::new());
    for (id, v) in &model {
        assert!(selects(&Filter::And(vec![]), &v.document(id), &SCHEMA));
    }
    // Full columns, in the segment (d0, d1) and the memtable (d2, d3).
    let flag = |b| FilterValue::Bool(b);
    let term = |field, value| Filter::Term { field, value };
    assert_eq!(ids(&term(f.flag, flag(true))), ["d0", "d2"]);
    assert_eq!(ids(&term(f.flag, flag(false))), ["d1", "d3"]);
    let both = Filter::Terms {
        field: f.flag,
        values: vec![flag(false), flag(true)],
    };
    assert_eq!(ids(&both).len(), 4);
    assert_eq!(ids(&Filter::Not(Box::new(both))), Vec::<String>::new());
    assert_eq!(ids(&Filter::Exists { field: f.flag }).len(), 4);
    assert_eq!(
        ids(&Filter::Not(Box::new(Filter::Exists { field: f.n }))),
        Vec::<String>::new()
    );
    assert_eq!(ids(&term(f.n, int(0))), ["d2"]);
    assert_eq!(
        ids(&i64s(Excluded(int(-1)), Included(int(1)))),
        ["d2", "d3"]
    );
    for (id, v) in &model {
        for filter in [term(f.flag, flag(true)), term(f.n, int(0))] {
            let want = ids(&filter).contains(id);
            assert_eq!(selects(&filter, &v.document(id), &SCHEMA), want, "{id}");
        }
    }
}

/// `not` and `exists` never bring back what a view leaves out: a document replaced or deleted in the memtable, a
/// tombstoned one, nor the segment copy of an id the memtable has written since the commit.
#[test]
fn complements_never_select_deleted_documents() {
    let f = &*FIELDS;
    let v = |n: u8| Values {
        words: Some(vec![2]),
        tags: vec![0],
        n: Some(n),
        x: None,
        at: None,
        flag: None,
    };
    let mut db = database();
    for id in ["kept", "upserted", "deleted", "tombstoned"] {
        db.put("c", &v(1).document(id), Durability::Buffered)
            .unwrap();
    }
    db.delete("c", "tombstoned", Durability::Buffered).unwrap();
    db.commit().unwrap();
    // Hidden copies: neither has a committed tombstone yet.
    db.put("c", &v(2).document("upserted"), Durability::Buffered)
        .unwrap();
    db.delete("c", "deleted", Durability::Buffered).unwrap();
    // Replaced within the memtable.
    db.put("c", &v(1).document("fresh"), Durability::Buffered)
        .unwrap();
    db.put("c", &v(2).document("fresh"), Durability::Buffered)
        .unwrap();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let ids = |filter: Filter| -> Vec<String> {
        let q = with_filter(BoolQuery::default(), &filter);
        scored(c, &memtable, &search(c, &memtable, &q, 100))
            .into_keys()
            .collect()
    };
    let one = || Filter::Term {
        field: f.n,
        value: FilterValue::I64(NS[1]),
    };
    assert_eq!(ids(Filter::Not(Box::new(one()))), ["fresh", "upserted"]);
    assert_eq!(ids(one()), ["kept"]);
    assert_eq!(
        ids(Filter::Exists { field: f.tag }),
        ["fresh", "kept", "upserted"]
    );
    assert_eq!(
        ids(Filter::Not(Box::new(Filter::Exists { field: f.x }))),
        ["fresh", "kept", "upserted"]
    );
}

/// Postings of several blocks in three segments, with upserts and deletes in the memtable: a filtered `must`, and
/// `should` clauses beside a filter with no `must`, prune under thresholds and return the exhaustive hits, which
/// are the reference's.
#[test]
fn multi_block_filtered_search_prunes_to_the_exhaustive_hits() {
    let seed: u64 = std::env::var("PERSIA_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x7405);
    let mut state = seed;
    let mut next = move |n: u64| {
        // SplitMix64: deterministic, seeded.
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % n
    };
    let random = |next: &mut dyn FnMut(u64) -> u64| Values {
        words: Some(
            (0..=next(5))
                .map(|_| next(WORDS.len() as u64) as u8)
                .collect(),
        ),
        tags: vec![next(TAGS.len() as u64) as u8],
        n: Some(next(NS.len() as u64) as u8),
        x: (next(4) > 0).then(|| next(XS.len() as u64) as u8),
        at: None,
        flag: Some(next(2) == 0),
    };
    let mut db = database();
    let mut model = BTreeMap::new();
    for segment in 0..3 {
        for i in 0..300 {
            let id = format!("s{segment}-{i}");
            let v = random(&mut next);
            db.put("c", &v.document(&id), Durability::Buffered).unwrap();
            model.insert(id, v);
        }
        db.commit().unwrap();
    }
    for _ in 0..150 {
        let id = format!("s{}-{}", next(3), next(300));
        if next(3) == 0 {
            db.delete("c", &id, Durability::Buffered).unwrap();
            model.remove(&id);
        } else {
            let v = random(&mut next);
            db.put("c", &v.document(&id), Durability::Buffered).unwrap();
            model.insert(id, v);
        }
    }
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    assert_eq!(c.segments().len(), 3);
    let memtable = c.memtable();
    let f = &*FIELDS;
    let filters = [
        Filter::Range {
            field: f.n,
            lower: Bound::Included(FilterValue::I64(0)),
            upper: Bound::Unbounded,
        },
        Filter::Or(vec![
            Filter::Term {
                field: f.tag,
                value: FilterValue::Keyword("a".into()),
            },
            Filter::Not(Box::new(Filter::Exists { field: f.x })),
        ]),
        Filter::Term {
            field: f.flag,
            value: FilterValue::Bool(true),
        },
    ];
    for filter in &filters {
        check(c, &memtable, &model, filter).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let q = with_filter(
            BoolQuery {
                must: vec![term("common")],
                ..BoolQuery::default()
            },
            filter,
        );
        let full = search(c, &memtable, &q, 2000);
        for k in [1, 10, 50] {
            assert_eq!(
                search(c, &memtable, &q, k),
                full[..k.min(full.len())],
                "seed {seed}, k = {k}"
            );
        }
    }
    // `should` beside a filter, no `must`: the filter scores 0, so the threshold reaches the disjunction.
    // `search` checks every pruning strategy against exhaustive top-k.
    let should_q = with_filter(
        BoolQuery {
            should: vec![term("alpha"), term("beta"), term("common")],
            ..BoolQuery::default()
        },
        &filters[0],
    );
    let full = search(c, &memtable, &should_q, 2000);
    for k in [1_usize, 10, 50] {
        assert_eq!(
            search(c, &memtable, &should_q, k),
            full[..k.min(full.len())],
            "should beside a filter, seed {seed}, k = {k}"
        );
    }
}

/// A filter whose value has another type than its field is rejected when planned, not run with the wrong keys.
#[test]
fn plans_reject_filters_that_do_not_fit_the_schema() {
    let f = &*FIELDS;
    let db = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let stats = c.search_statistics(&memtable);
    for filter in [
        Filter::Term {
            field: f.x,
            value: FilterValue::I64(1),
        },
        Filter::Range {
            field: f.tag,
            lower: Bound::Included(FilterValue::Keyword("a".into())),
            upper: Bound::Unbounded,
        },
    ] {
        let q = with_filter(BoolQuery::default(), &filter);
        assert!(
            matches!(
                SearchPlan::new(&q, c.schema(), &stats),
                Err(SearchError::Query(_))
            ),
            "{filter:?}"
        );
    }
    // Over the limits, still planned: limits are `Query::validate`'s.
    let wide = Filter::Terms {
        field: f.n,
        values: (0..2000).map(FilterValue::I64).collect(),
    };
    assert!(
        SearchPlan::new(
            &with_filter(BoolQuery::default(), &wide),
            c.schema(),
            &stats
        )
        .is_ok()
    );
    assert_eq!(memtable.doc_count(), 0);
}

/// A memtable view taken before more writes (upserts, deletes, new documents) filters as it did when taken: the new
/// methods read only what its `(docs, version)` cutoff covers, and a newer view sees the writes.
#[test]
fn older_memtable_views_filter_as_when_taken() {
    let f = &*FIELDS;
    let v = |n: u8, flag: bool| Values {
        words: Some(vec![2]),
        tags: vec![0],
        n: Some(n),
        x: None,
        at: None,
        flag: Some(flag),
    };
    let mut db = database();
    let mut model = BTreeMap::new();
    for (i, id) in ["a", "b", "c"].into_iter().enumerate() {
        let values = v(u8::try_from(i).unwrap(), i == 0);
        db.put("c", &values.document(id), Durability::Buffered)
            .unwrap();
        model.insert(id.to_owned(), values);
    }
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let old = c.memtable();
    let filters = [
        Filter::Term {
            field: f.flag,
            value: FilterValue::Bool(true),
        },
        Filter::Not(Box::new(Filter::Term {
            field: f.flag,
            value: FilterValue::Bool(true),
        })),
        Filter::Exists { field: f.tag },
        Filter::Range {
            field: f.n,
            lower: Bound::Included(FilterValue::I64(NS[1])),
            upper: Bound::Unbounded,
        },
    ];
    let before: Vec<_> = filters
        .iter()
        .map(|filter| {
            check(c, &old, &model, filter).unwrap();
            let q = with_filter(BoolQuery::default(), filter);
            search(c, &old, &q, 100)
        })
        .collect();
    let deleted = old.deleted();
    let present = old.present(f.tag).unwrap();
    // Writes after the view: an upsert that flips `flag`, a delete, a new document.
    db.put("c", &v(5, true).document("b"), Durability::Buffered)
        .unwrap();
    db.delete("c", "a", Durability::Buffered).unwrap();
    db.put("c", &v(4, true).document("d"), Durability::Buffered)
        .unwrap();
    for (filter, hits) in filters.iter().zip(&before) {
        check(c, &old, &model, filter).unwrap();
        let q = with_filter(BoolQuery::default(), filter);
        assert_eq!(&search(c, &old, &q, 100), hits, "{filter:?}");
    }
    assert_eq!(old.deleted(), deleted);
    assert_eq!(old.present(f.tag).unwrap(), present);
    let mut keys = Vec::new();
    old.column_keys(f.flag, |d, k| keys.push((d, k))).unwrap();
    assert_eq!(keys, [(0, 1), (1, 0), (2, 0)]);
    assert_eq!(old.trues(f.flag).unwrap().iter().collect::<Vec<_>>(), [0]);
    // A newer view sees the writes.
    let newer = db.snapshot();
    let c = newer.collection("c").unwrap();
    model.remove("a");
    model.insert("b".to_owned(), v(5, true));
    model.insert("d".to_owned(), v(4, true));
    for filter in &filters {
        check(c, &c.memtable(), &model, filter).unwrap();
    }
}
