//! Query execution (ROADMAP 7.4): top-10 over one segment of 50 000 synthetic documents, for a term, an AND, an OR
//! and a phrase with the default pruning (block-max WAND), each strategy of 7.4.4 side by side, and a term under
//! filters (7.4.5): a numeric range, a keyword, and a filter alone. Deterministic
//! input (a fixed-seed generator), so runs compare. Run: `cargo bench -p persia-engine --bench search`.

#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use std::ops::Bound;

use persia_engine::query::{
    BoolQuery, Filter, FilterValue, PhraseQuery, PhraseTerm, Query, TermQuery,
};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
use persia_engine::stats::SegmentStatistics;
use persia_engine::view::SegmentView;
use persia_format::ByteSource;

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

    filters(
        c,
        &|q| SearchPlan::new(q, &schema, &stats).unwrap(),
        &view,
        fields,
    );
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

criterion_group!(benches, bench);
criterion_main!(benches);
