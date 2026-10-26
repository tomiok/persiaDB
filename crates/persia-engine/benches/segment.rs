//! Indexing throughput: [`SegmentBuilder::add`] over synthetic documents, and [`SegmentWriter::finish`] (ROADMAP 5.1,
//! 5.2). Deterministic input (a fixed-seed generator), so runs compare; baselines in `docs/benchmarks.md`.
//! Run: `cargo bench -p persia-engine --bench segment`.

#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::segment::{SegmentBuilder, SegmentMeta, SegmentWriter};

const DOCS: u64 = 2_000;
const VOCABULARY: u64 = 5_000;

fn schema() -> CollectionSchema {
    SchemaBuilder::new("bench")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .field("price", FieldKind::F64 { stored: true })
        .build()
        .unwrap()
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

    /// A word with a skewed (roughly Zipfian) rank.
    fn word(&mut self) -> String {
        let a = self.next() % VOCABULARY + 1;
        let b = self.next() % a + 1;
        format!("w{b}")
    }
}

fn documents(schema: &CollectionSchema) -> Vec<Document> {
    let field = |name: &str| {
        schema
            .field(name)
            .map(persia_engine::schema::Field::id)
            .unwrap()
    };
    let (title, body, brand, price) = (
        field("title"),
        field("body"),
        field("brand"),
        field("price"),
    );
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    (0..DOCS)
        .map(|i| {
            let title_text: Vec<String> = (0..8).map(|_| rng.word()).collect();
            let body_text: Vec<String> = (0..120).map(|_| rng.word()).collect();
            #[allow(clippy::cast_precision_loss)]
            let p = (rng.next() % 100_000) as f64 / 100.0;
            Document::new(format!("doc-{i}"))
                .with(title, Value::String(title_text.join(" ")))
                .with(body, Value::String(body_text.join(" ")))
                .with(brand, Value::String(format!("brand-{}", rng.next() % 50)))
                .with(price, Value::F64(p))
        })
        .collect()
}

fn build(
    schema: &CollectionSchema,
    registry: &AnalyzerRegistry,
    docs: &[Document],
) -> SegmentBuilder {
    let mut b = SegmentBuilder::new(schema, registry).unwrap();
    for d in docs {
        b.add(d).unwrap();
    }
    b
}

fn bench(c: &mut Criterion) {
    let schema = schema();
    let registry = AnalyzerRegistry::builtin();
    let docs = documents(&schema);
    let meta = SegmentMeta {
        segment_id: 1,
        created_at: 0,
        merge_generation: 0,
    };

    let mut group = c.benchmark_group("segment");
    group.throughput(Throughput::Elements(DOCS));
    group.bench_function("add", |b| {
        b.iter(|| black_box(build(&schema, &registry, &docs)));
    });
    let built = build(&schema, &registry, &docs);
    group.bench_function("finish", |b| {
        b.iter(|| black_box(SegmentWriter::new(&built, meta).finish().unwrap()));
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
