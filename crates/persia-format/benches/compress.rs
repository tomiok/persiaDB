//! Block codec throughput (ROADMAP 1.4.1): zstd decode of a 16 KiB block (SPEC §5.2 default) is on the query hot
//! path for doc-store and column reads; encode is on the indexing path. Run: `cargo bench -p persia-format --bench compress`.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing
)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use persia_format::compress::{Codec, DEFAULT_ZSTD_LEVEL, compress_block, decompress_block};
use std::hint::black_box;

const BLOCK: usize = 16 * 1024;

/// Text-like data from a fixed LCG over a small vocabulary: compresses roughly like documents do.
fn block() -> Vec<u8> {
    const WORDS: [&[u8]; 8] = [
        b"persia ",
        b"search ",
        b"segment ",
        b"query ",
        b"the ",
        b"of ",
        b"block ",
        b"zstd ",
    ];
    let mut x = 42_u64;
    let mut out = Vec::with_capacity(BLOCK);
    while out.len() < BLOCK {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.extend_from_slice(WORDS[(x >> 61) as usize]);
    }
    out.truncate(BLOCK);
    out
}

fn codecs(c: &mut Criterion) {
    let src = block();
    let mut group = c.benchmark_group("block_codec");
    group.throughput(Throughput::Bytes(BLOCK as u64));
    for level in [1, DEFAULT_ZSTD_LEVEL, 9] {
        let mut out = Vec::with_capacity(2 * BLOCK);
        group.bench_with_input(
            BenchmarkId::new("zstd_compress", level),
            &level,
            |b, &level| {
                b.iter(|| {
                    out.clear();
                    compress_block(Codec::Zstd, level, black_box(&src), &mut out).unwrap()
                });
            },
        );
        let mut packed = Vec::new();
        compress_block(Codec::Zstd, level, &src, &mut packed).unwrap();
        group.bench_with_input(
            BenchmarkId::new("zstd_decompress", level),
            &packed,
            |b, packed| {
                b.iter(|| {
                    out.clear();
                    decompress_block(
                        Codec::Zstd,
                        black_box(packed),
                        BLOCK as u64,
                        BLOCK,
                        0,
                        &mut out,
                    )
                    .unwrap();
                });
            },
        );
    }
    let mut out = Vec::with_capacity(BLOCK);
    group.bench_function("none_decompress", |b| {
        b.iter(|| {
            out.clear();
            decompress_block(
                Codec::None,
                black_box(&src),
                BLOCK as u64,
                BLOCK,
                0,
                &mut out,
            )
            .unwrap();
        });
    });
    group.finish();
}

criterion_group!(benches, codecs);
criterion_main!(benches);
