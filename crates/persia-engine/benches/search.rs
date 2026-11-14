//! Query execution (ROADMAP 7.4): top-10 over one segment of 50 000 synthetic documents, for a term, an AND, an OR
//! (of 3 clauses, and of 50 as a relaxation expansion builds, 7.4.7) and a phrase with the default pruning
//! (block-max WAND), each strategy of 7.4.4 side by side, prefix and fuzzy expansions planned and searched (7.4.12),
//! and a term under filters (7.4.5): a numeric range, a keyword, and a filter alone; and whole requests (7.5): sorted
//! by a column, with a facet, and a page after a cursor; and the query suite of 7.8.3 end to end (`search_suite`); and
//! a request through the relaxation cascade when L0 has enough hits (`search_relaxed`, 8.1.2).
//! Deterministic input (a fixed-seed generator), so runs compare. Run: `cargo bench -p persia-engine --bench search`.

#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use std::ops::Bound;

use persia_engine::db::{
    CancelToken, Clock, Database, DatabaseOptions, Execution, ManualClock, SearchHit,
    SearchRequest, SegmentIds, SystemClock, Threads,
};
use persia_engine::query::{
    BoolQuery, DefaultFields, Filter, FilterValue, FuzzyQuery, PhraseQuery, PhraseTerm,
    PrefixQuery, Query, QueryCompiler, QueryLimits, TermQuery,
};
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, Level, Levels, RelaxationConfig,
    SchemaBuilder, TextOptions,
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

/// Document `i`'s `at`: one second apart, from late 2023 (microseconds).
fn time(i: u64) -> i64 {
    1_700_000_000_000_000 + i64::try_from(i).unwrap() * 1_000_000
}

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

/// The benchmark segment, its schema and the fields `body`, `price`, `tag` and `at` (a datetime that grows with
/// the docid, as an insertion time does, so its FOR blocks bound narrow ranges: ROADMAP 7.4.13, 7.5.6).
fn segment() -> (Arc<CollectionSchema>, Arc<Segment>, [FieldId; 4]) {
    let schema = SchemaBuilder::new("bench")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field("price", FieldKind::I64 { stored: false })
        .field("at", FieldKind::Datetime { stored: false })
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let body: FieldId = schema.field("body").unwrap().id();
    let price: FieldId = schema.field("price").unwrap().id();
    let tag: FieldId = schema.field("tag").unwrap().id();
    let at: FieldId = schema.field("at").unwrap().id();
    let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for i in 0..DOCS {
        let len = 5 + rng.next() % 30;
        let text: Vec<String> = (0..len).map(|_| format!("w{}", rng.word())).collect();
        let doc = Document::new(format!("d{i}"))
            .with(body, Value::String(text.join(" ")))
            .with(price, Value::I64(i64::try_from(rng.next() % 1000).unwrap()))
            .with(tag, Value::String(format!("t{}", rng.next() % 10)))
            .with(at, Value::Datetime(time(i)));
        builder.add(&doc).unwrap();
    }
    let meta = SegmentMeta {
        segment_id: 1,
        created_at: 0,
        merge_generation: 0,
    };
    let bytes = SegmentWriter::new(&builder, meta).finish().unwrap();
    let segment = Arc::new(Segment::open(ByteSource::new(bytes.into()), &schema).unwrap());
    (Arc::new(schema), segment, [body, price, tag, at])
}

/// The pruning strategies `search_top10_pruning` runs side by side.
const STRATEGIES: [(&str, Pruning); 3] = [
    ("exhaustive", Pruning::Exhaustive),
    ("max_score", Pruning::MaxScore),
    ("block_max_wand", Pruning::BlockMaxWand),
];

