//! The write path of a database (ROADMAP 6.3): `put` of 2 000 documents with `Buffered` durability through the stage,
//! log and publish steps, the `commit` that flushes them into one segment and publishes it, `commit_upserts`, the same
//! commit when every document replaces one in older segments, and `commit_inserts_*`, when every document is new but
//! 4 or 64 older segments are searched for it (ROADMAP 6.4.3). The container is a simulated file, so these measure
//! the engine's work, not the disk's sync latency. Deterministic input, so runs
//! compare. Run: `cargo bench -p persia-engine --bench database`.

#![allow(missing_docs, clippy::unwrap_used, clippy::indexing_slicing)] // Fixed input.

use std::sync::Arc;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use persia_engine::db::{Database, DatabaseOptions, ManualClock, SegmentIds};
use persia_engine::document::{Document, Value};
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

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

/// `DOCS` documents of about 40 words each.
fn documents(schema: &CollectionSchema) -> Vec<Document> {
    documents_named(schema, "doc")
}

/// [`documents`] with ids `<prefix>-<i>`.
fn documents_named(schema: &CollectionSchema, prefix: &str) -> Vec<Document> {
    let body = schema.field("body").unwrap().id();
    let tag = schema.field("tag").unwrap().id();
    (0..DOCS)
        .map(|i| {
            let text: Vec<&str> = (0..40)
                .map(|j| WORDS[(i * 7 + j * 3) % WORDS.len()])
                .collect();
            Document::new(format!("{prefix}-{i}"))
                .with(body, Value::String(text.join(" ")))
                .with(tag, Value::String(WORDS[i % WORDS.len()].into()))
        })
        .collect()
}

fn database() -> Database<SimFile> {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(0)),
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [1; 16], options).unwrap();
    db.create_collection(schema()).unwrap();
    db
}

fn bench(c: &mut Criterion) {
    let docs = documents(&schema());
    let mut group = c.benchmark_group("database");
    group.throughput(Throughput::Elements(DOCS as u64));
    group.bench_function("put_buffered", |b| {
        b.iter_batched(
            database,
            |mut db| {
                for doc in &docs {
                    db.put("bench", doc, Durability::Buffered).unwrap();
                }
                db
            },
            BatchSize::LargeInput,
        );
    });
    group.bench_function("commit", |b| {
        b.iter_batched(
            || {
                let mut db = database();
                db.put_batch("bench", &docs, Durability::Buffered).unwrap();
                db
            },
            |mut db| {
                db.commit().unwrap();
                db
            },
            BatchSize::LargeInput,
        );
    });
    // Every document replaces a committed one: the commit looks each id up in 4 segments and writes a sidecar
    // (ROADMAP 6.4.3).
    group.bench_function("commit_upserts", |b| {
        b.iter_batched(
            || {
                let mut db = database();
                for _ in 0..4 {
                    db.put_batch("bench", &docs, Durability::Buffered).unwrap();
                    db.commit().unwrap();
                }
                db.put_batch("bench", &docs, Durability::Buffered).unwrap();
                db
            },
            |mut db| {
                db.commit().unwrap();
                db
            },
            BatchSize::LargeInput,
        );
    });
    // Every document is new, but the commit still looks each id up in every committed segment (ROADMAP 6.4.3).
    for segments in [4, 64] {
        let older: Vec<Vec<Document>> = (0..segments)
            .map(|s| documents_named(&schema(), &format!("old{s}")))
            .collect();
        group.bench_function(format!("commit_inserts_{segments}_segments"), |b| {
            b.iter_batched(
                || {
                    let mut db = database();
                    for docs in &older {
                        db.put_batch("bench", docs, Durability::Buffered).unwrap();
                        db.commit().unwrap();
                    }
                    db.put_batch("bench", &docs, Durability::Buffered).unwrap();
                    db
                },
                |mut db| {
                    db.commit().unwrap();
                    db
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
