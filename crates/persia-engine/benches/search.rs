//! Query execution (ROADMAP 7.4): top-10 over one segment of 50 000 synthetic documents, for a term, an AND, an OR
//! (of 3 clauses, and of 50 as a relaxation expansion builds, 7.4.7) and a phrase with the default pruning
//! (block-max WAND), each strategy of 7.4.4 side by side, prefix and fuzzy expansions planned and searched (7.4.12),
//! and a term under filters (7.4.5): a numeric range, a keyword, and a filter alone; and whole requests (7.5): sorted
//! by a column, with a facet, and a page after a cursor. Deterministic input (a fixed-seed generator), so runs
//! compare. Run: `cargo bench -p persia-engine --bench search`.

#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use std::ops::Bound;

use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, SearchHit, SearchRequest, SegmentIds,
    Threads,
};
use persia_engine::query::{
    BoolQuery, Filter, FilterValue, FuzzyQuery, PhraseQuery, PhraseTerm, PrefixQuery, Query,
    TermQuery,
};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::search::{FacetRequest, Order, Projection, Pruning, SearchPlan, Sort, SortKey};
use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
use persia_engine::stats::SegmentStatistics;
use persia_engine::view::SegmentView;
use persia_engine::wal::Durability;
use persia_format::ByteSource;
use persia_storage::container::sim::SimFile;

const DOCS: u64 = 50_000;
/// Words `w0`.. `w199`, drawn skewed toward low numbers so lists range from long to short.
const VOCABULARY: u64 = 200;

/// xorshift64*: a fixed seed, so every run indexes the same documents.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn word(&mut self) -> u64 {
        let a = self.next() % VOCABULARY;
        let b = self.next() % VOCABULARY;
        a.min(b)
    }
}

/// The benchmark segment, its schema and the fields `body`, `price` and `tag`.
fn segment() -> (Arc<CollectionSchema>, Arc<Segment>, [FieldId; 3]) {
    let schema = SchemaBuilder::new("bench")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field("price", FieldKind::I64 { stored: false })
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let body: FieldId = schema.field("body").unwrap().id();
    let price: FieldId = schema.field("price").unwrap().id();
    let tag: FieldId = schema.field("tag").unwrap().id();
    let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for i in 0..DOCS {
        let len = 5 + rng.next() % 30;
        let text: Vec<String> = (0..len).map(|_| format!("w{}", rng.word())).collect();
        let doc = Document::new(format!("d{i}"))
            .with(body, Value::String(text.join(" ")))
            .with(price, Value::I64(i64::try_from(rng.next() % 1000).unwrap()))
            .with(tag, Value::String(format!("t{}", rng.next() % 10)));
        builder.add(&doc).unwrap();
    }
    let meta = SegmentMeta {
        segment_id: 1,
        created_at: 0,
        merge_generation: 0,
    };
    let bytes = SegmentWriter::new(&builder, meta).finish().unwrap();
    let segment = Arc::new(Segment::open(ByteSource::new(bytes.into()), &schema).unwrap());
    (Arc::new(schema), segment, [body, price, tag])
}

