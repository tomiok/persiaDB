//! The WAL (ROADMAP 6.2): encoding records into chunks through [`WalWriter`] with `Buffered` durability (rotation at
//! the default 1 MiB), one `Fsync` batch, and replaying the chunks into a memtable. The log is a simulated file, so
//! these measure the engine's work, not the disk's sync latency. Deterministic input, so runs compare.
//! Run: `cargo bench -p persia-engine --bench wal`.

#![allow(missing_docs, clippy::unwrap_used, clippy::indexing_slicing)] // Fixed input.

use std::collections::BTreeMap;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, FieldSet, Value};
use persia_engine::memtable::Memtable;
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::wal::{Durability, WalRecord, WalWriter, replay_container};
use persia_storage::container::sim::SimFile;
use persia_storage::container::{Container, Mode};

const DOCS: usize = 2_000;
const WORDS: [&str; 8] = [
    "alpha", "beta", "gamma", "delta", "omega", "search", "engine", "log",
];

fn schema() -> CollectionSchema {
    SchemaBuilder::new("bench")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap()
}

/// `DOCS` encoded documents of about 40 words each.
fn documents(schema: &CollectionSchema) -> Vec<Vec<u8>> {
    let body = schema.field("body").unwrap().id();
    let tag = schema.field("tag").unwrap().id();
    (0..DOCS)
        .map(|i| {
            let text: Vec<&str> = (0..40)
                .map(|j| WORDS[(i * 7 + j * 3) % WORDS.len()])
                .collect();
            Document::new(format!("doc-{i}"))
                .with(body, Value::String(text.join(" ")))
                .with(tag, Value::String(WORDS[i % WORDS.len()].into()))
                .encode(schema, FieldSet::All)
                .unwrap()
        })
        .collect()
}

fn write(docs: &[Vec<u8>], durability: Durability, batch: bool) -> SimFile {
    let file = SimFile::new();
    let mut wal = WalWriter::new(Container::create_on(file.clone(), [1; 16]).unwrap());
    let records: Vec<_> = docs
        .iter()
        .map(|d| WalRecord::put("bench", d).unwrap())
        .collect();
    if batch {
        wal.append_batch(&records, durability).unwrap();
    } else {
        for r in &records {
            wal.append(r, durability).unwrap();
        }
    }
    wal.sync().unwrap();
    file
}

fn bench(c: &mut Criterion) {
    let schema = schema();
    let docs = documents(&schema);
    let bytes: u64 = docs.iter().map(|d| d.len() as u64).sum();
    let mut g = c.benchmark_group("wal");
    g.throughput(Throughput::Bytes(bytes));
    g.bench_function("append_buffered", |b| {
        b.iter(|| black_box(write(&docs, Durability::Buffered, false)));
    });
    g.bench_function("append_batch_fsync", |b| {
        b.iter(|| black_box(write(&docs, Durability::Fsync, true)));
    });
    let file = write(&docs, Durability::Buffered, false);
    let registry = AnalyzerRegistry::builtin();
    g.bench_function("replay", |b| {
        b.iter(|| {
            let container = Container::open_on(file.clone(), Mode::ReadOnly).unwrap();
            let mut memtables = BTreeMap::from([(
                "bench".to_owned(),
                Memtable::new(&schema, &registry).unwrap(),
            )]);
            black_box(replay_container(&container, &mut memtables).unwrap())
        });
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
