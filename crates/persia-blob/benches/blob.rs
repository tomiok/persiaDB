//! Blob ingest throughput (ROADMAP 9.1.1, 9.1.2): every uploaded byte is hashed with SHA-256 (ADR-0002), so the
//! hasher bounds `put_blob`. Run: `cargo bench -p persia-blob --bench blob`.

#![allow(missing_docs, clippy::cast_possible_truncation)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use persia_blob::{ContentHasher, blob_id_of};
use std::hint::black_box;

/// Incompressible-looking bytes from a fixed LCG (blobs are usually already compressed, SPEC §9.2).
fn data(len: usize) -> Vec<u8> {
    let mut x = 42_u64;
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (x >> 56) as u8
        })
        .collect()
}

fn hashing(c: &mut Criterion) {
    let src = data(4 << 20);
    let mut group = c.benchmark_group("blob_hash");
    group.throughput(Throughput::Bytes(src.len() as u64));
    group.bench_function("sha256_one_shot_4MiB", |b| {
        b.iter(|| blob_id_of(black_box(&src)));
    });
    // The chunker feeds the hasher in reader-sized pieces.
    for piece in [8 << 10, 64 << 10] {
        group.bench_with_input(
            BenchmarkId::new("sha256_pieces", piece),
            &piece,
            |b, &piece| {
                b.iter(|| {
                    let mut h = ContentHasher::new();
                    for p in black_box(&src).chunks(piece) {
                        h.update(p);
                    }
                    h.finish()
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, hashing);
criterion_main!(benches);
