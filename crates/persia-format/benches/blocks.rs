//! Block decode throughput on the query hot path (ROADMAP 1.2.3, 1.2.4): a full postings block (unpack +
//! delta + header checks), FOR column blocks by random access and by iteration, and one 16 KiB block of a
//! block-framed section (ROADMAP 1.5.4).
//! Run: `cargo bench -p persia-format --bench blocks`.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing
)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use persia_format::block::{self, BlockReader, BlockWriter, Compression};
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

/// Text-like data from the LCG over a small vocabulary: compresses roughly like documents do.
fn text(len: usize) -> Vec<u8> {
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
    let mut next = lcg(42);
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        out.extend_from_slice(WORDS[(next() % 8) as usize]);
    }
    out.truncate(len);
    out
}

/// One block of a block-framed section (ROADMAP 1.5.4, SPEC §5.2): CRC over header + payload, header checks
/// against the table, then the codec. A 16 KiB block is the SPEC default and a doc-store/column read unit.
fn framed_blocks(c: &mut Criterion) {
    let size = block::DEFAULT_BLOCK_SIZE;
    let data = text(4 * size as usize);
    let mut group = c.benchmark_group("block_framed");
    group.throughput(Throughput::Bytes(u64::from(size)));
    for (name, compression) in [
        ("none", Compression::None),
        ("zstd", Compression::default()),
    ] {
        let mut w = BlockWriter::new(size, compression).unwrap();
        w.write(&data).unwrap();
        let section = w.finish().unwrap();
        let reader = BlockReader::open(&section, 0).unwrap();
        let mut out = Vec::with_capacity(size as usize);
        group.bench_function(BenchmarkId::new("read_block", name), |b| {
            b.iter(|| {
                out.clear();
                reader.read_block(black_box(1), &mut out).unwrap();
                black_box(&out);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, postings, for_blocks, framed_blocks);
criterion_main!(benches);