/// `search_top10/and_proximity`: `and` with the proximity bonus of its two words (ROADMAP 7.3.5, off by default; the
/// body has positions), so the bonus's cost shows beside `and`.
fn with_proximity(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    and: SearchPlan,
    view: &SegmentView<'_>,
) {
    let and = and.with_proximity();
    group.bench_function("and_proximity", |b| {
        b.iter(|| black_box(and.top_k(view, 10).unwrap()));
    });
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
    let and = SearchPlan::new(&queries[1].1, &schema, &stats).unwrap();
    with_proximity(&mut group, and, &view);
    group.finish();

    // The strategies side by side; `or_rare` is a common word beside two rare ones.
    let or_rare = Query::Bool(BoolQuery {
        should: vec![term("w3"), term("w150"), term("w190")],
        ..BoolQuery::default()
    });
    let [and_common, must_should, must_should_rare] = conjunctions(&term);
    let [or4, or4_msm2, or4_msm3, or50_msm2] = at_least(&term);
    let pruned = [
        ("or4", &or4),
        ("or4_msm2", &or4_msm2),
        ("or4_msm3", &or4_msm3),
        ("or50_msm2", &or50_msm2),
        ("term", &queries[0].1),
        ("or", &queries[2].1),
        ("or_rare", &or_rare),
        ("or50", &or50),
        ("and", &queries[1].1),
        ("and_common", &and_common),
        ("must_should", &must_should),
        ("must_should_rare", &must_should_rare),
    ];
    let mut group = c.benchmark_group("search_top10_pruning");
    for (name, query) in pruned {
        let plan = SearchPlan::new(query, &schema, &stats).unwrap();
        for (strategy, pruning) in STRATEGIES {
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
    filter_sweep(
        c,
        &|q| SearchPlan::new(q, &schema, &stats).unwrap(),
        &view,
        fields,
    );
}

/// L2 `drop_terms` (ROADMAP 8.1.6): four words, common to rare, any of them, then at least 2 and 3 of them; and
/// `or50` with at least 2.
fn at_least(term: &impl Fn(&str) -> Query) -> [Query; 4] {
    let of = |should: Vec<Query>, minimum_should_match| {
        Query::Bool(BoolQuery {
            should,
            minimum_should_match,
            ..BoolQuery::default()
        })
    };
    let four = || ["w3", "w20", "w60", "w120"].map(term).to_vec();
    let fifty = (0..50).map(|i| term(&format!("w{}", i * 4))).collect();
    [of(four(), 0), of(four(), 2), of(four(), 3), of(fifty, 2)]
}

/// Conjunctions and `should` beside `must` (ROADMAP 7.4.8): two common words; a common `must` beside rarer
/// `should`s; the commonest word beside two rare ones, which must hold one of them to beat a warm threshold.
fn conjunctions(term: &impl Fn(&str) -> Query) -> [Query; 3] {
    [
        Query::Bool(BoolQuery {
            must: vec![term("w0"), term("w1")],
            ..BoolQuery::default()
        }),
        Query::Bool(BoolQuery {
            must: vec![term("w3")],
            should: vec![term("w40"), term("w120")],
            ..BoolQuery::default()
        }),
        Query::Bool(BoolQuery {
            must: vec![term("w0")],
            should: vec![term("w150"), term("w190")],
            ..BoolQuery::default()
        }),
    ]
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
/// filter alone. `at` in its last 10 % selects whole FOR blocks (ROADMAP 7.4.13), and a rare term (`w150`, in about
/// 5 % of the documents) is what a filter checked per candidate is for.
fn filters(
    c: &mut Criterion,
    plan: &dyn Fn(&Query) -> SearchPlan,
    view: &SegmentView<'_>,
    fields: [FieldId; 4],
) {
    let [body, price, tag, at] = fields;
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
    let recent = Filter::Range {
        field: at,
        lower: Bound::Included(FilterValue::Datetime(time(DOCS / 10 * 9))),
        upper: Bound::Unbounded,
    };
    let rare = Query::Term(TermQuery {
        field: body,
        term: "w150".into(),
    });
    let filters = [
        ("term/range_10pct", filtered(vec![term.clone()], below(100))),
        ("term/range_50pct", filtered(vec![term.clone()], below(500))),
        ("term/keyword", filtered(vec![term.clone()], tagged.clone())),
        ("filter_only/range_10pct", filtered(vec![], below(100))),
        (
            "term/recent_10pct",
            filtered(vec![term.clone()], recent.clone()),
        ),
        ("filter_only/recent_10pct", filtered(vec![], recent)),
        ("rare_term/range_10pct", filtered(vec![rare], below(100))),
        (
            "term/keyword_range_10pct",
            Query::Bool(BoolQuery {
                must: vec![term.clone()],
                filter: vec![tagged.clone(), below(100)],
                ..BoolQuery::default()
            }),
        ),
    ];
    let mut group = c.benchmark_group("search_top10_filtered");
    for (name, query) in &filters {
        let plan = plan(query);
        group.bench_function(*name, |b| {
            b.iter(|| black_box(plan.top_k(view, 10).unwrap()));
        });
    }
    // The last one as before 7.4.16: both filters selected as one bitmap.
    if let Some((name, query)) = filters.last() {
        let plan = plan(query).with_check_share(0);
        group.bench_function(format!("{name}/bitmap"), |b| {
            b.iter(|| black_box(plan.top_k(view, 10).unwrap()));
        });
    }
    group.finish();
}

/// The per-candidate threshold's sweep (ROADMAP 7.4.16): `must` clauses from rare to frequent (single words, then
/// disjunctions of the most common word with others), under `price < 100` alone and beside the `keyword` `tag = t3`,
/// each with its column filter selected as a bitmap (`bitmap`, share 0) and checked per candidate (`check`, share 1).
/// Each id names the `must`'s cost as a per mille of the view's documents. Where `check` stops winning fits the
/// engine's check shares.
fn filter_sweep(
    c: &mut Criterion,
    plan: &dyn Fn(&Query) -> SearchPlan,
    view: &SegmentView<'_>,
    fields: [FieldId; 4],
) {
    use persia_engine::search::Matcher;
    use persia_engine::view::SearchView;
    let [body, price, tag, _] = fields;
    let term = |w: u64| {
        Query::Term(TermQuery {
            field: body,
            term: format!("w{w}"),
        })
    };
    let mut musts: Vec<Query> = [199, 170, 140, 110, 80, 50, 25, 10, 0]
        .into_iter()
        .map(term)
        .collect();
    for words in [
        &[150, 190][..],
        &[80, 190],
        &[50, 150],
        &[25, 120],
        &[0, 190],
        &[0, 120],
        &[0, 60],
        &[0, 20],
        &[0, 1],
        &[0, 1, 2],
        &[0, 1, 2, 3],
    ] {
        musts.push(Query::Bool(BoolQuery {
            should: words.iter().copied().map(term).collect(),
            ..BoolQuery::default()
        }));
    }
    let range = Filter::Range {
        field: price,
        lower: Bound::Unbounded,
        upper: Bound::Excluded(FilterValue::I64(100)),
    };
    let keyword = Filter::Term {
        field: tag,
        value: FilterValue::Keyword("t3".into()),
    };
    let filters = [
        ("range_10pct", vec![range.clone()]),
        ("keyword_range_10pct", vec![keyword, range]),
    ];
    let mut group = c.benchmark_group("search_top10_filtered_sweep");
    for must in &musts {
        let cost = plan(must).matcher(view).unwrap().cost();
        let per_mille = cost * 1000 / u64::from(view.doc_count());
        let shape = if matches!(must, Query::Bool(_)) {
            "or"
        } else {
            "term"
        };
        for (name, filter) in &filters {
            let query = Query::Bool(BoolQuery {
                must: vec![must.clone()],
                filter: filter.clone(),
                ..BoolQuery::default()
            });
            for (mode, share) in [("bitmap", 0), ("check", 1)] {
                let plan = plan(&query).with_check_share(share);
                group.bench_function(format!("{name}/{per_mille:04}/{shape}/{mode}"), |b| {
                    b.iter(|| black_box(plan.top_k(view, 10).unwrap()));
                });
            }
        }
    }
    group.finish();
}
/// A collection of the same documents, committed in `segments` segments, for whole requests (ROADMAP 7.5): its
/// fields `body` (with `positions` when asked), `price`, `tag` (`facet: true`) and `at`.
fn collection(segments: u64, positions: bool) -> (Database<SimFile>, [FieldId; 4]) {
    let schema = bench_schema(positions, Vec::new());
    let id = |name| schema.field(name).unwrap().id();
    let (body, price, tag, at) = (id("body"), id("price"), id("tag"), id("at"));
    collection_of(schema, segments, [body, price, tag, at])
}

/// The bench collection's schema, with `synonyms` (which a schema change may change freely, SPEC §6.2.4).
fn bench_schema(positions: bool, synonyms: Vec<Vec<String>>) -> CollectionSchema {
    SchemaBuilder::new("bench")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                positions,
                ..TextOptions::default()
            }),
        )
        .field("price", FieldKind::I64 { stored: true })
        .field("at", FieldKind::Datetime { stored: false })
        .field(
            "tag",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .synonyms(synonyms)
        .build()
        .unwrap()
}

fn collection_of(
    schema: CollectionSchema,
    segments: u64,
    [body, price, tag, at]: [FieldId; 4],
) -> (Database<SimFile>, [FieldId; 4]) {
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
            .with(tag, Value::String(format!("t{}", rng.next() % 10)))
            .with(at, Value::Datetime(time(i)));
        db.put("bench", &doc, Durability::Buffered).unwrap();
        if (i + 1) % (DOCS / segments) == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    (db, [body, price, tag, at])
}

/// Whole requests over a committed collection: 10 hits of a term by score (pruned, and with a later key), by a column of random values and
/// by one that grows with the docid (oldest first: whole FOR blocks lose, ROADMAP 7.5.6; newest first: none can), a facet beside them (every
/// match visited), and a deep page by cursor.
fn requests(c: &mut Criterion) {
    let (db, [body, price, tag, at]) = collection(1, false);
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
    let newest = Sort::new([SortKey::field(at, Order::Desc)], schema).unwrap();
    let oldest = Sort::new([SortKey::field(at, Order::Asc)], schema).unwrap();
    // A tie-break after the score (ROADMAP 7.4.11): pruned like the score alone, ties at the threshold compared.
    let score_newest =
        Sort::new([SortKey::score(), SortKey::field(at, Order::Desc)], schema).unwrap();
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
        ("score_then_newest", page(&score_newest, &[])),
        ("sort_price", page(&by_price, &[])),
        ("sort_newest", page(&newest, &[])),
        ("sort_oldest", page(&oldest, &[])),
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
    let (db, [body, price, ..]) = collection(4, false);
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
        // The cost of a deadline that never passes: the system clock read every 256 matches and 1024 reads of a
        // view's readers (ROADMAP 7.6.3).
        let far = SystemClock.now() + std::time::Duration::from_secs(3600);
        let exec = Execution::default()
            .threads(Threads::Current)
            .deadline(Arc::new(SystemClock), far)
            .cancel(CancelToken::new());
        group.bench_function(format!("{name}/current_deadline"), |b| {
            b.iter(|| black_box(views.search_with(&plan, &request, &exec).unwrap()));
        });
    }
    group.finish();
}

