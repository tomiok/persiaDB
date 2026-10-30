//! Query execution (ROADMAP 7.4): exhaustive top-10 over one segment of 50 000 synthetic documents, for a term, an
//! AND, an OR and a phrase, before `MaxScore` and block-max WAND prune (7.4.4). Deterministic input (a fixed-seed
//! generator), so runs compare. Run: `cargo bench -p persia-engine --bench search`.

#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::query::{BoolQuery, PhraseQuery, PhraseTerm, Query, TermQuery};
use persia_engine::schema::{FieldId, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::SearchPlan;
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

fn bench(c: &mut Criterion) {
    let schema = SchemaBuilder::new("bench")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .build()
        .unwrap();
    let body: FieldId = schema.field("body").unwrap().id();
    let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for i in 0..DOCS {
        let len = 5 + rng.next() % 30;
        let text: Vec<String> = (0..len).map(|_| format!("w{}", rng.word())).collect();
        builder
            .add(&Document::new(format!("d{i}")).with(body, Value::String(text.join(" "))))
            .unwrap();
    }
    let meta = SegmentMeta {
        segment_id: 1,
        created_at: 0,
        merge_generation: 0,
    };
    let bytes = SegmentWriter::new(&builder, meta).finish().unwrap();
    let segment = Arc::new(Segment::open(ByteSource::new(bytes.into()), &schema).unwrap());
    let schema = Arc::new(schema);
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
}

criterion_group!(benches, bench);
criterion_main!(benches);