fn bench(c: &mut Criterion) {
    let (schema, segment, fields) = segment();
    let [body, ..] = fields;
    let stats = SegmentStatistics::new(&schema, &[Arc::clone(&segment)]);
    let view = SegmentView::new(&segment, None);

    let term = |w: &str| {
        Query::Term(TermQuery {
            field: body,
            term: w.into(),
        })
    };
    // 50 clauses, as a relaxation expansion builds (SPEC §7.6): every fourth word, common to rare.
    let or50 = Query::Bool(BoolQuery {
        should: (0..50).map(|i| term(&format!("w{}", i * 4))).collect(),
        ..BoolQuery::default()
    });
    let queries = [
        ("term", term("w3")),
        (
            "and",
            Query::Bool(BoolQuery {
                must: vec![term("w3"), term("w40")],
                ..BoolQuery::default()
            }),
        ),
        (
            "or",
            Query::Bool(BoolQuery {
                should: vec![term("w3"), term("w40"), term("w120")],
                ..BoolQuery::default()
            }),
        ),
        ("or50", or50.clone()),
        (
            "phrase",
            Query::Phrase(PhraseQuery {
                field: body,
                terms: ["w0", "w1"]
                    .iter()
                    .zip(0..)
                    .map(|(t, position)| PhraseTerm {
                        term: (*t).into(),
                        position,
                    })
                    .collect(),
            }),
        ),
    ];
    let mut group = c.benchmark_group("search_top10");
    for (name, query) in &queries {
        let plan = SearchPlan::new(query, &schema, &stats).unwrap();
        group.bench_function(*name, |b| {
            b.iter(|| black_box(plan.top_k(&view, 10).unwrap()));
        });
    }
    group.finish();

    // The strategies side by side; `or_rare` is a common word beside two rare ones.
    let or_rare = Query::Bool(BoolQuery {
        should: vec![term("w3"), term("w150"), term("w190")],
        ..BoolQuery::default()
    });
    let pruned = [
        ("term", &queries[0].1),
        ("or", &queries[2].1),
        ("or_rare", &or_rare),
        ("or50", &or50),
    ];
    let strategies = [
        ("exhaustive", Pruning::Exhaustive),
        ("max_score", Pruning::MaxScore),
        ("block_max_wand", Pruning::BlockMaxWand),
    ];
    let mut group = c.benchmark_group("search_top10_pruning");
    for (name, query) in pruned {
        let plan = SearchPlan::new(query, &schema, &stats).unwrap();
        for (strategy, pruning) in strategies {
            group.bench_function(format!("{name}/{strategy}"), |b| {
                b.iter(|| black_box(plan.top_k_with(&view, 10, pruning).unwrap()));
            });
        }
    }
    group.finish();

    expansions(c, &schema, &stats, &view, body);

    filters(
        c,
        &|q| SearchPlan::new(q, &schema, &stats).unwrap(),
        &view,
        fields,
    );
}

/// Expanded at plan time over the segment's terms (7.4.12), then searched: `w1` starts 111 words, capped at 50; `w15`
/// is one edit from 21.
fn expansions(
    c: &mut Criterion,
    schema: &CollectionSchema,
    stats: &SegmentStatistics,
    view: &SegmentView<'_>,
    body: FieldId,
) {
    let expansions = [
        (
            "prefix",
            Query::Prefix(PrefixQuery {
                field: body,
                prefix: "w1".into(),
                max_expansions: 50,
            }),
        ),
        (
            "fuzzy",
            Query::Fuzzy(FuzzyQuery {
                field: body,
                term: "w15".into(),
                distance: 1,
                max_expansions: 50,
            }),
        ),
    ];
    let mut group = c.benchmark_group("search_top10_expansion");
    for (name, query) in &expansions {
        group.bench_function(*name, |b| {
            b.iter(|| {
                let plan = SearchPlan::with_terms(query, schema, stats, view).unwrap();
                black_box(plan.top_k(view, 10).unwrap())
            });
        });
    }
    group.finish();
}

/// A term under filters: `price < 100` selects about 10 %, `price < 500` about half, a `tag` about 10 %; and a
/// filter alone.
fn filters(
    c: &mut Criterion,
    plan: &dyn Fn(&Query) -> SearchPlan,
    view: &SegmentView<'_>,
    fields: [FieldId; 3],
) {
    let [body, price, tag] = fields;
    let term = Query::Term(TermQuery {
        field: body,
        term: "w3".into(),
    });
    let below = |p: i64| Filter::Range {
        field: price,
        lower: Bound::Unbounded,
        upper: Bound::Excluded(FilterValue::I64(p)),
    };
    let filtered = |must: Vec<Query>, filter: Filter| {
        Query::Bool(BoolQuery {
            must,
            filter: vec![filter],
            ..BoolQuery::default()
        })
    };
    let tagged = Filter::Term {
        field: tag,
        value: FilterValue::Keyword("t3".into()),
    };
    let filters = [
        ("term/range_10pct", filtered(vec![term.clone()], below(100))),
        ("term/range_50pct", filtered(vec![term.clone()], below(500))),
        ("term/keyword", filtered(vec![term], tagged)),
        ("filter_only/range_10pct", filtered(vec![], below(100))),
    ];
    let mut group = c.benchmark_group("search_top10_filtered");
    for (name, query) in &filters {
        let plan = plan(query);
        group.bench_function(*name, |b| {
            b.iter(|| black_box(plan.top_k(view, 10).unwrap()));
        });
    }
    group.finish();
}

