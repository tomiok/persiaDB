//! Blob pack append and read (ROADMAP 9.2.2, 9.2.3, 9.2.11): `bind` syncs twice per blob (SPEC §18.9), and reads serve
//! `get_blob` streams and ranges, and `read_chunk_checked` one chunk. Each read runs with `pread` and with mapped reads
//! (SPEC §4.7, ADR-0049 item 12). Run: `cargo bench -p persia-blob --bench pack`.

// `unsafe_code`: the mapped cases make a `MapPermit` (`MapPermit::new` is `unsafe`) over a pack the bench owns.
#![allow(
    missing_docs,
    unsafe_code,
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
    read_chunk_checked, read_range,
};
use persia_format::BlobRef;
use persia_format::checksum::crc32c;
use persia_storage::container::{Container, MapPermit};

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
    let path = dir.path().join("r.persia");
    let (container, store) = writer(&path);
    let len = 16 << 20;
    let m = put(&store, &data(len, 9), 4 << 20, 1);
    let r = reference(&m);
    // A second handle over the same pack, read-only, with mapped reads on.
    let mapped = PackStore::open(pack_path(&path), container.header().db_uuid).unwrap();
    // SAFETY: nothing writes the pack while it is mapped: the writer `store` writes nothing after the `put` above, and
    // no other handle opens the pack (MapPermit::new's contract).
    mapped.enable_mapped_reads(unsafe { MapPermit::new() });
    assert!(mapped.mapped_range().is_some());
    let mut group = c.benchmark_group("pack_read");
    // The `pread` cases keep the names they had before mapped reads were benched, so earlier baselines compare.
    for (suffix, store) in [("", &store), ("_mapped", &mapped)] {
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_function(format!("full_16MiB{suffix}"), |b| {
            b.iter(|| read_range(store, black_box(&r), 0..m.size()).unwrap());
        });
        group.throughput(Throughput::Bytes(1 << 20));
        group.bench_function(format!("range_1MiB_across_two_chunks{suffix}"), |b| {
            let start = (4 << 20) - (512 << 10);
            b.iter(|| read_range(store, black_box(&r), start..start + (1 << 20)).unwrap());
        });
        // ROADMAP 9.2.11: one chunk, owned, checked in the frame's one CRC pass.
        group.throughput(Throughput::Bytes(4 << 20));
        group.bench_function(format!("chunk_checked_4MiB{suffix}"), |b| {
            b.iter(|| read_chunk_checked(store, black_box(&m), 1).unwrap());
        });
    }
    group.finish();
}

criterion_group!(benches, append, read);
criterion_main!(benches);
