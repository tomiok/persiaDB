//! Block decode throughput on the query hot path (ROADMAP 1.2.3, 1.2.4): a full postings block (unpack +
//! delta + header checks) and FOR column blocks by random access and by iteration.
//! Run: `cargo bench -p persia-format --bench blocks`.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_possible_truncation)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use persia_format::{Reader, Writer, bitpack, for_codec};
use std::hint::black_box;

/// A fixed LCG: deterministic data without an RNG dependency.
fn lcg(seed: u64) -> impl FnMut() -> u64 {
    let mut x = seed;
    move || {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        x >> 16
    }
}

fn postings(c: &mut Criterion) {
    let mut group = c.benchmark_group("postings_block");
    group.throughput(Throughput::Elements(bitpack::BLOCK_LEN as u64));
    // Average docid step 2 (dense), 64 and 4096 (sparse): gap widths of about 2, 7 and 13 bits.
    for step in [2_u64, 64, 4096] {
        let mut next = lcg(step);
        let mut docid = 0_u32;
        let docids: [u32; bitpack::BLOCK_LEN] = core::array::from_fn(|_| {
            docid += 1 + (next() % (2 * step - 1)) as u32;
            docid
        });
        let mut w = Writer::new();
        w.write_postings_block(&docids, Some(0)).unwrap();
        let bytes = w.into_inner();
        group.bench_with_input(BenchmarkId::new("decode", step), &bytes, |b, bytes| {
            let mut out = [0_u32; bitpack::BLOCK_LEN];
            b.iter(|| {
                Reader::new(black_box(bytes))
                    .read_postings_block(Some(0), &mut out)
                    .unwrap();
                black_box(&out);
            });
        });
    }
    group.finish();
}

fn for_blocks(c: &mut Criterion) {
    let mut group = c.benchmark_group("for_block");
    group.throughput(Throughput::Elements(for_codec::VALUES_PER_BLOCK as u64));
    for width in [7_u32, 20, 64] {
        let mut next = lcg(u64::from(width));
        let mask = u64::MAX >> (64 - width);
        let keys: Vec<u64> = (0..for_codec::VALUES_PER_BLOCK)
            .map(|_| 1_000 + (next() & mask))
            .collect();
        let mut w = Writer::new();
        w.write_for_block(&keys).unwrap();
        let bytes = w.into_inner();
        let mut pick = lcg(99);
        let indices: Vec<usize> = (0..for_codec::VALUES_PER_BLOCK)
            .map(|_| (pick() % for_codec::VALUES_PER_BLOCK as u64) as usize)
            .collect();

        group.bench_with_input(BenchmarkId::new("iter", width), &bytes, |b, bytes| {
            b.iter(|| {
                let block = Reader::new(black_box(bytes)).read_for_block().unwrap();
                black_box(block.iter().fold(0_u64, u64::wrapping_add))
            });
        });
        group.bench_with_input(BenchmarkId::new("get_random", width), &bytes, |b, bytes| {
            let block = Reader::new(bytes).read_for_block().unwrap();
            b.iter(|| {
                black_box(indices.iter().fold(0_u64, |acc, &i| {
                    acc.wrapping_add(block.get(black_box(i)).unwrap())
                }))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, postings, for_blocks);
criterion_main!(benches);