/// The query suite (ROADMAP 7.8.3), each a whole request end to end over 4 segments, on the default pool: search,
/// merge, and the page's 10 documents read (planned once, outside the loop). `term` is `w3`, `and` is `w3 w40` as
/// user text (SPEC §7.1.1), `or` is `w3 OR w40 OR w120`, `phrase` is `"w0 w1"`, `filtered` is `w3` with
/// `price < 100`, `sorted` is `w3` by price, `facets` is `w3` with a `tag` facet, `explain` is `term` with every hit
/// explained (SPEC §7.7), and `explain_filtered` is `filtered` explained, one pass per view reusing the
/// search's filter selection (ROADMAP 7.7.2, 7.7.3).
fn suite(c: &mut Criterion) {
    let (db, [body, price, tag, _]) = collection(4, true);
    let snapshot = db.snapshot();
    let coll = snapshot.collection("bench").unwrap();
    let memtable = coll.memtable();
    let schema = coll.schema();
    let views = coll.views(&memtable).unwrap();
    let stats = coll.search_statistics(&memtable);
    let compiler = QueryCompiler::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    let fields = DefaultFields::all(schema);
    let term = |w: &str| {
        Query::Term(TermQuery {
            field: body,
            term: w.into(),
        })
    };
    let page = SearchRequest {
        limit: 10,
        ..SearchRequest::default()
    };
    let filtered = Query::Bool(BoolQuery {
        must: vec![term("w3")],
        filter: vec![Filter::Range {
            field: price,
            lower: Bound::Unbounded,
            upper: Bound::Excluded(FilterValue::I64(100)),
        }],
        ..BoolQuery::default()
    });
    let cases = [
        ("term", term("w3"), page.clone()),
        (
            "and",
            compiler
                .compile("w3 w40", &fields, &QueryLimits::default())
                .query,
            page.clone(),
        ),
        (
            "or",
            Query::Bool(BoolQuery {
                should: vec![term("w3"), term("w40"), term("w120")],
                ..BoolQuery::default()
            }),
            page.clone(),
        ),
        (
            "phrase",
            compiler
                .compile("\"w0 w1\"", &fields, &QueryLimits::default())
                .query,
            page.clone(),
        ),
        ("filtered", filtered.clone(), page.clone()),
        (
            "sorted",
            term("w3"),
            SearchRequest {
                sort: Sort::new([SortKey::field(price, Order::Asc)], schema).unwrap(),
                ..page.clone()
            },
        ),
        (
            "facets",
            term("w3"),
            SearchRequest {
                facets: vec![FacetRequest::new(tag, 10, schema).unwrap()],
                ..page.clone()
            },
        ),
        (
            "explain",
            term("w3"),
            SearchRequest {
                explain: true,
                ..page.clone()
            },
        ),
        (
            "explain_filtered",
            filtered.clone(),
            SearchRequest {
                explain: true,
                ..page
            },
        ),
    ];
    let mut group = c.benchmark_group("search_suite");
    for (name, query, request) in &cases {
        let plan = SearchPlan::new(query, schema, &stats).unwrap();
        let hits = views
            .search(&plan, request, Pruning::default())
            .unwrap()
            .hits;
        assert_eq!(hits.len(), 10, "{name}");
        group.bench_function(*name, |b| {
            b.iter(|| black_box(views.search(&plan, request, Pruning::default()).unwrap()));
        });
    }
    group.finish();
}

