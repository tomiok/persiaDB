//! `bool` filters over a collection's segments and memtable (ROADMAP 7.4.5; SPEC §7.1, §7.1.2, §7.3).
//!
//! Invariants:
//! 1. A filter selects exactly the live documents the reference (`reference::filter`, over the documents' values)
//!    selects, in segments and the memtable alike, deleted and hidden copies never.
//! 2. A filter scores 0: a filtered query scores each document it matches as the same query without the filter.
//! 3. With a filter, `should` clauses are optional (SPEC §7.1): every selected document matches.
//! 4. Pruned and exhaustive top-k are the same hits, scores bit for bit (SPEC §7.3).
//! 5. Several filters, some read from postings and some from column keys, select and score the same at every check
//!    share: a bitmap of all of them, or of the postings ones with the column ones checked per candidate (ROADMAP
//!    7.4.16), and explanations score each hit as the search did.

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
    CollectionHit, CollectionSnapshot, Database, DatabaseOptions, Located, ManualClock,
    SearchRequest, SegmentIds,
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
    // Every live document: the top-k of every query holds every hit.
    let all = model.len().max(1);
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
    let deleted = old.deleted().unwrap();
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
    assert_eq!(old.deleted().unwrap(), deleted);
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

/// Values of document `i` of [`many_blocks`]: each column holds a few values per run of documents, so FOR blocks
/// are narrow, some cut by the filters and some not (ROADMAP 7.4.13); "alpha" is rare, "common" in every document.
fn banded(i: usize) -> Values {
    let mut words = vec![2];
    if i.is_multiple_of(29) {
        words.push(0);
    }
    if i.is_multiple_of(3) {
        words.push(1);
    }
    Values {
        words: Some(words),
        tags: vec![(i % 3) as u8],
        n: (!i.is_multiple_of(11)).then_some(((i / 700) % NS.len()) as u8),
        x: Some(((i / 1100) % XS.len()) as u8),
        at: (!i.is_multiple_of(13)).then_some(((i / 900) % ATS.len()) as u8),
        flag: Some(i.is_multiple_of(2)),
    }
}

/// Two segments of 2 100 documents each (three FOR blocks), banded values, and a memtable that deletes and replaces
/// some of them, some twice.
static MANY_BLOCKS: LazyLock<(Database<SimFile>, BTreeMap<String, Values>)> = LazyLock::new(|| {
    let mut db = database();
    let mut model = BTreeMap::new();
    for segment in 0..2 {
        for i in 0..2_100 {
            let id = format!("s{segment}-{i}");
            let v = banded(i + segment * 1_300);
            db.put("c", &v.document(&id), Durability::Buffered).unwrap();
            model.insert(id, v);
        }
        db.commit().unwrap();
    }
    for i in (0..2_100).step_by(37) {
        let id = format!("s{}-{i}", i % 2);
        if i % 3 == 0 {
            db.delete("c", &id, Durability::Buffered).unwrap();
            model.remove(&id);
        } else {
            let v = banded(i * 7);
            db.put("c", &v.document(&id), Durability::Buffered).unwrap();
            model.insert(id, v);
        }
    }
    // Some ids put twice in the memtable: the first copy is superseded there, as the segments' are.
    for i in (0..2_100).step_by(74) {
        let id = format!("s{}-{i}", i % 2);
        let v = banded(i * 11);
        db.put("c", &v.document(&id), Durability::Buffered).unwrap();
        model.insert(id, v);
    }
    (db, model)
});

/// A rare `must` under `filter`, which views check per candidate when every filter reads only column keys
/// (ROADMAP 7.4.13): the reference's documents, scored as without the filter, and explained as scored.
fn rare(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    model: &BTreeMap<String, Values>,
    filter: &Filter,
) -> Result<(), TestCaseError> {
    let must = BoolQuery {
        must: vec![term("alpha")],
        ..BoolQuery::default()
    };
    let unfiltered = scored(
        c,
        memtable,
        &search(c, memtable, &Query::Bool(must.clone()), 1000),
    );
    let query = with_filter(must, filter);
    let hits = search(c, memtable, &query, 1000);
    let filtered = scored(c, memtable, &hits);
    let want: BTreeSet<String> = model
        .iter()
        .filter(|(id, v)| v.holds("alpha") && selects(filter, &v.document(id), &SCHEMA))
        .map(|(id, _)| id.clone())
        .collect();
    prop_assert_eq!(
        filtered.keys().cloned().collect::<BTreeSet<_>>(),
        want,
        "{:?}",
        filter
    );
    for (id, s) in &filtered {
        prop_assert_eq!(*s, unfiltered[id], "{}", id);
    }
    let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(memtable)).unwrap();
    let views = c.views(memtable).unwrap();
    let request = SearchRequest {
        limit: 100,
        explain: true,
        ..SearchRequest::default()
    };
    let page = views.search(&plan, &request, Pruning::default()).unwrap();
    prop_assert_eq!(page.hits.len(), hits.len().min(100));
    for hit in &page.hits {
        let explained = hit.explanation.as_ref().map(|e| e.score().to_bits());
        prop_assert_eq!(explained, Some(hit.score.to_bits()), "{:?}", hit.location);
    }
    Ok(())
}

