//! ROADMAP 7.5.7: the column copies a memtable's views share give every view the hits, sorts and facets of a view
//! that copies its own, across puts, upserts, deletes and flushes, on any thread.

use core::ops::Bound;

use persia_storage::container::sim::SimFile;
use proptest::prelude::*;

use crate::db::{Database, DatabaseOptions, Execution, SearchRequest, SearchResults, Threads};
use crate::document::{Document, Value};
use crate::memtable::MemtableView;
use crate::query::{BoolQuery, Filter, FilterValue, Query, TermQuery};
use crate::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use crate::search::{FacetRequest, Order, Projection, SearchPlan, Sort, SortKey};
use crate::wal::Durability;

use super::CollectionSnapshot;

struct Fields {
    body: FieldId,
    sku: FieldId,
    price: FieldId,
    flag: FieldId,
}

fn schema() -> (CollectionSchema, Fields) {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field(
            "sku",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .field("price", FieldKind::I64 { stored: false })
        .field("flag", FieldKind::Bool { stored: false })
        .build()
        .unwrap();
    let id = |name| schema.field(name).unwrap().id();
    let fields = Fields {
        body: id("body"),
        sku: id("sku"),
        price: id("price"),
        flag: id("flag"),
    };
    (schema, fields)
}

fn database() -> (Database<SimFile>, Fields) {
    let (schema, fields) = schema();
    let mut db = Database::create_on(SimFile::new(), [5; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    (db, fields)
}

/// Document `id`, version `v`: a word every version holds, its `sku` (none for some), a price and a flag.
fn doc(f: &Fields, id: u32, v: u32) -> Document {
    let mut doc = Document::new(format!("d{id}"))
        .with(f.body, Value::String(format!("all w{}", v % 3)))
        .with(f.price, Value::I64(i64::from((id * 7 + v * 13) % 50)))
        .with(f.flag, Value::Bool((id + v).is_multiple_of(2)));
    if !(id + v).is_multiple_of(5) {
        doc = doc.with(
            f.sku,
            Value::String(format!("sku-{:02}", (id * 3 + v) % 17)),
        );
    }
    doc
}

/// The requests compared: sorts by a `keyword` and a numeric column both ways, a facet, a column filter (a scan of the
/// column's keys) with a sort by it, and a `bool` filter.
fn requests(schema: &CollectionSchema, f: &Fields) -> Vec<(Query, SearchRequest)> {
    let all = Query::Term(TermQuery {
        field: f.body,
        term: "all".into(),
    });
    let page = |sort: Sort, facets: Vec<FacetRequest>| SearchRequest {
        sort,
        limit: 7,
        facets,
        projection: Projection::None,
        ..SearchRequest::default()
    };
    let by = |field, order| Sort::new([SortKey::field(field, order)], schema).unwrap();
    let facet = || vec![FacetRequest::new(f.sku, 5, schema).unwrap()];
    let filtered = |filter| {
        Query::Bool(BoolQuery {
            must: vec![all.clone()],
            filter: vec![filter],
            ..BoolQuery::default()
        })
    };
    let cheap = Filter::Range {
        field: f.price,
        lower: Bound::Included(FilterValue::I64(10)),
        upper: Bound::Excluded(FilterValue::I64(30)),
    };
    let flag = Filter::Term {
        field: f.flag,
        value: FilterValue::Bool(true),
    };
    vec![
        (all.clone(), page(by(f.sku, Order::Asc), Vec::new())),
        (all.clone(), page(by(f.sku, Order::Desc), facet())),
        (all.clone(), page(Sort::score(), facet())),
        (all.clone(), page(by(f.price, Order::Asc), Vec::new())),
        (all.clone(), page(by(f.price, Order::Desc), facet())),
        (filtered(cheap), page(by(f.price, Order::Asc), facet())),
        (filtered(flag), page(by(f.sku, Order::Asc), Vec::new())),
    ]
}

fn search(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    requests: &[(Query, SearchRequest)],
    exec: &Execution,
) -> Vec<SearchResults> {
    let stats = c.search_statistics(memtable);
    let views = c.views(memtable).unwrap();
    requests
        .iter()
        .map(|(query, request)| {
            let plan = SearchPlan::new(query, c.schema(), &stats).unwrap();
            views.search_with(&plan, request, exec).unwrap()
        })
        .collect()
}

/// `memtable`'s results equal those of the same view copying its own columns.
fn check(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    requests: &[(Query, SearchRequest)],
    exec: &Execution,
) {
    let shared = search(c, memtable, requests, exec);
    let own = search(c, &memtable.unshared(), requests, exec);
    assert_eq!(shared, own, "memtable version {}", memtable.version());
}

/// One step of a write/read interleaving.
#[derive(Debug, Clone)]
enum Step {
    /// Put (or upsert) document `id` at version `v`.
    Put(u32, u32),
    Delete(u32),
    /// Flush the memtable into a segment.
    Commit,
    /// Search a new view, twice (the second reads what the first shared), and every view kept so far.
    Read,
    /// Keep a view taken now for later reads, as a reader still on an older request.
    Keep,
}

fn run(steps: &[Step]) {
    let (db, f) = database();
    let mut db = db;
    let exec = Execution::default().threads(Threads::Current);
    let mut kept: Vec<(std::sync::Arc<crate::db::Snapshot>, MemtableView)> = Vec::new();
    for step in steps {
        match *step {
            Step::Put(id, v) => db.put("c", &doc(&f, id, v), Durability::Buffered).unwrap(),
            Step::Delete(id) => {
                db.delete("c", &format!("d{id}"), Durability::Buffered)
                    .unwrap();
            }
            Step::Commit => {
                db.commit().unwrap();
            }
            Step::Keep => {
                let snapshot = db.snapshot();
                let memtable = snapshot.collection("c").unwrap().memtable();
                kept.push((snapshot, memtable));
            }
            Step::Read => {
                let snapshot = db.snapshot();
                let c = snapshot.collection("c").unwrap();
                let requests = requests(c.schema(), &f);
                check(c, &c.memtable(), &requests, &exec);
                check(c, &c.memtable(), &requests, &exec);
                for (snapshot, memtable) in &kept {
                    let c = snapshot.collection("c").unwrap();
                    check(c, memtable, &requests, &exec);
                    // A new view of an older snapshot: the same memtable, at its latest writes (or at the flush).
                    check(c, &c.memtable(), &requests, &exec);
                }
            }
        }
    }
}

/// Interleaved puts, upserts, deletes and flushes, with views kept from before each: every view, new or kept, returns
/// what a view copying its own columns returns. Views of the same documents but other operations (a delete after a
/// read) share a copy; a kept view of fewer documents reads its own.
#[test]
fn shared_copies_give_the_hits_of_own_copies() {
    let mut steps = Vec::new();
    for id in 0..40 {
        steps.push(Step::Put(id, 0));
    }
    steps.extend([Step::Read, Step::Keep]);
    // Deletes only: the same documents, other versions. A new view reads the copy the last one shared.
    steps.extend([Step::Delete(3), Step::Delete(11), Step::Read, Step::Keep]);
    // Upserts: more documents, replaced ones dead.
    for id in (0..40).step_by(3) {
        steps.push(Step::Put(id, 1));
    }
    steps.extend([Step::Read, Step::Keep, Step::Delete(0), Step::Read]);
    // A flush: the new memtable starts with an empty cache; the kept views still read the flushed one.
    steps.extend([Step::Commit, Step::Read, Step::Keep]);
    for id in 20..60 {
        steps.push(Step::Put(id, 2));
    }
    steps.extend([Step::Delete(25), Step::Read, Step::Commit, Step::Read]);
    run(&steps);
}

/// Views of the same documents share one copy; one of more documents copies again and replaces it, and one of fewer
/// documents (a reader on an older request) copies its own and leaves the shared one. The shared copies count in the
/// memtable's size.
#[test]
fn a_copy_is_shared_by_views_of_the_same_documents() {
    use crate::memtable::Memtable;
    use crate::view::{KeyReader, KeywordReader, SearchView};
    use persia_analysis::AnalyzerRegistry;

    let (schema, f) = schema();
    let mut mem = Memtable::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    for id in 0..30 {
        mem.put(&doc(&f, id, 0)).unwrap();
    }
    let older = mem.view();
    mem.put(&doc(&f, 30, 0)).unwrap();
    let empty = mem.size_in_bytes();
    let first = mem.view();
    let words = first.keywords(f.sku).unwrap();
    let keys = first.keys(f.price).unwrap();
    let cached = mem.size_in_bytes();
    assert!(cached > empty, "{cached} > {empty}");
    // A delete: other operations, the same documents. The copy is the one shared, not a new one.
    mem.delete("d4").unwrap();
    let second = mem.view();
    assert_ne!(second.version(), first.version());
    assert!(std::ptr::eq(
        words.value(0).unwrap(),
        second.keywords(f.sku).unwrap().value(0).unwrap()
    ));
    let mut again = second.keys(f.price).unwrap();
    let mut keys = keys;
    for docid in 0..31 {
        assert_eq!(again.key(docid).unwrap(), keys.key(docid).unwrap());
    }
    assert_eq!(mem.size_in_bytes(), cached);
    assert!(again.same_copy(&keys));
    // An older view copies its own (fewer documents) and leaves the shared one.
    let old_words = older.keywords(f.sku).unwrap();
    assert!(!older.keys(f.price).unwrap().same_copy(&keys));
    assert!(mem.view().keys(f.price).unwrap().same_copy(&keys));
    assert!(!std::ptr::eq(
        old_words.value(0).unwrap(),
        words.value(0).unwrap()
    ));
    assert!(std::ptr::eq(
        words.value(0).unwrap(),
        mem.view().keywords(f.sku).unwrap().value(0).unwrap()
    ));
    // More documents: a new copy replaces the shared one.
    mem.put(&doc(&f, 31, 0)).unwrap();
    let newer = mem.view().keywords(f.sku).unwrap();
    let newer_keys = mem.view().keys(f.price).unwrap();
    assert!(!newer_keys.same_copy(&keys));
    assert!(mem.view().keys(f.price).unwrap().same_copy(&newer_keys));
    assert!(!std::ptr::eq(
        newer.value(0).unwrap(),
        words.value(0).unwrap()
    ));
    assert!(std::ptr::eq(
        newer.value(0).unwrap(),
        mem.view().keywords(f.sku).unwrap().value(0).unwrap()
    ));
    // An unshared view neither reads nor fills it.
    let own = mem.view().unshared().keywords(f.sku).unwrap();
    assert!(!std::ptr::eq(
        own.value(0).unwrap(),
        newer.value(0).unwrap()
    ));
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (0..24_u32, 0..4_u32).prop_map(|(id, v)| Step::Put(id, v)),
        2 => (0..24_u32).prop_map(Step::Delete),
        1 => Just(Step::Commit),
        2 => Just(Step::Read),
        1 => Just(Step::Keep),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Random write/read interleavings: every view returns what a view copying its own columns returns.
    #[test]
    fn random_interleavings_match_own_copies(steps in prop::collection::vec(step(), 1..60)) {
        let mut steps = steps;
        steps.push(Step::Read);
        run(&steps);
    }
}

/// Readers searching on rayon's pool while a writer adds, replaces and deletes documents and flushes: every view a
/// reader takes returns what the same view copying its own columns returns.
#[test]
fn readers_on_the_pool_match_own_copies_while_a_writer_advances() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (db, f) = database();
    let db = Mutex::new(db);
    let done = AtomicBool::new(false);
    let pool = Execution::default().threads(Threads::Global);
    let own_exec = Execution::default().threads(Threads::Current);
    std::thread::scope(|s| {
        for _ in 0..3 {
            s.spawn(|| {
                let mut reads = 0_u32;
                while !done.load(Ordering::Acquire) || reads < 20 {
                    let snapshot = db.lock().unwrap().snapshot();
                    let c = snapshot.collection("c").unwrap();
                    let requests = requests(c.schema(), &f);
                    let memtable = c.memtable();
                    let shared = search(c, &memtable, &requests, &pool);
                    let own = search(c, &memtable.unshared(), &requests, &own_exec);
                    assert_eq!(shared, own, "memtable version {}", memtable.version());
                    reads += 1;
                }
            });
        }
        for round in 0..400_u32 {
            let mut db = db.lock().unwrap();
            let id = (round * 7) % 90;
            if round % 11 == 5 {
                db.delete("c", &format!("d{id}"), Durability::Buffered)
                    .unwrap();
            } else {
                db.put("c", &doc(&f, id, round), Durability::Buffered)
                    .unwrap();
            }
            if round % 150 == 149 {
                db.commit().unwrap();
            }
        }
        done.store(true, Ordering::Release);
    });
}