/// `prefix` and `fuzzy` planned over a collection's views (ROADMAP 7.4.14): 4 committed segments of the benchmark
/// documents and a memtable of 10 000 more, so every term is listed by five views, and `w1`'s 111 terms lose the cap
/// of 50 to the memtable's counts too. Planning only: the expansion walk and its `df` lookups.
fn collection_expansions(c: &mut Criterion) {
    let (mut db, [body, price, tag, _]) = collection(4, false);
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for i in 0..10_000 {
        let len = 5 + rng.next() % 30;
        let text: Vec<String> = (0..len).map(|_| format!("w{}", rng.word())).collect();
        let doc = Document::new(format!("m{i}"))
            .with(body, Value::String(text.join(" ")))
            .with(price, Value::I64(i64::try_from(rng.next() % 1000).unwrap()))
            .with(tag, Value::String(format!("t{}", rng.next() % 10)));
        db.put("bench", &doc, Durability::Buffered).unwrap();
    }
    let snapshot = db.snapshot();
    let coll = snapshot.collection("bench").unwrap();
    assert_eq!(coll.segments().len(), 4);
    let memtable = coll.memtable();
    assert_eq!(memtable.live_count(), 10_000);
    let schema = coll.schema();
    let views = coll.views(&memtable).unwrap();
    let stats = coll.search_statistics(&memtable);
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
        // A 1 KiB term at distance 2 (ROADMAP 8.2.4.1): no term is within it, but the walk still steps its automaton
        // through every term the first few bytes do not rule out.
        (
            "fuzzy_long",
            Query::Fuzzy(FuzzyQuery {
                field: body,
                term: format!("w15{}", "x".repeat(1021)),
                distance: 2,
                max_expansions: 50,
            }),
        ),
    ];
    let mut group = c.benchmark_group("search_expansion_collection");
    for (name, query) in &expansions {
        group.bench_function(format!("{name}/plan"), |b| {
            b.iter(|| black_box(SearchPlan::with_terms(query, schema, &stats, &views).unwrap()));
        });
    }
    group.finish();
}

