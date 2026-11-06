//! Requests over a `keyword` column with a large dictionary (ROADMAP 7.5.5): 40 000 documents committed in 4 segments
//! plus 5 000 in the memtable, each with its own `sku` (so every segment's dictionary holds 10 000 values) and a
//! `price`. Each request takes a fresh memtable view and views, as a request does: 10 hits of `w3` sorted by `sku`,
//! by score with a `sku` facet, both at once, and sorted by `price`, every view on the calling thread
//! (`search_keyword_columns`) and on rayon's global pool (`search_keyword_columns_pool`, ROADMAP 7.5.7). Deterministic
//! input (a fixed-seed generator). `search_keyword_columns_write` upserts one memtable document before each `sort_sku`
//! request, so each request copies the memtable's columns again.
//! Run: `cargo bench -p persia-engine --bench keyword_columns`.

#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, SearchRequest, SegmentIds, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{Query, TermQuery};
use persia_engine::schema::{FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
use persia_engine::search::{FacetRequest, Order, Projection, SearchPlan, Sort, SortKey};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

const SEGMENTS: u64 = 4;
const PER_SEGMENT: u64 = 10_000;
const IN_MEMTABLE: u64 = 5_000;

/// xorshift64*: a fixed seed, so every run indexes the same documents.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// The database the requests run on, and its `body`, `price` and `sku` fields.
fn database() -> (Database<SimFile>, [FieldId; 3]) {
    let schema = SchemaBuilder::new("bench")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("price", FieldKind::I64 { stored: false })
        .field(
            "sku",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .build()
        .unwrap();
    let id = |name| schema.field(name).unwrap().id();
    let (body, price, sku) = (id("body"), id("price"), id("sku"));
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [1; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for i in 0..SEGMENTS * PER_SEGMENT + IN_MEMTABLE {
        let words: Vec<String> = (0..8).map(|_| format!("w{}", rng.next() % 20)).collect();
        let doc = Document::new(format!("d{i}"))
            .with(body, Value::String(words.join(" ")))
            .with(price, Value::I64(i64::try_from(rng.next() % 1000).unwrap()))
            .with(sku, Value::String(format!("sku-{:016x}", rng.next())));
        db.put("bench", &doc, Durability::Buffered).unwrap();
        if (i + 1) % PER_SEGMENT == 0 && i < SEGMENTS * PER_SEGMENT {
            db.commit().unwrap();
        }
    }
    (db, [body, price, sku])
}

fn bench(c: &mut Criterion) {
    let (mut db, [body, price, sku]) = database();
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
    // document more than the last, so it copies its columns again and replaces the shared copy (ROADMAP 7.5.7). The
    // memtable grows by one docid per iteration, the same for every build of this bench.
    let sort_sku = &requests[0].1;
    let mut upserts = 0_u64;
    let mut group = c.benchmark_group("search_keyword_columns_write");
    for (name, exec) in [
        ("sort_sku", Execution::default().threads(Threads::Current)),
        (
            "sort_sku_pool",
            Execution::default().threads(Threads::Global),
        ),
    ] {
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let i = SEGMENTS * PER_SEGMENT + upserts % IN_MEMTABLE;
                    upserts += 1;
                    let doc = Document::new(format!("d{i}"))
                        .with(body, Value::String(format!("w3 w{}", upserts % 20)))
                        .with(price, Value::I64(i64::try_from(upserts % 1000).unwrap()))
                        .with(sku, Value::String(format!("sku-{upserts:016x}")));
                    db.put("bench", &doc, Durability::Buffered).unwrap();
                },
                |()| {
                    let memtable = coll.memtable();
                    let stats = coll.search_statistics(&memtable);
                    let plan = SearchPlan::new(&query, schema, &stats).unwrap();
                    let views = coll.views(&memtable).unwrap();
                    black_box(views.search_with(&plan, sort_sku, &exec).unwrap())
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
