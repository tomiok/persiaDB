//! Blob pack append and read (ROADMAP 9.2.2, 9.2.3): `bind` syncs twice per blob (SPEC §18.9), and reads serve
//! `get_blob` streams and ranges. Run: `cargo bench -p persia-blob --bench pack`.
//!
//! Reads here use `pread`: mapped reads need an `unsafe` `MapPermit::new`, which only files listed in xtask's
//! `UNSAFE_ALLOWED` may call.

#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::unwrap_used,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::Path;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use persia_blob::{
    BindOutcome, BlobManifest, BlobStore, ChunkSize, PackStore, UploadId, blob_id_of, pack_path,
    read_range,
};
use persia_format::BlobRef;
use persia_format::checksum::crc32c;
use persia_storage::container::Container;

/// Incompressible-looking bytes from a fixed LCG.
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (x >> 56) as u8
        })
        .collect()
}

fn put(store: &PackStore, data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
    let crcs = data.chunks(chunk as usize).map(crc32c).collect();
    let m = BlobManifest::new(
        blob_id_of(data),
        data.len() as u64,
        ChunkSize::new(chunk).unwrap(),
        UploadId::new(upload),
        crcs,
    )
    .unwrap();
    for (i, c) in data.chunks(chunk as usize).enumerate() {
        store.stage_chunk(m.upload_id(), i as u32, c).unwrap();
    }
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    m
}

/// A container at `path` and its pack opened read-write with its writer proof (ROADMAP 9.2.7). The container is
/// returned first, so a caller holding both drops the pack first.
fn writer(path: &Path) -> (Container, PackStore) {
    let container = Container::create(path).unwrap();
    let store =
        PackStore::open_writer(pack_path(path), &container.writer_proof().unwrap()).unwrap();
    (container, store)
}

fn reference(m: &BlobManifest) -> BlobRef {
    BlobRef::new(
        m.blob_id(),
        m.size(),
        "a/b",
        m.chunk_size().get(),
        BTreeMap::new(),
    )
    .unwrap()
}

fn append(c: &mut Criterion) {
    let mut group = c.benchmark_group("pack_append");
    group.sample_size(10);
    for (len, chunk) in [(64 << 10, 64 << 10), (4 << 20, 4 << 20)] {
        let src = data(len, 7);
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::new("stage_and_bind", len), &src, |b, src| {
            let dir = tempfile::tempdir().unwrap();
            let (_container, store) = writer(&dir.path().join("b.persia"));
            let mut n = 0_u64;
            b.iter(|| {
                // Distinct bytes per blob, or bind would dedup.
                n += 1;
                let mut blob = src.clone();
                blob[..8].copy_from_slice(&n.to_le_bytes());
                put(&store, black_box(&blob), chunk, u128::from(n));
            });
        });
    }
    group.finish();
}

fn read(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let (_container, store) = writer(&dir.path().join("r.persia"));
    let len = 16 << 20;
    let m = put(&store, &data(len, 9), 4 << 20, 1);
    let r = reference(&m);
    let mut group = c.benchmark_group("pack_read");
    group.throughput(Throughput::Bytes(len as u64));
    group.bench_function("full_16MiB", |b| {
        b.iter(|| read_range(&store, black_box(&r), 0..m.size()).unwrap());
    });
    group.throughput(Throughput::Bytes(1 << 20));
    group.bench_function("range_1MiB_across_two_chunks", |b| {
        let start = (4 << 20) - (512 << 10);
        b.iter(|| read_range(&store, black_box(&r), start..start + (1 << 20)).unwrap());
    });
    group.finish();
}

criterion_group!(benches, append, read);
criterion_main!(benches);
