//! Remote blob transfers (ROADMAP 9.3.2): staging and reading 32 chunks over a backend with a fixed 2 ms latency per
//! request, one request at a time and with 8 in flight. Run: `cargo bench -p persia-blob --bench remote`.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_possible_truncation)]

use core::num::NonZeroUsize;
use core::ops::Range;
use std::hint::black_box;
use std::time::Duration;

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use persia_blob::{BlobManifest, BlobStore, ChunkSize, RemoteBlobStore, UploadId, blob_id_of};
use persia_format::ByteSource;
use persia_format::checksum::crc32c;
use persia_storage::{
    Capabilities, Key, KeyPrefix, MemoryStorage, ObjectMeta, PutOutcome, Result, Storage,
};

const LATENCY: Duration = Duration::from_millis(2);
const CHUNK: usize = 64 << 10;
const CHUNKS: usize = 32;

/// A memory backend that sleeps before every request, like a network round trip.
#[derive(Debug, Default)]
struct Slow(MemoryStorage);

impl Storage for Slow {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        std::thread::sleep(LATENCY);
        self.0.get_range(key, range)
    }
    fn get(&self, key: &Key) -> Result<Bytes> {
        std::thread::sleep(LATENCY);
        self.0.get(key)
    }
    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        std::thread::sleep(LATENCY);
        self.0.put(key, data)
    }
    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        std::thread::sleep(LATENCY);
        self.0.put_if_absent(key, data)
    }
    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        std::thread::sleep(LATENCY);
        self.0.list(prefix)
    }
    fn delete(&self, key: &Key) -> Result<()> {
        std::thread::sleep(LATENCY);
        self.0.delete(key)
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
}

fn transfers(c: &mut Criterion) {
    let data: Vec<u8> = (0..CHUNK * CHUNKS).map(|i| (i % 251) as u8).collect();
    let chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
    let m = BlobManifest::new(
        blob_id_of(&data),
        data.len() as u64,
        ChunkSize::new(CHUNK as u32).unwrap(),
        UploadId::new(1),
        chunks.iter().map(|c| crc32c(c)).collect(),
    )
    .unwrap();
    let mut group = c.benchmark_group("remote_2ms");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.sample_size(10);
    for p in [1, 8] {
        let store =
            RemoteBlobStore::new(Slow::default()).with_parallelism(NonZeroUsize::new(p).unwrap());
        store.stage_chunks(UploadId::new(1), 0, &chunks).unwrap();
        // Staging the same bytes again answers `Created`: every iteration does the full round trips.
        group.bench_with_input(BenchmarkId::new("stage_chunks", p), &p, |b, _| {
            b.iter(|| {
                store
                    .stage_chunks(UploadId::new(1), 0, black_box(&chunks))
                    .unwrap();
            });
        });
        group.bench_with_input(BenchmarkId::new("read_chunks", p), &p, |b, _| {
            b.iter(|| store.read_chunks(&m, 0..CHUNKS as u32).unwrap());
        });
    }
    group.finish();
}

criterion_group!(benches, transfers);
criterion_main!(benches);