/// The relaxation cascade at L0 (ROADMAP 8.1.2): a request whose query has `min_results` hits at L0, over 4 segments
/// on the calling thread, planned and searched each iteration, as `plain` (`SearchPlan::with_terms`, then
/// `search_with`), through `search_relaxed` with the collection's levels, and with `relax: false`. The three should
/// cost the same: the cascade builds no later level when L0 has enough hits.
///
/// `l2` and `l2_every_step` (ROADMAP 8.2.2) relax a five-word query no document holds whole (`zz` is in none) to L2
/// `drop_terms`: the importance `df` reads, then the `minimum_should_match` steps, each excluding L0 and the steps
/// before it. `l2` stops at the first step with a hit; `l2_every_step` (the largest `min_results`) reaches each of
/// them, down to `drop_terms:1`. `l3` (ROADMAP 8.2.3) runs L0 and L3 alone on a two-word query whose last word starts
/// every term; `l4` (ROADMAP 8.2.4) L0 and L4 alone on a two-word query whose second word is a typo.
fn relaxed(c: &mut Criterion) {
    let (db, _) = collection(4, false);
    let snapshot = db.snapshot();
    let coll = snapshot.collection("bench").unwrap();
    let memtable = coll.memtable();
    let schema = coll.schema();
    let views = coll.views(&memtable).unwrap();
    let query = compile_text(schema, "w3 w40");
    let request = SearchRequest {
        limit: 10,
        ..SearchRequest::default()
    };
    let exec = Execution::default().threads(Threads::Current);
    let relax = RelaxPlan::new(schema.relaxation(), &RelaxOptions::default()).unwrap();
    let off = RelaxPlan::new(schema.relaxation(), &RelaxOptions::disabled()).unwrap();
    let mut group = c.benchmark_group("search_relaxed");
    group.bench_function("plain", |b| {
        b.iter(|| {
            let stats = coll.search_statistics(&memtable);
            let plan = SearchPlan::with_terms(&query, schema, &stats, &exec.terms(&views)).unwrap();
            black_box(views.search_with(&plan, &request, &exec).unwrap())
        });
    });
    for (name, relax) in [("l0", &relax), ("off", &off)] {
        group.bench_function(name, |b| {
            b.iter(|| {
                black_box(
                    views
                        .search_relaxed(&query, &request, relax, &exec)
                        .unwrap(),
                )
            });
        });
    }
    let compiler = QueryCompiler::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    let missing = compiler.compile(
        "w3 w40 w7 w120 zz",
        &DefaultFields::all(schema),
        &QueryLimits::default(),
    );
    let every = RelaxPlan::new(
        schema.relaxation(),
        &RelaxOptions {
            min_results: Some(*RelaxationConfig::MIN_RESULTS.end()),
            ..RelaxOptions::default()
        },
    )
    .unwrap();
    for (name, relax) in [("l2", &relax), ("l2_every_step", &every)] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let out = views
                    .search_relaxed(&missing, &request, relax, &exec)
                    .unwrap();
                assert_eq!(out.relaxation.level, RelaxLevel::DropTerms);
                black_box(out)
            });
        });
    }
    // L1 (ROADMAP 8.2.1): `nosuch` is in no document, its synonyms `w40` and `w120` are; `w3` has 50 alternatives
    // in none, among 2 000 other groups.
    let synonyms = synonym_compiler(schema);
    let text = synonyms.compile(
        "w3 nosuch",
        &DefaultFields::all(schema),
        &QueryLimits::default(),
    );
    group.bench_function("l1", |b| {
        b.iter(|| {
            let out = views
                .search_relaxed(&text, &request, &relax, &exec)
                .unwrap();
            assert_eq!(out.relaxation.level, RelaxLevel::Synonyms);
            black_box(out)
        });
    });
    // L3 (ROADMAP 8.2.3) alone: `w` is no term, but every term starts with it. L0 (`w3` and `w`) has no hit, so the
    // step requires `w3` exactly (2 characters, not last) and one of the 50 most frequent terms starting `w`.
    // L4 (ROADMAP 8.2.4) alone: `w1x4` is no term, but one edit from `w104`..`w194` and `w14`. L0 has no hit, so the
    // step requires `w3` exactly (2 characters) and one of the terms within one edit of `w1x4`.
    for (name, level, text) in [
        ("l3", Level::Prefix, "w3 w"),
        ("l4", Level::Fuzzy, "w3 w1x4"),
    ] {
        let alone = RelaxPlan::new(
            schema.relaxation(),
            &RelaxOptions {
                levels: Some(Levels::Custom(vec![level])),
                ..RelaxOptions::default()
            },
        )
        .unwrap();
        let query = compiler.compile(text, &DefaultFields::all(schema), &QueryLimits::default());
        group.bench_function(name, |b| {
            b.iter(|| {
                let out = views
                    .search_relaxed(&query, &request, &alone, &exec)
                    .unwrap();
                assert_eq!(out.relaxation.level, RelaxLevel::from(level));
                black_box(out)
            });
        });
    }
    group.finish();
}

