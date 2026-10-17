//! Committed-frame reads (ROADMAP 2.3.6; SPEC §4.7): `pread` into a fresh buffer versus a slice of the mapping, for
//! a small (WAL-sized) and a large (segment-sized) frame, page cache warm.
//! Run: `cargo bench -p persia-storage --bench container_read`.

#![allow(missing_docs, clippy::unwrap_used)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::{Commit, Container, Mode};
use std::hint::black_box;

fn read_frame(c: &mut Criterion) {
    let mut group = c.benchmark_group("container_read_frame_source");
    for size in [4 * 1024, 1024 * 1024] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bench.persia");
        let mut w = Container::create(&path).unwrap();
        let frame = w
            .append(FrameKind::Segment, &vec![7; size], Compression::None)
            .unwrap();
        w.sync().unwrap();
        let manifest = w
            .append(FrameKind::Manifest, b"m", Compression::None)
            .unwrap();
        let wal_start = w.end();
        w.commit(Commit {
            manifest,
            wal_start,
            fence_token: 0,
        })
        .unwrap();
        drop(w);

        let plain = Container::open(&path, Mode::ReadOnly).unwrap();
        let mut mapped = Container::open(&path, Mode::ReadOnly).unwrap();
        mapped.enable_mapped_reads().unwrap();
        group.throughput(Throughput::Bytes(size as u64));
        for (name, c) in [("pread", &plain), ("mapped", &mapped)] {
            group.bench_with_input(BenchmarkId::new(name, size), &frame.offset, |b, &offset| {
                b.iter(|| black_box(c.read_frame_source(offset, 2 * size).unwrap()));
            });
        }
    }
    group.finish();
}

criterion_group!(benches, read_frame);
criterion_main!(benches);
