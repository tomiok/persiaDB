//! Scalar bit-packing throughput (ROADMAP 1.2.2.5): the baseline a SIMD decoder must beat (ADR-0004).
//! Baseline numbers are recorded in `docs/benchmarks.md`.
//! Run: `cargo bench -p persia-format --bench bitpack`.

#![allow(missing_docs, clippy::unwrap_used)]

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use persia_format::bitpack::{BLOCK_LEN, BitWidth};
use persia_format::{Reader, Writer};
use std::hint::black_box;

/// Deterministic values that fill `width` bits (a fixed LCG, no RNG dependency).
fn block(width: u8) -> [u32; BLOCK_LEN] {
    let mask = u32::try_from((1_u64 << width) - 1).unwrap();
    let mut x = 0x9e37_79b9_u32;
    core::array::from_fn(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        x & mask
    })
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("bitpack");
    group.throughput(Throughput::Elements(BLOCK_LEN as u64));
    for w in [1_u8, 4, 8, 13, 20, 32] {
        let width = BitWidth::new(w).unwrap();
        let values = block(w);
        let mut writer = Writer::new();
        writer.write_packed_block(&values, width).unwrap();
        let bytes = writer.into_inner();

        group.bench_with_input(BenchmarkId::new("unpack", w), &bytes, |b, bytes| {
            let mut out = [0_u32; BLOCK_LEN];
            b.iter(|| {
                Reader::new(black_box(bytes))
                    .read_packed_block(width, &mut out)
                    .unwrap();
                black_box(&out);
            });
        });
        group.bench_with_input(BenchmarkId::new("pack", w), &values, |b, values| {
            // A fresh writer per iteration, created outside the timed part.
            b.iter_batched_ref(
                Writer::new,
                |writer| writer.write_packed_block(black_box(values), width).unwrap(),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