/// Invariants 1–4 over segments of several FOR blocks whose keys come in bands, so a scan skips blocks or selects
/// them whole (ROADMAP 7.4.13), with a frequent `must` (the filters' bitmap) and a rare one (checked per candidate).
#[test]
fn filters_over_banded_blocks_select_what_the_reference_selects() {
    let cases = if cfg!(feature = "slow") { 128 } else { 6 };
    let (db, model) = &*MANY_BLOCKS;
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    assert_eq!(c.segments().len(), 2);
    let memtable = c.memtable();
    let f = &*FIELDS;
    // Bands whose edges fall inside FOR blocks and on them, and none.
    let fixed = [
        range(
            f.n,
            Bound::Included(FilterValue::I64(0)),
            Bound::Included(FilterValue::I64(1)),
        ),
        range(f.n, Bound::Excluded(FilterValue::I64(-1)), Bound::Unbounded),
        range(
            f.at,
            Bound::Included(FilterValue::Datetime(0)),
            Bound::Excluded(FilterValue::Datetime(MAX_DATETIME)),
        ),
        Filter::Terms {
            field: f.n,
            values: vec![FilterValue::I64(i64::MAX), FilterValue::I64(-1)],
        },
        Filter::Not(Box::new(range(
            f.x,
            Bound::Unbounded,
            Bound::Included(FilterValue::F64(0.0)),
        ))),
        Filter::And(vec![
            Filter::Exists { field: f.at },
            Filter::Term {
                field: f.flag,
                value: FilterValue::Bool(true),
            },
        ]),
    ];
    for filter in &fixed {
        check(c, &memtable, model, filter).unwrap();
        rare(c, &memtable, model, filter).unwrap();
    }
    run(
        "filters_over_banded_blocks_select_what_the_reference_selects",
        cases,
        filter(),
        |filter| {
            check(c, &memtable, model, &filter)?;
            rare(c, &memtable, model, &filter)
        },
    );
}

/// Each read of [`persia_engine::db::Clock::now`] moves the clock 1 µs forward and is counted: a search reads it once
/// per so many steps of its readers, so the reads measure its work.
#[derive(Debug)]
struct CountingClock {
    start: std::time::Instant,
    reads: std::sync::atomic::AtomicU64,
}

impl persia_engine::db::Clock for CountingClock {
    fn now(&self) -> std::time::Instant {
        let n = self
            .reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.start + std::time::Duration::from_micros(n)
    }

    fn unix_micros(&self) -> i64 {
        0
    }
}

/// The per-candidate path runs (ROADMAP 7.4.13): a rare `must` under column filters reads about what the `must` alone
/// reads, where selecting the filters' bitmap reads the keys of every FOR block they cut.
#[test]
fn a_rare_must_checks_its_filters_per_candidate_without_a_scan() {
    use persia_engine::db::{Execution, Threads};
    let (db, _) = &*MANY_BLOCKS;
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let f = &*FIELDS;
    let reads = |query: &Query| {
        let plan = SearchPlan::new(query, c.schema(), &c.search_statistics(&memtable)).unwrap();
        let clock = Arc::new(CountingClock {
            start: std::time::Instant::now(),
            reads: std::sync::atomic::AtomicU64::new(0),
        });
        let far = clock.start + std::time::Duration::from_secs(3_600);
        let exec = Execution::default()
            .threads(Threads::Current)
            .deadline(clock.clone(), far);
        views.top_k_with(&plan, 10, &exec).unwrap();
        clock.reads.load(std::sync::atomic::Ordering::Relaxed)
    };
    let filters = vec![
        Filter::Terms {
            field: f.n,
            values: vec![FilterValue::I64(0), FilterValue::I64(1)],
        },
        Filter::Not(Box::new(Filter::Term {
            field: f.x,
            value: FilterValue::F64(0.5),
        })),
        Filter::Exists { field: f.at },
    ];
    let must = BoolQuery {
        must: vec![term("alpha")],
        ..BoolQuery::default()
    };
    let alone = reads(&Query::Bool(must.clone()));
    let checked = reads(&Query::Bool(BoolQuery {
        filter: filters.clone(),
        ..must
    }));
    assert!(
        checked <= alone + 1,
        "{checked} reads filtered, {alone} alone"
    );
    // The same filters under a frequent `must` select their bitmaps: the keys of the FOR blocks they cut are read.
    let common = BoolQuery {
        must: vec![term("common")],
        ..BoolQuery::default()
    };
    let alone = reads(&Query::Bool(common.clone()));
    let scanned = reads(&Query::Bool(BoolQuery {
        filter: filters,
        ..common
    }));
    assert!(scanned > alone, "{scanned} reads filtered, {alone} alone");
}

