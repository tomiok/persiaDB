//! A search request over a collection: sort, pages, cursors, facets and hit assembly (ROADMAP 7.5; SPEC §7.4, §7.5,
//! §7.5.1).
//!
//! Invariants:
//! 1. Hits rank by the sort keys in order (values compared as their type, a missing value where `missing` puts it,
//!    a multi-valued keyword by its smallest value ascending and largest descending), then segment order with the
//!    memtable last, then docid; the page is `offset..offset + limit` of that order. A model sorting the documents'
//!    own values (not the columns) agrees, scores bit for bit, under every pruning strategy.
//! 2. Paging with each page's last cursor visits every hit once, in order, as one large page does.
//! 3. A facet counts every live match holding each value, whatever the page; the values returned are the `size`
//!    most frequent, then ascending bytes.
//! 4. A hit's document is the stored document, projected; its id and blob reference always come back.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are fixed.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::Arc;

use persia_engine::db::{
    CollectionSnapshot, Cursor, Database, DatabaseOptions, Located, ManualClock, SearchRequest,
    SearchResults, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::MemtableView;
use persia_engine::query::{BoolQuery, Query, TermQuery};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::search::{
    FacetRequest, MAX_WINDOW, Missing, Order, Projection, Pruning, RequestError, SearchPlan, Sort,
    SortKey, SortValue,
};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_format::{BlobId, BlobRef};
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestCaseError, TestRunner};

const WORDS: [&str; 3] = ["alpha", "beta", "common"];
const TAGS: [&str; 4] = ["red", "green", "blue", "zz"];
const FLOATS: [f64; 4] = [-1.5, -0.0, 0.0, 2.5];
const IDS: u8 = 8;
const PROPTEST_SEED: u64 = 0x7500_0000_0001;
const STRATEGIES: [Pruning; 3] = [
    Pruning::Exhaustive,
    Pruning::MaxScore,
    Pruning::BlockMaxWand,
];

struct Fields {
    body: FieldId,
    tag: FieldId,
    n: FieldId,
    f: FieldId,
    flag: FieldId,
}

fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field(
            "tag",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .field("n", FieldKind::I64 { stored: true })
        .field("f", FieldKind::F64 { stored: true })
        .field("flag", FieldKind::Bool { stored: true })
        .build()
        .unwrap()
}

fn fields() -> Fields {
    let s = schema();
    let id = |name| s.field(name).unwrap().id();
    Fields {
        body: id("body"),
        tag: id("tag"),
        n: id("n"),
        f: id("f"),
        flag: id("flag"),
    }
}

