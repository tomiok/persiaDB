//! Requests over a `keyword` column with a large dictionary (ROADMAP 7.5.5): 40 000 documents committed in 4 segments
//! plus 5 000 in the memtable, each with its own `sku` (so every segment's dictionary holds 10 000 values) and a
//! `price`. Each request takes a fresh memtable view and views, as a request does: 10 hits of `w3` sorted by `sku`,
//! by score with a `sku` facet, both at once, and sorted by `price`, every view on the calling thread
//! (`search_keyword_columns`) and on rayon's global pool (`search_keyword_columns_pool`, ROADMAP 7.5.7). Deterministic
//! input (a fixed-seed generator). `search_keyword_columns_write` upserts one memtable document before each `sort_sku`
//! request, so each request copies the memtable's columns again.
//! Run: `cargo bench -p persia-engine --bench keyword_columns`.

#![allow(missing_docs, clippy::unwrap_used)]

#[path = "support/keyword_columns.rs"]
mod support;

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use persia_engine::db::{Execution, SearchRequest, Threads};
use persia_engine::query::{Query, TermQuery};
use persia_engine::search::{FacetRequest, Order, Projection, SearchPlan, Sort, SortKey};
use support::{Fields, Shape, SortSku, database, upsert};

const SHAPE: Shape = Shape {
    segments: 4,
    per_segment: 10_000,
    in_memtable: 5_000,
};

fn bench(c: &mut Criterion) {
    let (db, Fields { body, price, sku }) = database(&SHAPE);
    let snapshot = db.snapshot();
    let coll = snapshot.collection("bench").unwrap();
    assert_eq!(coll.segments().len(), 4);
    let schema = coll.schema();
    let query = Query::Term(TermQuery {
        field: body,
        term: "w3".into(),
    });
    let by_sku = Sort::new([SortKey::field(sku, Order::Asc)], schema).unwrap();
    let by_price = Sort::new([SortKey::field(price, Order::Asc)], schema).unwrap();
    let facet = vec![FacetRequest::new(sku, 10, schema).unwrap()];
    let page = |sort: &Sort, facets: &[FacetRequest]| SearchRequest {
        sort: sort.clone(),
        limit: 10,
        facets: facets.to_vec(),
        projection: Projection::None,
        ..SearchRequest::default()
    };
    let requests = [
        ("sort_sku", page(&by_sku, &[])),
        ("score_facet_sku", page(&Sort::score(), &facet)),
        ("sort_sku_facet_sku", page(&by_sku, &facet)),
        ("sort_price", page(&by_price, &[])),
    ];
    // On the calling thread the time is the work of every view; on rayon's pool, the slowest view's (the memtable's,
    // here) and the pool's dispatch.
    let execs = [
        (
            "search_keyword_columns",
            Execution::default().threads(Threads::Current),
        ),
        (
            "search_keyword_columns_pool",
            Execution::default().threads(Threads::Global),
        ),
    ];
    for (group_name, exec) in &execs {
        let mut group = c.benchmark_group(*group_name);
        for (name, request) in &requests {
            group.bench_function(*name, |b| {
                b.iter(|| {
                    let memtable = coll.memtable();
                    let stats = coll.search_statistics(&memtable);
                    let plan = SearchPlan::new(&query, schema, &stats).unwrap();
                    let views = coll.views(&memtable).unwrap();
                    black_box(views.search_with(&plan, request, exec).unwrap())
                });
            });
        }
        group.finish();
    }
    // One upsert of a memtable document before each request (not timed): every request's memtable view has one
    // document more than the last, so it copies its columns again and replaces the shared copy (ROADMAP 7.5.7). Each
    // mode starts from a fresh database, and each request reads the snapshot after its write: one taken before would
    // miss the writes, and any flush they cause. An upsert replaces a document, so the memtable grows by one docid per
    // iteration, and criterion picks the iteration count from the elapsed time: a faster build runs more, so its later
    // requests copy more documents. Compare two builds only at equal iteration counts (criterion prints them).
    drop((snapshot, db));
    let mut group = c.benchmark_group("search_keyword_columns_write");
    for (name, exec) in [
        ("sort_sku", Execution::default().threads(Threads::Current)),
        (
            "sort_sku_pool",
            Execution::default().threads(Threads::Global),
        ),
    ] {
        let (mut db, fields) = database(&SHAPE);
        let request = SortSku::new(&db.snapshot(), fields);
        let mut upserts = 0_u64;
        group.bench_function(name, |b| {
            b.iter_batched_ref(
                || {
                    upserts += 1;
                    upsert(&mut db, fields, &SHAPE, upserts)
                },
                |snapshot| black_box(request.run(snapshot, &exec)),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