/// The check shares a test runs a plan at (ROADMAP 7.4.16): never check, always check, the two fitted ones (a third
/// alone, a fifth beside postings filters), and one so low only the rarest candidates are checked.
const SHARES: [u64; 5] = [0, 1, 3, 5, 1_000];

/// Invariant 5: a `must` of `word` under every filter of `filters`, planned at every share of [`SHARES`]: the
/// reference's documents, scored as without the filters, the same hits under every pruning strategy and share, and
/// explained as scored.
fn mixed(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    model: &BTreeMap<String, Values>,
    filters: &[Filter],
    word: &str,
) -> Result<(), TestCaseError> {
    let stats = c.search_statistics(memtable);
    let views = c.views(memtable).unwrap();
    let all = model.len().max(1);
    let must = BoolQuery {
        must: vec![term(word)],
        ..BoolQuery::default()
    };
    let unfiltered_plan = SearchPlan::new(&Query::Bool(must.clone()), c.schema(), &stats).unwrap();
    let unfiltered = scored(
        c,
        memtable,
        &views
            .top_k(&unfiltered_plan, all, Pruning::Exhaustive)
            .unwrap(),
    );
    let query = Query::Bool(BoolQuery {
        filter: filters.to_vec(),
        ..must
    });
    let want: BTreeSet<String> = model
        .iter()
        .filter(|(id, v)| {
            v.holds(word) && filters.iter().all(|f| selects(f, &v.document(id), &SCHEMA))
        })
        .map(|(id, _)| id.clone())
        .collect();
    let bits = |hits: &[CollectionHit]| -> Vec<(Located, u64)> {
        hits.iter()
            .map(|h| (h.location, h.score.to_bits()))
            .collect()
    };
    let mut first: Option<Vec<(Located, u64)>> = None;
    for share in SHARES {
        let plan = SearchPlan::new(&query, c.schema(), &stats)
            .unwrap()
            .with_check_share(share);
        let exhaustive = views.top_k(&plan, all, Pruning::Exhaustive).unwrap();
        let hits = scored(c, memtable, &exhaustive);
        prop_assert_eq!(
            hits.keys().cloned().collect::<BTreeSet<_>>(),
            want.clone(),
            "{:?} at share {}",
            filters,
            share
        );
        for (id, s) in &hits {
            prop_assert_eq!(*s, unfiltered[id], "{}", id);
        }
        for pruning in STRATEGIES {
            for k in [1, 3, all] {
                let top = views.top_k(&plan, k, pruning).unwrap();
                prop_assert_eq!(bits(&top), bits(&exhaustive[..k.min(exhaustive.len())]));
            }
        }
        match &first {
            Some(f) => prop_assert_eq!(f, &bits(&exhaustive), "share {}", share),
            None => first = Some(bits(&exhaustive)),
        }
        let request = SearchRequest {
            limit: 100,
            explain: true,
            ..SearchRequest::default()
        };
        let page = views.search(&plan, &request, Pruning::default()).unwrap();
        prop_assert_eq!(page.hits.len(), exhaustive.len().min(100));
        for hit in &page.hits {
            let explained = hit.explanation.as_ref().map(|e| e.score().to_bits());
            prop_assert_eq!(explained, Some(hit.score.to_bits()), "{:?}", hit.location);
        }
        // Nested beside a `should`: a hit of `beta` alone is explained without the filtered `bool`, so the
        // explanation reads the filters as the search did (its kept selection, and its check).
        let nested = Query::Bool(BoolQuery {
            should: vec![query.clone(), term("beta")],
            ..BoolQuery::default()
        });
        let nested_plan = SearchPlan::new(&nested, c.schema(), &stats)
            .unwrap()
            .with_check_share(share);
        let page = views
            .search(&nested_plan, &request, Pruning::default())
            .unwrap();
        let mut found = want.clone();
        found.extend(
            model
                .iter()
                .filter(|(_, v)| v.holds("beta"))
                .map(|(id, _)| id.clone()),
        );
        // Every match, the reference's documents; the page is the first 100 of them, as ranked.
        let every = views.top_k(&nested_plan, all, Pruning::Exhaustive).unwrap();
        prop_assert_eq!(
            scored(c, memtable, &every)
                .into_keys()
                .collect::<BTreeSet<_>>(),
            found.clone(),
            "nested {:?} at share {}",
            filters,
            share
        );
        let paged: Vec<(Located, u64)> = page
            .hits
            .iter()
            .map(|h| (h.location, h.score.to_bits()))
            .collect();
        prop_assert_eq!(paged, bits(&every[..every.len().min(100)]));
        for hit in &page.hits {
            let explained = hit.explanation.as_ref().map(|e| e.score().to_bits());
            prop_assert_eq!(explained, Some(hit.score.to_bits()), "{:?}", hit.location);
        }
    }
    Ok(())
}

