//! BM25F scoring (ROADMAP 7.3): building a two-field `combined` scorer from collection statistics, and scoring
//! 100 000 (frequency, norm) hits with it, the per-candidate work of a search. Deterministic input, so runs compare.
//! Run: `cargo bench -p persia-engine --bench score`.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_possible_truncation)] // Fixed input.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use persia_engine::schema::{FieldId, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::score::{FieldHit, FieldParams, TermScorer};
use persia_engine::segment::ReadError;
use persia_engine::stats::{FieldTotals, StatisticsProvider};

const HITS: u32 = 100_000;

struct Stats;

impl StatisticsProvider for Stats {
    fn doc_count(&self) -> u64 {
        1_000_000
    }

    fn field(&self, _: FieldId) -> FieldTotals {
        FieldTotals {
            docs_with_field: 1_000_000,
            sum_field_len: 30_000_000,
        }
    }

    fn df(&self, _: FieldId, _: &[u8]) -> Result<u64, ReadError> {
        Ok(12_345)
    }
}

fn bench(c: &mut Criterion) {
    let text = |boost| {
        FieldKind::Text(TextOptions {
            boost,
            ..TextOptions::default()
        })
    };
    let schema = SchemaBuilder::new("bench")
        .field("title", text(3.0))
        .field("body", text(1.0))
        .build()
        .unwrap();
    let params: Vec<FieldParams> = ["title", "body"]
        .map(|n| FieldParams::of(&schema, schema.field(n).unwrap().id(), None).unwrap())
        .to_vec();
    let hits: Vec<[Option<FieldHit>; 2]> = (0..HITS)
        .map(|i| {
            let title = (i % 3 != 0).then(|| FieldHit {
                tf: 1 + i % 2,
                norm: (8 + i % 16) as u8,
            });
            let body = Some(FieldHit {
                tf: 1 + i % 7,
                norm: (20 + i % 60) as u8,
            });
            [title, body]
        })
        .collect();

    let mut group = c.benchmark_group("score");
    group.bench_function("build_combined_scorer", |b| {
        b.iter(|| TermScorer::new(black_box(&params), 12_345, &Stats).unwrap());
    });
    let scorer = TermScorer::new(&params, 12_345, &Stats).unwrap();
    group.throughput(Throughput::Elements(u64::from(HITS)));
    group.bench_function("score_100k_hits", |b| {
        b.iter(|| hits.iter().map(|h| scorer.score(black_box(h))).sum::<f64>());
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
