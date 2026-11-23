//! Following a writer from a read-only database (ROADMAP 6.6.6, ADR-0060), with 4 committed segments and a WAL
//! backlog of 2 000 unflushed documents (20 synced chunks) behind them. A refresh re-reads every chunk past the
//! `log_end` of the superblock slot it read (by content hash, ADR-0060), and scans the WAL from `wal_start`, so each
//! case fixes both: `refresh_unchanged_backlog` is a refresh with nothing new and the whole backlog past `log_end`;
//! `refresh_unchanged` the same once a commit that flushes nothing (a created collection) moved `log_end` past it;
//! `reopen` the new open the reader needed before; `refresh_one_put` a refresh (incremental) after a synced put by
//! the writer, which is not timed, from that last state. Every [`RESET_EVERY`] iterations the writer and the reader
//! start again from that state, outside the timing, so at most that many one-document chunks lie past `log_end` and
//! the cost per iteration does not grow with the iteration count. The container is a simulated file, so these
//! measure the engine's work, not the disk's. Deterministic input, so runs compare. Run:
//! `cargo bench -p persia-engine --bench read_only`.

#![allow(missing_docs, clippy::unwrap_used, clippy::indexing_slicing)] // Fixed input.

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, ManualClock, ReadOnlyDatabase, Refreshed, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

const DOCS: usize = 2_000;
/// `refresh_one_put` starts again from the same state every this many puts, so its cost per iteration does not grow.
const RESET_EVERY: usize = 64;
const WORDS: [&str; 8] = [
    "alpha", "beta", "gamma", "delta", "omega", "search", "engine", "log",
];

fn schema(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

/// A document of about 40 words.
fn document(schema: &CollectionSchema, id: &str, i: usize) -> Document {
    let body = schema.field("body").unwrap().id();
    let text: Vec<&str> = (0..40)
        .map(|j| WORDS[(i * 7 + j * 3) % WORDS.len()])
        .collect();
    Document::new(id).with(body, Value::String(text.join(" ")))
}

fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(0)),
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    }
}

/// 4 segments of `DOCS` documents, then `DOCS` documents in the WAL, synced 100 at a time, all past `log_end`.
fn database() -> (SimFile, Database<SimFile>) {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [1; 16], options()).unwrap();
    let schema = schema("bench");
    db.create_collection(schema.clone()).unwrap();
    for segment in 0..4 {
        let docs: Vec<_> = (0..DOCS)
            .map(|i| document(&schema, &format!("s{segment}-{i}"), i))
            .collect();
        db.put_batch("bench", &docs, Durability::Buffered).unwrap();
        db.commit().unwrap();
    }
    for batch in 0..DOCS / 100 {
        let docs: Vec<_> = (0..100)
            .map(|i| document(&schema, &format!("w{batch}-{i}"), i))
            .collect();
        db.put_batch("bench", &docs, Durability::Fsync).unwrap();
    }
    (file, db)
}

/// A commit that flushes nothing, so `log_end` moves past every chunk and `wal_start` stays: it creates an empty
/// collection. The reader takes it incrementally.
fn commit_without_flush(db: &mut Database<SimFile>, reader: &ReadOnlyDatabase<SimFile>) {
    db.create_collection(schema("mark")).unwrap();
    let refreshed = reader.refresh().unwrap();
    assert!(matches!(
        refreshed,
        Refreshed::Incremental { records: 0, .. }
    ));
}

fn bench(c: &mut Criterion) {
    let registry = AnalyzerRegistry::builtin();
    let schema = schema("bench");
    let mut group = c.benchmark_group("read_only");
    group.sample_size(20);

    let (file, mut db) = database();
    let reader = ReadOnlyDatabase::open_on(file.clone(), &registry).unwrap();
    group.bench_function("refresh_unchanged_backlog", |b| {
        b.iter(|| assert_eq!(reader.refresh().unwrap(), Refreshed::Unchanged));
    });
    commit_without_flush(&mut db, &reader);
    group.bench_function("refresh_unchanged", |b| {
        b.iter(|| assert_eq!(reader.refresh().unwrap(), Refreshed::Unchanged));
    });
    group.bench_function("reopen", |b| {
        b.iter(|| ReadOnlyDatabase::open_on(file.clone(), &registry).unwrap());
    });
    let image = file.bytes();
    drop((reader, db));
    let restart = || {
        let file = SimFile::from_bytes(image.clone());
        let db = Database::open_on(file.clone(), options()).unwrap();
        let reader = ReadOnlyDatabase::open_on(file, &registry).unwrap();
        (db, reader)
    };
    let (mut db, mut reader) = restart();
    let mut n = 0usize;
    group.bench_function("refresh_one_put", |b| {
        b.iter_custom(|iters| {
            let mut timed = Duration::ZERO;
            for _ in 0..iters {
                if n.is_multiple_of(RESET_EVERY) {
                    (db, reader) = restart();
                }
                n += 1;
                let doc = document(&schema, &format!("new-{n}"), n);
                db.put("bench", &doc, Durability::Fsync).unwrap();
                let start = Instant::now();
                let refreshed = reader.refresh().unwrap();
                timed += start.elapsed();
                assert!(matches!(
                    refreshed,
                    Refreshed::Incremental { records: 1, .. }
                ));
            }
            timed
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