/// A collection of the same documents, committed in `segments` segments, for whole requests (ROADMAP 7.5): its
/// fields `body`, `price`, `tag`
/// (`facet: true`).
fn collection(segments: u64) -> (Database<SimFile>, [FieldId; 3]) {
    let schema = SchemaBuilder::new("bench")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("price", FieldKind::I64 { stored: true })
        .field(
            "tag",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .build()
        .unwrap();
    let id = |name| schema.field(name).unwrap().id();
    let (body, price, tag) = (id("body"), id("price"), id("tag"));
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [1; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for i in 0..DOCS {
        let len = 5 + rng.next() % 30;
        let text: Vec<String> = (0..len).map(|_| format!("w{}", rng.word())).collect();
        let doc = Document::new(format!("d{i}"))
            .with(body, Value::String(text.join(" ")))
            .with(price, Value::I64(i64::try_from(rng.next() % 1000).unwrap()))
            .with(tag, Value::String(format!("t{}", rng.next() % 10)));
        db.put("bench", &doc, Durability::Buffered).unwrap();
        if (i + 1) % (DOCS / segments) == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    (db, [body, price, tag])
}

/// Whole requests over a committed collection: 10 hits of a term by score (pruned) and by a column, a facet beside
/// them (every match visited), and a deep page by cursor.
fn requests(c: &mut Criterion) {
    let (db, [body, price, tag]) = collection(1);
    let snapshot = db.snapshot();
    let coll = snapshot.collection("bench").unwrap();
    let memtable = coll.memtable();
    let schema = coll.schema();
    let views = coll.views(&memtable).unwrap();
    let query = Query::Term(TermQuery {
        field: body,
        term: "w3".into(),
    });
    let plan = SearchPlan::new(&query, schema, &coll.search_statistics(&memtable)).unwrap();
    let by_price = Sort::new([SortKey::field(price, Order::Asc)], schema).unwrap();
    let facet = vec![FacetRequest::new(tag, 10, schema).unwrap()];
    let page = |sort: &Sort, facets: &[FacetRequest]| SearchRequest {
        sort: sort.clone(),
        limit: 10,
        facets: facets.to_vec(),
        projection: Projection::None,
        ..SearchRequest::default()
    };
    let deep = views
        .search(
            &plan,
            &SearchRequest {
                offset: 990,
                limit: 10,
                ..page(&by_price, &[])
            },
            Pruning::default(),
        )
        .unwrap();
    let cursor = deep.hits.last().map(SearchHit::cursor);
    let requests = [
        ("score", page(&Sort::score(), &[])),
        ("sort_price", page(&by_price, &[])),
        ("score_facet", page(&Sort::score(), &facet)),
        (
            "sort_price_after_1000",
            SearchRequest {
                search_after: cursor,
                ..page(&by_price, &[])
            },
        ),
    ];
    let mut group = c.benchmark_group("search_request");
    for (name, request) in &requests {
        group.bench_function(*name, |b| {
            b.iter(|| black_box(views.search(&plan, request, Pruning::default()).unwrap()));
        });
    }
    group.finish();
}

/// The same collection in 4 segments: 10 hits by score and by a column, the views on the calling thread and on rayon's
/// global pool (ROADMAP 7.6.1).
fn parallel(c: &mut Criterion) {
    let (db, [body, price, _]) = collection(4);
    let snapshot = db.snapshot();
    let coll = snapshot.collection("bench").unwrap();
    assert_eq!(coll.segments().len(), 4);
    let memtable = coll.memtable();
    let schema = coll.schema();
    let views = coll.views(&memtable).unwrap();
    let query = Query::Term(TermQuery {
        field: body,
        term: "w3".into(),
    });
    let plan = SearchPlan::new(&query, schema, &coll.search_statistics(&memtable)).unwrap();
    let by_price = Sort::new([SortKey::field(price, Order::Asc)], schema).unwrap();
    let mut group = c.benchmark_group("search_parallel");
    for (name, sort) in [("score", Sort::score()), ("sort_price", by_price)] {
        let request = SearchRequest {
            sort,
            limit: 10,
            projection: Projection::None,
            ..SearchRequest::default()
        };
        for (threads_name, threads) in [("current", Threads::Current), ("global", Threads::Global)]
        {
            let exec = Execution::default().threads(threads);
            group.bench_function(format!("{name}/{threads_name}"), |b| {
                b.iter(|| black_box(views.search_with(&plan, &request, &exec).unwrap()));
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench, requests, parallel);
criterion_main!(benches);