/// Invariant 5 after every operation, over small collections: several random filters (`keyword` ones read postings),
/// a frequent and a rare `must`.
#[test]
fn mixed_filters_select_what_the_reference_selects_at_every_share() {
    let cases = if cfg!(feature = "slow") { 256 } else { 32 };
    run(
        "mixed_filters_select_what_the_reference_selects_at_every_share",
        cases,
        (
            prop::collection::vec(op(), 1..16),
            prop::collection::vec(filter(), 2..5),
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
                // After every operation: puts and upserts, deletes, and commits that flush the memtable.
                let snapshot = db.snapshot();
                let c = snapshot.collection("c").unwrap();
                let memtable = c.memtable();
                for word in ["common", "alpha"] {
                    mixed(c, &memtable, &model, &filters, word)?;
                }
            }
            Ok(())
        },
    );
}

/// Invariant 5 over segments of several FOR blocks: a `keyword` filter beside column filters, fixed and random.
#[test]
fn mixed_filters_over_banded_blocks_select_what_the_reference_selects() {
    let cases = if cfg!(feature = "slow") { 64 } else { 4 };
    let (db, model) = &*MANY_BLOCKS;
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let f = &*FIELDS;
    let tag = |t: &str| Filter::Term {
        field: f.tag,
        value: FilterValue::Keyword(t.into()),
    };
    let fixed = vec![
        tag("a"),
        range(
            f.n,
            Bound::Included(FilterValue::I64(0)),
            Bound::Included(FilterValue::I64(1)),
        ),
        Filter::Exists { field: f.at },
    ];
    for word in ["common", "alpha"] {
        mixed(c, &memtable, model, &fixed, word).unwrap();
    }
    run(
        "mixed_filters_over_banded_blocks_select_what_the_reference_selects",
        cases,
        (0..TAGS.len(), prop::collection::vec(filter(), 1..3)),
        |(t, mut filters)| {
            filters.push(tag(TAGS[t]));
            for word in ["common", "alpha"] {
                mixed(c, &memtable, model, &filters, word)?;
            }
            Ok(())
        },
    );
}

/// The per-candidate path runs beside a postings filter (ROADMAP 7.4.16): a rare `must` under a `keyword` filter and
/// column filters reads about what it reads under the `keyword` filter alone, where a bitmap of the column filters
/// reads the keys of every FOR block they cut.
#[test]
fn a_rare_must_checks_its_column_filters_beside_a_keyword_bitmap() {
    use persia_engine::db::{Execution, Threads};
    let (db, _) = &*MANY_BLOCKS;
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let f = &*FIELDS;
    // `None`: the fitted shares, as a search plans.
    let reads = |query: &Query, share: Option<u64>| {
        let plan = SearchPlan::new(query, c.schema(), &c.search_statistics(&memtable)).unwrap();
        let plan = match share {
            Some(share) => plan.with_check_share(share),
            None => plan,
        };
        let clock = Arc::new(CountingClock {
            start: std::time::Instant::now(),
            reads: std::sync::atomic::AtomicU64::new(0),
        });
        let far = clock.start + std::time::Duration::from_secs(3_600);
        let exec = Execution::default()
            .threads(Threads::Current)
            .deadline(clock.clone(), far);
        views.top_k_with(&plan, 10, &exec).unwrap();
        clock.reads.load(std::sync::atomic::Ordering::Relaxed)
    };
    let keyword = Filter::Terms {
        field: f.tag,
        values: vec![
            FilterValue::Keyword("a".into()),
            FilterValue::Keyword("b".into()),
        ],
    };
    let column = vec![
        Filter::Terms {
            field: f.n,
            values: vec![FilterValue::I64(0), FilterValue::I64(1)],
        },
        Filter::Exists { field: f.at },
    ];
    let query = |word: &str, filters: Vec<Filter>| {
        Query::Bool(BoolQuery {
            must: vec![term(word)],
            filter: filters,
            ..BoolQuery::default()
        })
    };
    let mut both = column.clone();
    both.insert(1, keyword.clone());
    let alone = reads(&query("alpha", vec![keyword.clone()]), None);
    let checked = reads(&query("alpha", both.clone()), None);
    assert!(
        checked <= alone + 1,
        "{checked} reads checked, {alone} keyword alone"
    );
    // Never checking selects the column filters' bitmap too.
    let scanned = reads(&query("alpha", both), Some(0));
    assert!(
        scanned > checked,
        "{scanned} reads scanned, {checked} checked"
    );
}