/// Compiling user text (SPEC §7.1.1) without synonyms, and with the groups of [`synonym_compiler`], which the compiler
/// scans for (SPEC §6.3.2): a match compiles its alternatives. The maps are built once, by the first compilation.
fn compile(c: &mut Criterion) {
    let schema = &bench_schema(false, Vec::new());
    let compiler = QueryCompiler::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    let synonyms = synonym_compiler(schema);
    let mut group = c.benchmark_group("compile");
    let fields = DefaultFields::all(schema);
    let text = "w3 w40 w7 w120 nosuch";
    for (name, compiler) in [("plain", &compiler), ("synonyms", &synonyms)] {
        group.bench_function(name, |b| {
            b.iter(|| black_box(compiler.compile(text, &fields, &QueryLimits::default())));
        });
    }
    group.finish();
}

/// A compiler for `schema` with 2 002 synonym groups: `nosuch` (in no document) with `w40` and `w120`, `w3` with 50
/// words in none, and 2 000 pairs of words in none.
fn synonym_compiler(schema: &CollectionSchema) -> QueryCompiler {
    let mut groups = vec![
        vec!["nosuch".to_owned(), "w40".to_owned(), "w120".to_owned()],
        std::iter::once("w3".to_owned())
            .chain((0..50).map(|i| format!("a{i}")))
            .collect(),
    ];
    groups.extend((0..2000).map(|i| vec![format!("s{i}"), format!("t{i}")]));
    let with_synonyms = bench_schema(false, groups);
    assert_eq!(with_synonyms.hash(), schema.hash());
    QueryCompiler::new(&with_synonyms, &AnalyzerRegistry::builtin()).unwrap()
}

/// `text` compiled as user text over the default fields (`body`), as a request's `q` is.
fn compile_text(schema: &CollectionSchema, text: &str) -> Query {
    let compiler = QueryCompiler::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    compiler
        .compile(text, &DefaultFields::all(schema), &QueryLimits::default())
        .query
}
criterion_group!(
    benches,
    bench,
    requests,
    parallel,
    suite,
    collection_expansions,
    relaxed,
    compile
);
criterion_main!(benches);