fn database(schema: CollectionSchema) -> Database<SimFile> {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [5; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    db
}

fn term(field: FieldId, t: &str) -> Query {
    Query::Term(TermQuery {
        field,
        term: t.into(),
    })
}

/// The document at `location`, as stored.
fn document(views: &persia_engine::db::CollectionViews<'_>, location: Located) -> Document {
    match location {
        Located::Segment { index, docid } => views.segments().nth(index).unwrap().document(docid),
        Located::Memtable(docid) => views.memtable().document(docid),
    }
    .unwrap()
}

fn order(location: Located) -> (usize, u32) {
    match location {
        Located::Segment { index, docid } => (index, docid),
        Located::Memtable(docid) => (usize::MAX, docid),
    }
}

/// A document's values, as the model reads them from the document (not from the columns).
#[derive(Debug, Clone, PartialEq)]
enum ModelValue {
    Missing,
    Score(f64),
    I64(i64),
    F64(f64),
    Bool(bool),
    Keyword(String),
}

fn model_value(key: SortKey, doc: &Document, score: f64) -> ModelValue {
    let SortKey::Field { field, order, .. } = key else {
        return ModelValue::Score(score + 0.0);
    };
    match doc.get(field) {
        None => ModelValue::Missing,
        Some(Value::I64(v)) => ModelValue::I64(*v),
        Some(Value::F64(v)) => ModelValue::F64(*v + 0.0),
        Some(Value::Bool(v)) => ModelValue::Bool(*v),
        Some(Value::String(s)) => ModelValue::Keyword(s.clone()),
        Some(Value::Strings(list)) => {
            let pick = match order {
                Order::Asc => list.iter().min(),
                Order::Desc => list.iter().max(),
            };
            pick.map_or(ModelValue::Missing, |s| ModelValue::Keyword(s.clone()))
        }
        Some(other) => panic!("unexpected value {other:?}"),
    }
}

fn model_compare(keys: &[SortKey], a: &[ModelValue], b: &[ModelValue]) -> Ordering {
    for ((key, a), b) in keys.iter().zip(a).zip(b) {
        let (order, missing) = match *key {
            SortKey::Score { order } => (order, Missing::Last),
            SortKey::Field { order, missing, .. } => (order, missing),
        };
        let o = match (a, b) {
            (ModelValue::Missing, ModelValue::Missing) => Ordering::Equal,
            (ModelValue::Missing, _) if missing == Missing::First => Ordering::Less,
            (ModelValue::Missing, _) => Ordering::Greater,
            (_, ModelValue::Missing) if missing == Missing::First => Ordering::Greater,
            (_, ModelValue::Missing) => Ordering::Less,
            (a, b) => {
                let o = match (a, b) {
                    (ModelValue::Score(x), ModelValue::Score(y))
                    | (ModelValue::F64(x), ModelValue::F64(y)) => x.total_cmp(y),
                    (ModelValue::I64(x), ModelValue::I64(y)) => x.cmp(y),
                    (ModelValue::Bool(x), ModelValue::Bool(y)) => x.cmp(y),
                    (ModelValue::Keyword(x), ModelValue::Keyword(y)) => x.cmp(y),
                    _ => panic!("values of different keys: {a:?} {b:?}"),
                };
                if order == Order::Desc { o.reverse() } else { o }
            }
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Every match of `plan`, as the model ranks them by `keys`: `(location, score, document)`.
fn model(
    views: &persia_engine::db::CollectionViews<'_>,
    plan: &SearchPlan,
    keys: &[SortKey],
) -> Vec<(Located, f64, Document)> {
    let mut all: Vec<_> = views
        .top_k(plan, MAX_WINDOW * 10, Pruning::Exhaustive)
        .unwrap()
        .into_iter()
        .map(|h| (h.location, h.score, document(views, h.location)))
        .collect();
    let values = |(_, score, doc): &(Located, f64, Document)| -> Vec<ModelValue> {
        keys.iter().map(|&k| model_value(k, doc, *score)).collect()
    };
    all.sort_by(|a, b| {
        model_compare(keys, &values(a), &values(b)).then(order(a.0).cmp(&order(b.0)))
    });
    all
}

fn model_facet(
    matches: &[(Located, f64, Document)],
    field: FieldId,
    size: usize,
) -> Vec<(Vec<u8>, u64)> {
    let mut counts = BTreeMap::<Vec<u8>, u64>::new();
    for (_, _, doc) in matches {
        let values: BTreeSet<&str> = match doc.get(field) {
            Some(Value::String(s)) => [s.as_str()].into(),
            Some(Value::Strings(list)) => list.iter().map(String::as_str).collect(),
            _ => BTreeSet::new(),
        };
        for v in values {
            *counts.entry(v.as_bytes().to_vec()).or_default() += 1;
        }
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    counts.truncate(size);
    counts
}

fn located(results: &SearchResults) -> Vec<(Located, u64)> {
    results
        .hits
        .iter()
        .map(|h| (h.location, h.score.to_bits()))
        .collect()
}

#[derive(Debug, Clone)]
enum Op {
    Put {
        id: u8,
        words: Vec<u8>,
        tags: Vec<u8>,
        n: Option<i8>,
        f: Option<u8>,
        flag: Option<bool>,
    },
    Delete(u8),
    Commit,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (
            0..IDS,
            prop::collection::vec(0..u8::try_from(WORDS.len()).unwrap(), 1..4),
            prop::collection::vec(0..u8::try_from(TAGS.len()).unwrap(), 0..3),
            prop::option::of(-3_i8..3),
            prop::option::of(0..u8::try_from(FLOATS.len()).unwrap()),
            prop::option::of(any::<bool>()),
        )
            .prop_map(|(id, words, tags, n, f, flag)| Op::Put { id, words, tags, n, f, flag }),
        1 => (0..IDS).prop_map(Op::Delete),
        1 => Just(Op::Commit),
    ]
}

fn put(db: &mut Database<SimFile>, op: &Op) {
    let f = fields();
    match op {
        Op::Put {
            id,
            words,
            tags,
            n,
            f: float,
            flag,
        } => {
            let body: Vec<_> = words.iter().map(|&w| WORDS[w as usize]).collect();
            let mut d = Document::new(format!("d{id}")).with(f.body, Value::String(body.join(" ")));
            match tags.as_slice() {
                [] => {}
                [t] => d
                    .insert(f.tag, Value::String(TAGS[*t as usize].into()))
                    .map_or((), drop),
                more => d
                    .insert(
                        f.tag,
                        Value::Strings(more.iter().map(|&t| TAGS[t as usize].into()).collect()),
                    )
                    .map_or((), drop),
            }
            if let Some(n) = n {
                d.insert(f.n, Value::I64(i64::from(*n)));
            }
            if let Some(i) = float {
                d.insert(f.f, Value::F64(FLOATS[*i as usize]));
            }
            if let Some(b) = flag {
                d.insert(f.flag, Value::Bool(*b));
            }
            db.put("c", &d, Durability::Buffered).unwrap();
        }
        Op::Delete(id) => {
            db.delete("c", &format!("d{id}"), Durability::Buffered)
                .unwrap();
        }
        Op::Commit => {
            db.commit().unwrap();
        }
    }
}

fn sort_key() -> impl Strategy<Value = SortKey> {
    let f = fields();
    let field = prop::sample::select(vec![f.tag, f.n, f.f, f.flag]);
    let order = prop_oneof![Just(Order::Asc), Just(Order::Desc)];
    let missing = prop_oneof![Just(Missing::Last), Just(Missing::First)];
    prop_oneof![
        1 => order.clone().prop_map(|order| SortKey::Score { order }),
        3 => (field, order, missing).prop_map(|(field, order, missing)| SortKey::Field { field, order, missing }),
    ]
}

fn query() -> impl Strategy<Value = Query> {
    let f = fields();
    prop_oneof![
        Just(Query::MatchAll),
        (0..WORDS.len()).prop_map(move |w| term(f.body, WORDS[w])),
        Just(Query::Bool(BoolQuery {
            should: WORDS.iter().map(|w| term(fields().body, w)).collect(),
            ..BoolQuery::default()
        })),
    ]
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

/// Invariants 1–3 against the model, over segments, deletions, upserts and the memtable.
#[test]
fn requests_match_a_model() {
    let cases = if cfg!(feature = "slow") { 512 } else { 96 };
    run(
        "requests_match_a_model",
        cases,
        (
            prop::collection::vec(op(), 1..30),
            query(),
            prop::collection::vec(sort_key(), 0..4),
            0..6_usize,
            0..7_usize,
            1..4_usize,
            1..5_usize,
        ),
        |(ops, query, keys, offset, limit, page, facet_size)| {
            let mut db = database(schema());
            for op in &ops {
                put(&mut db, op);
            }
            let snapshot = db.snapshot();
            let c: &CollectionSnapshot = snapshot.collection("c").unwrap();
            let memtable: MemtableView = c.memtable();
            let plan =
                SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
            let views = c.views(&memtable).unwrap();
            let sort = Sort::new(keys.iter().copied(), c.schema()).unwrap();
            let keys: Vec<SortKey> = sort.keys().collect();
            let all = model(&views, &plan, &keys);
            let f = fields();
            let request = SearchRequest {
                sort,
                offset,
                limit,
                facets: vec![FacetRequest::new(f.tag, facet_size, c.schema()).unwrap()],
                ..SearchRequest::default()
            };
            let expected: Vec<(Located, u64)> = all
                .iter()
                .skip(offset)
                .take(limit)
                .map(|(l, s, _)| (*l, s.to_bits()))
                .collect();
            let facet: Vec<(Vec<u8>, u64)> = model_facet(&all, f.tag, facet_size);
            for pruning in STRATEGIES {
                // With and without the facet: only the facet turns pruning off.
                for facets in [true, false] {
                    let request = SearchRequest {
                        facets: if facets {
                            request.facets.clone()
                        } else {
                            Vec::new()
                        },
                        ..request.clone()
                    };
                    let results = views.search(&plan, &request, pruning).unwrap();
                    prop_assert_eq!(
                        &located(&results),
                        &expected,
                        "{:?}, facets {}",
                        pruning,
                        facets
                    );
                    for (hit, (_, _, doc)) in results.hits.iter().zip(all.iter().skip(offset)) {
                        prop_assert_eq!(&hit.document, doc);
                        prop_assert_eq!(hit.sort.len(), keys.len());
                    }
                    if facets {
                        let got: Vec<(Vec<u8>, u64)> = results.facets[0]
                            .values
                            .iter()
                            .map(|v| (v.value.clone(), v.count))
                            .collect();
                        prop_assert_eq!(&got, &facet);
                    } else {
                        prop_assert!(results.facets.is_empty());
                    }
                }
                // Invariant 2: cursor pages, `page` hits each, visit every match in order.
                let mut seen = Vec::new();
                let mut after: Option<Cursor> = None;
                loop {
                    let request = SearchRequest {
                        sort: request.sort.clone(),
                        limit: page,
                        search_after: after.clone(),
                        ..SearchRequest::default()
                    };
                    let results = views.search(&plan, &request, pruning).unwrap();
                    let Some(last) = results.hits.last() else {
                        break;
                    };
                    after = Some(last.cursor());
                    seen.extend(located(&results));
                    prop_assert!(seen.len() <= all.len(), "pages repeat hits");
                }
                let every: Vec<(Located, u64)> =
                    all.iter().map(|(l, s, _)| (*l, s.to_bits())).collect();
                prop_assert_eq!(seen, every, "{:?}", pruning);
            }
            Ok(())
        },
    );
}

/// A collection of three documents with fixed values: `a` and `c` committed, `b` in the memtable.
fn fixture() -> Database<SimFile> {
    let f = fields();
    let mut db = database(schema());
    let docs = [
        Document::new("a")
            .with(f.body, Value::String("alpha".into()))
            .with(f.tag, Value::Strings(vec!["red".into(), "blue".into()]))
            .with(f.n, Value::I64(2)),
        Document::new("c")
            .with(f.body, Value::String("alpha beta".into()))
            .with(f.tag, Value::String("green".into()))
            .with(f.n, Value::I64(-1))
            .with(f.f, Value::F64(0.5)),
        Document::new("b")
            .with(f.body, Value::String("beta".into()))
            .with(f.tag, Value::String("red".into())),
    ];
    for (i, d) in docs.iter().enumerate() {
        db.put("c", d, Durability::Fsync).unwrap();
        if i == 1 {
            db.commit().unwrap();
        }
    }
    db
}

/// The ids of `request`'s hits for `query`.
fn ids(db: &Database<SimFile>, query: &Query, request: &SearchRequest) -> Vec<String> {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(query, c.schema(), &c.search_statistics(&memtable)).unwrap();
    let results = c
        .views(&memtable)
        .unwrap()
        .search(&plan, request, Pruning::default())
        .unwrap();
    results
        .hits
        .iter()
        .map(|h| h.document.id().to_owned())
        .collect()
}

fn sorted(keys: impl IntoIterator<Item = SortKey>) -> SearchRequest {
    SearchRequest {
        sort: Sort::new(keys, &schema()).unwrap(),
        ..SearchRequest::default()
    }
}

#[test]
fn missing_values_go_last_or_first_in_either_order() {
    let db = fixture();
    let f = fields();
    let field = |order, missing| SortKey::Field {
        field: f.n,
        order,
        missing,
    };
    let all = Query::MatchAll;
    assert_eq!(
        ids(&db, &all, &sorted([field(Order::Asc, Missing::Last)])),
        ["c", "a", "b"]
    );
    assert_eq!(
        ids(&db, &all, &sorted([field(Order::Desc, Missing::Last)])),
        ["a", "c", "b"]
    );
    assert_eq!(
        ids(&db, &all, &sorted([field(Order::Asc, Missing::First)])),
        ["b", "c", "a"]
    );
    assert_eq!(
        ids(&db, &all, &sorted([field(Order::Desc, Missing::First)])),
        ["b", "a", "c"]
    );
}

#[test]
fn a_multi_valued_keyword_sorts_by_its_smallest_ascending_and_largest_descending() {
    let db = fixture();
    let f = fields();
    // a: blue, red · b: red · c: green
    assert_eq!(
        ids(
            &db,
            &Query::MatchAll,
            &sorted([SortKey::field(f.tag, Order::Asc)])
        ),
        ["a", "c", "b"]
    );
    // Descending, a and b tie on red: segment order first.
    assert_eq!(
        ids(
            &db,
            &Query::MatchAll,
            &sorted([SortKey::field(f.tag, Order::Desc)])
        ),
        ["a", "b", "c"]
    );
}

#[test]
fn a_later_key_breaks_ties_of_an_earlier_one() {
    let db = fixture();
    let f = fields();
    // flag is missing everywhere; then by n descending.
    let keys = [
        SortKey::field(f.flag, Order::Asc),
        SortKey::field(f.n, Order::Desc),
    ];
    assert_eq!(ids(&db, &Query::MatchAll, &sorted(keys)), ["a", "c", "b"]);
    // match_all scores 0 everywhere: score ascending is location order.
    let keys = [SortKey::Score { order: Order::Asc }];
    assert_eq!(ids(&db, &Query::MatchAll, &sorted(keys)), ["a", "c", "b"]);
}

#[test]
fn a_hit_reports_its_score_and_sort_values() {
    let db = fixture();
    let f = fields();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let query = term(f.body, "alpha");
    let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
    let views = c.views(&memtable).unwrap();
    let request = sorted([
        SortKey::field(f.f, Order::Desc),
        SortKey::field(f.tag, Order::Desc),
        SortKey::score(),
    ]);
    let results = views.search(&plan, &request, Pruning::default()).unwrap();
    let scored = views.top_k(&plan, 10, Pruning::Exhaustive).unwrap();
    assert_eq!(results.hits.len(), 2);
    let c_hit = &results.hits[0];
    assert_eq!(c_hit.document.id(), "c");
    let c_score = scored
        .iter()
        .find(|h| h.location == c_hit.location)
        .unwrap()
        .score;
    assert_eq!(c_hit.score.to_bits(), c_score.to_bits());
    assert_eq!(
        c_hit.sort,
        [
            SortValue::Key(persia_format::for_codec::f64_to_key(0.5)),
            SortValue::Keyword(b"green".to_vec()),
            SortValue::Score(c_score),
        ]
    );
    assert_eq!(results.hits[1].sort[0], SortValue::Missing);
    assert_eq!(results.hits[1].sort[1], SortValue::Keyword(b"red".to_vec()));
}

#[test]
fn offset_and_limit_cut_the_ranked_hits() {
    let db = fixture();
    let f = fields();
    let base = sorted([SortKey::field(f.n, Order::Asc)]);
    let page = |offset, limit| SearchRequest {
        offset,
        limit,
        ..base.clone()
    };
    assert_eq!(ids(&db, &Query::MatchAll, &page(1, 1)), ["a"]);
    assert_eq!(ids(&db, &Query::MatchAll, &page(1, 10)), ["a", "b"]);
    assert_eq!(
        ids(&db, &Query::MatchAll, &page(3, 10)),
        Vec::<String>::new()
    );
    assert_eq!(
        ids(&db, &Query::MatchAll, &page(0, 0)),
        Vec::<String>::new()
    );
    // By score too, where pruning applies.
    let page = |offset, limit| SearchRequest {
        offset,
        limit,
        ..SearchRequest::default()
    };
    assert_eq!(ids(&db, &Query::MatchAll, &page(2, 5)), ["b"]);
}

#[test]
fn a_cursor_offset_counts_after_the_cursor() {
    let db = fixture();
    let f = fields();
    let base = sorted([SortKey::field(f.n, Order::Asc)]);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(
        &Query::MatchAll,
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    let views = c.views(&memtable).unwrap();
    let first = views
        .search(
            &plan,
            &SearchRequest {
                limit: 1,
                ..base.clone()
            },
            Pruning::default(),
        )
        .unwrap();
    assert_eq!(first.hits[0].document.id(), "c");
    let next = SearchRequest {
        offset: 1,
        search_after: Some(first.hits[0].cursor()),
        ..base
    };
    let rest = views.search(&plan, &next, Pruning::default()).unwrap();
    assert_eq!(
        rest.hits
            .iter()
            .map(|h| h.document.id())
            .collect::<Vec<_>>(),
        ["b"]
    );
}

#[test]
fn bad_requests_are_errors() {
    let db = fixture();
    let s = schema();
    let f = fields();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(
        &Query::MatchAll,
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    let views = c.views(&memtable).unwrap();
    let run = |r: &SearchRequest| views.search(&plan, r, Pruning::default());

    let deep = SearchRequest {
        offset: MAX_WINDOW,
        limit: 1,
        ..SearchRequest::default()
    };
    assert!(matches!(run(&deep), Err(RequestError::Window { .. })));
    let overflow = SearchRequest {
        offset: usize::MAX,
        limit: 1,
        ..SearchRequest::default()
    };
    assert!(matches!(run(&overflow), Err(RequestError::Window { .. })));
    let edge = SearchRequest {
        offset: MAX_WINDOW - 1,
        limit: 1,
        ..SearchRequest::default()
    };
    assert_eq!(run(&edge).unwrap().hits, []);

    let cursor = |values| {
        Some(Cursor {
            values,
            location: Located::Memtable(0),
        })
    };
    let wrong_count = SearchRequest {
        search_after: cursor(vec![]),
        ..SearchRequest::default()
    };
    assert!(matches!(
        run(&wrong_count),
        Err(RequestError::Cursor { .. })
    ));
    let wrong_type = SearchRequest {
        search_after: cursor(vec![SortValue::Key(1)]),
        ..SearchRequest::default()
    };
    assert!(matches!(run(&wrong_type), Err(RequestError::Cursor { .. })));
    let nan = SearchRequest {
        search_after: cursor(vec![SortValue::Score(f64::NAN)]),
        ..SearchRequest::default()
    };
    assert!(matches!(run(&nan), Err(RequestError::Cursor { .. })));
    let keyword = SearchRequest {
        search_after: cursor(vec![SortValue::Key(1)]),
        ..sorted([SortKey::field(f.tag, Order::Asc)])
    };
    assert!(matches!(run(&keyword), Err(RequestError::Cursor { .. })));
    let missing_ok = SearchRequest {
        search_after: cursor(vec![SortValue::Missing]),
        ..sorted([SortKey::field(f.tag, Order::Asc)])
    };
    assert!(run(&missing_ok).is_ok());

    assert!(matches!(
        Sort::new([SortKey::field(f.body, Order::Asc)], &s),
        Err(RequestError::SortField { .. })
    ));
    assert!(matches!(
        Sort::new([SortKey::field(beyond(), Order::Asc)], &s),
        Err(RequestError::SortField { .. })
    ));
    assert!(matches!(
        Sort::new(vec![SortKey::score(); 17], &s),
        Err(RequestError::TooManySortKeys { .. })
    ));
    assert!(Sort::new(vec![SortKey::score(); 16], &s).is_ok());
    assert!(matches!(
        FacetRequest::new(f.n, 3, &s),
        Err(RequestError::FacetField { .. })
    ));
    assert!(matches!(
        FacetRequest::new(f.tag, 1001, &s),
        Err(RequestError::FacetSize { .. })
    ));
    assert!(FacetRequest::new(f.tag, 1000, &s).is_ok());
}

#[test]
fn a_facet_counts_every_match_whatever_the_page() {
    let db = fixture();
    let f = fields();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(
        &Query::MatchAll,
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    let views = c.views(&memtable).unwrap();
    let request = SearchRequest {
        limit: 0,
        facets: vec![
            FacetRequest::new(f.tag, 2, c.schema()).unwrap(),
            FacetRequest::new(f.tag, 10, c.schema()).unwrap(),
        ],
        ..SearchRequest::default()
    };
    let results = views.search(&plan, &request, Pruning::default()).unwrap();
    assert_eq!(results.hits, []);
    let counts = |i: usize| -> Vec<(&[u8], u64)> {
        results.facets[i]
            .values
            .iter()
            .map(|v| (v.value.as_slice(), v.count))
            .collect()
    };
    // red: a (segment) and b (memtable); blue and green once: ties by bytes.
    assert_eq!(counts(0), [(&b"red"[..], 2), (b"blue", 1)]);
    assert_eq!(counts(1), [(&b"red"[..], 2), (b"blue", 1), (b"green", 1)]);
    assert_eq!(results.facets[0].field, f.tag);
}

#[test]
fn a_hit_returns_its_projected_document_and_blob() {
    let f = fields();
    let mut db = database(schema());
    let blob = BlobRef::new(
        BlobId::from_bytes([7; 32]),
        3,
        "text/plain",
        1024,
        BTreeMap::new(),
    )
    .unwrap();
    let mut d = Document::new("x")
        .with(f.body, Value::String("alpha".into()))
        .with(f.n, Value::I64(1))
        .with(f.tag, Value::String("red".into()));
    d.set_blob_ref(Some(blob.clone()));
    db.put("c", &d, Durability::Fsync).unwrap();
    let get = |db: &Database<SimFile>, projection: Projection| {
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let plan = SearchPlan::new(
            &Query::MatchAll,
            c.schema(),
            &c.search_statistics(&memtable),
        )
        .unwrap();
        let request = SearchRequest {
            projection,
            ..SearchRequest::default()
        };
        let mut results = c
            .views(&memtable)
            .unwrap()
            .search(&plan, &request, Pruning::default())
            .unwrap();
        results.hits.remove(0).document
    };
    for committed in [false, true] {
        if committed {
            db.commit().unwrap();
        }
        let all = get(&db, Projection::All);
        assert_eq!(all, d, "committed: {committed}");
        let none = get(&db, Projection::None);
        assert_eq!(
            (none.id(), none.blob_ref(), none.fields().count()),
            ("x", Some(&blob), 0)
        );
        let some = get(&db, Projection::fields(["n"], &schema()).unwrap());
        assert_eq!(some.fields().collect::<Vec<_>>(), [(f.n, &Value::I64(1))]);
        assert_eq!(some.blob_ref(), Some(&blob));
    }
}

#[test]
fn a_projection_names_stored_or_extra_fields() {
    let strict = SchemaBuilder::new("c")
        .field("hidden", FieldKind::I64 { stored: false })
        .field("shown", FieldKind::I64 { stored: true })
        .build()
        .unwrap();
    assert!(matches!(
        Projection::fields(["hidden"], &strict),
        Err(RequestError::Projection { .. })
    ));
    assert!(matches!(
        Projection::fields(["extra"], &strict),
        Err(RequestError::Projection { .. })
    ));
    assert!(Projection::fields(["shown"], &strict).is_ok());

    let loose = SchemaBuilder::new("c")
        .field("shown", FieldKind::I64 { stored: true })
        .strict(false)
        .build()
        .unwrap();
    let shown = loose.field("shown").unwrap().id();
    let mut db = database(loose.clone());
    let mut d = Document::new("x").with(shown, Value::I64(1));
    d.insert_extra("note", serde_json::json!("hi"));
    d.insert_extra("other", serde_json::json!(2));
    db.put("c", &d, Durability::Fsync).unwrap();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(
        &Query::MatchAll,
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    let request = SearchRequest {
        projection: Projection::fields(["note", "absent"], &loose).unwrap(),
        ..SearchRequest::default()
    };
    let results = c
        .views(&memtable)
        .unwrap()
        .search(&plan, &request, Pruning::default())
        .unwrap();
    let got = &results.hits[0].document;
    assert_eq!(got.fields().count(), 0);
    assert_eq!(got.extra().keys().collect::<Vec<_>>(), ["note"]);
}

/// Every document's column keys (`n`, `f`, `flag`) and keyword values (`tag`) in `view`, read in `docids` order.
type Columns = BTreeMap<u32, (Vec<Option<u64>>, Vec<Vec<u8>>)>;

fn columns<V: SearchView>(view: &V, docids: impl Iterator<Item = u32>) -> Columns {
    use persia_engine::view::{KeyReader, KeywordReader};
    let f = fields();
    let mut keys = [f.n, f.f, f.flag].map(|field| view.keys(field).unwrap());
    let mut words = view.keywords(f.tag).unwrap();
    let mut out = BTreeMap::new();
    for docid in docids {
        let k = keys.iter_mut().map(|r| r.key(docid).unwrap()).collect();
        let ords = words.ords(docid).unwrap().to_vec();
        let w = ords
            .into_iter()
            .map(|o| words.value(o).unwrap().to_vec())
            .collect();
        out.insert(docid, (k, w));
    }
    out
}

/// The segment and memtable column readers agree on every document's values.
#[test]
fn segment_and_memtable_columns_agree() {
    let f = fields();
    let mut db = database(schema());
    for id in 0..IDS {
        let op = Op::Put {
            id,
            words: vec![id % 3],
            tags: (0..id % 4).map(|t| (t + id) % 4).collect(),
            n: (id % 3 != 0).then(|| i8::try_from(id).unwrap() - 4),
            f: (id % 2 == 0).then_some(id % 4),
            flag: (id % 5 != 0).then_some(id % 2 == 0),
        };
        put(&mut db, &op);
    }
    let in_memtable = {
        let snapshot = db.snapshot();
        let memtable = snapshot.collection("c").unwrap().memtable();
        columns(&memtable, 0..memtable.doc_count())
    };
    db.commit().unwrap();
    let snapshot = db.snapshot();
    let coll = snapshot.collection("c").unwrap();
    let segment = coll.segment_views().next().unwrap();
    // Out of order too: a reader is random access.
    let in_segment = columns(&segment, (0..segment.doc_count()).rev());
    assert_eq!(in_memtable, in_segment);
    assert!(
        in_memtable
            .values()
            .any(|(k, w)| k.iter().any(Option::is_none) && w.len() > 1)
    );
    // A field of the wrong type is an error.
    assert!(segment.keys(f.tag).is_err());
    assert!(segment.keywords(f.n).is_err());
    assert!(coll.memtable().keys(f.body).is_err());
    assert!(coll.memtable().keywords(f.flag).is_err());
}

/// A field id past [`schema`]'s fields.
fn beyond() -> FieldId {
    let mut b = SchemaBuilder::new("wide");
    for name in ["a", "b", "c", "d", "e", "f"] {
        b = b.field(name, FieldKind::I64 { stored: true });
    }
    b.build().unwrap().field("f").unwrap().id()
}

#[test]
fn a_cursor_from_a_location_no_view_has_is_well_defined() {
    let db = fixture();
    let f = fields();
    let base = sorted([SortKey::field(f.n, Order::Asc)]);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(
        &Query::MatchAll,
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    let views = c.views(&memtable).unwrap();
    // n = -1 is c's; a tie at a segment index past every segment ranks the memtable's ties after it, the
    // segments' before it.
    let at = |location| SearchRequest {
        search_after: Some(Cursor {
            values: vec![SortValue::Key(persia_format::for_codec::i64_to_key(-1))],
            location,
        }),
        ..base.clone()
    };
    for location in [
        Located::Segment {
            index: usize::MAX - 1,
            docid: u32::MAX,
        },
        Located::Segment { index: 7, docid: 0 },
    ] {
        let hits = views
            .search(&plan, &at(location), Pruning::default())
            .unwrap()
            .hits;
        let ids: Vec<_> = hits.iter().map(|h| h.document.id()).collect();
        assert_eq!(ids, ["a", "b"], "{location:?}");
    }
}

#[test]
fn keywords_compare_by_bytes_across_segments_with_different_dictionaries() {
    let f = fields();
    let mut db = database(schema());
    // Segment 0 holds only "zz" (ord 0), segment 1 "blue" and "zz" (ords 0, 1), the memtable "green".
    let tagged = |id: &str, tag: &str| Document::new(id).with(f.tag, Value::String(tag.into()));
    db.put("c", &tagged("s0", "zz"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("c", &tagged("s1z", "zz"), Durability::Fsync)
        .unwrap();
    db.put("c", &tagged("s1b", "blue"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    db.put("c", &tagged("m", "green"), Durability::Fsync)
        .unwrap();
    let asc = sorted([SortKey::field(f.tag, Order::Asc)]);
    assert_eq!(ids(&db, &Query::MatchAll, &asc), ["s1b", "m", "s0", "s1z"]);
    let desc = sorted([SortKey::field(f.tag, Order::Desc)]);
    assert_eq!(ids(&db, &Query::MatchAll, &desc), ["s0", "s1z", "m", "s1b"]);
}

#[test]
fn a_page_past_the_matches_is_empty_and_still_counts_facets() {
    let db = fixture();
    let f = fields();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(
        &Query::MatchAll,
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    let views = c.views(&memtable).unwrap();
    let facet = FacetRequest::new(f.tag, 1, c.schema()).unwrap();
    let request = SearchRequest {
        offset: 50,
        facets: vec![facet],
        ..SearchRequest::default()
    };
    let results = views.search(&plan, &request, Pruning::default()).unwrap();
    assert_eq!(results.hits, []);
    assert_eq!(results.facets[0].values[0].value, b"red");
    assert_eq!(results.facets[0].values[0].count, 2);

    let many = SearchRequest {
        facets: vec![facet; 17],
        ..SearchRequest::default()
    };
    assert!(matches!(
        views.search(&plan, &many, Pruning::default()),
        Err(RequestError::TooManyFacets { .. })
    ));
    let most = SearchRequest {
        facets: vec![facet; 16],
        ..SearchRequest::default()
    };
    assert_eq!(
        views
            .search(&plan, &most, Pruning::default())
            .unwrap()
            .facets
            .len(),
        16
    );
}
