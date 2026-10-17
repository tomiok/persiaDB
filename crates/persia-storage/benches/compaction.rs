//! Container compaction (ROADMAP 2.4.21; SPEC §4.6): `compact_into` a fresh file, for many small WAL frames (where
//! per-frame system calls dominate) and a few large Segment frames (where bytes do); with the target's sync, and with
//! syncs skipped (`frames_no_sync`) to time the copy alone.
//! Run: `cargo bench -p persia-storage --bench compaction`.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_possible_truncation)]

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::{Commit, Container, ContainerFile};
use std::hint::black_box;

const NONE: Compression = Compression::None;

/// A container whose frames all sit in the committed WAL range, so compaction copies every one.
fn source(dir: &std::path::Path, frames: usize, len: usize) -> (Container, u64) {
    let mut c = Container::create(dir.join("bench.persia")).unwrap();
    let payload: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
    let first = c.append(FrameKind::WalChunk, &payload, NONE).unwrap();
    for _ in 1..frames {
        c.append(FrameKind::WalChunk, &payload, NONE).unwrap();
    }
    c.sync().unwrap();
    let manifest = c.append(FrameKind::Manifest, b"m", NONE).unwrap();
    c.commit(Commit {
        manifest,
        wal_start: first.offset,
        fence_token: 0,
    })
    .unwrap();
    let bytes = c.end();
    (c, bytes)
}

/// A real file whose syncs do nothing, to time the copy alone.
#[derive(Debug)]
struct NoSync(std::fs::File);

impl ContainerFile for NoSync {
    fn file_len(&self) -> std::io::Result<u64> {
        self.0.file_len()
    }
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.0.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        self.0.write_all_at(buf, offset)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn sync_all(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.0.set_len(len)
    }
    fn try_lock(&self) -> std::io::Result<bool> {
        Ok(true)
    }
}

fn compact(c: &mut Criterion) {
    let mut group = c.benchmark_group("container_compact_into");
    group.sample_size(20);
    for (frames, len) in [(20_000, 100), (2, 8 << 20)] {
        let dir = tempfile::tempdir().unwrap();
        let (mut source, bytes) = source(dir.path(), frames, len);
        group.throughput(Throughput::Bytes(bytes));
        group.bench_function(
            BenchmarkId::new("frames", format!("{frames}x{len}B")),
            |b| {
                b.iter_batched(
                    || tempfile::tempfile_in(dir.path()).unwrap(),
                    |target| {
                        black_box(
                            source
                                .compact_into(target, &[], |_| b"m".to_vec(), NONE)
                                .unwrap(),
                        )
                    },
                    BatchSize::PerIteration,
                );
            },
        );
        group.bench_function(
            BenchmarkId::new("frames_no_sync", format!("{frames}x{len}B")),
            |b| {
                b.iter_batched(
                    || NoSync(tempfile::tempfile_in(dir.path()).unwrap()),
                    |target| {
                        black_box(
                            source
                                .compact_into(target, &[], |_| b"m".to_vec(), NONE)
                                .unwrap(),
                        )
                    },
                    BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, compact);
criterion_main!(benches);
