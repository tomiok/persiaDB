//! Flushes during replay at the docid limit (ROADMAP 6.6.5): a backlog with more operations than a memtable has
//! docids (`MAX_DOCS`, capped here) opens, flushing during replay, to exactly the documents written.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_storage::container::sim::{CrashLoss, SimFile};

use crate::db::{Database, DatabaseOptions, ManualClock, SegmentIds};
use crate::document::{Document, Value};
use crate::schema::{FieldKind, SchemaBuilder, TextOptions};
use crate::wal::Durability;

fn options(max_docs: Option<u32>) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(5),
        memtable_max_docs: max_docs,
        ..DatabaseOptions::default()
    }
}

#[test]
fn a_backlog_past_the_docid_limit_is_flushed_during_replay() {
    let schema = SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [9; 16], options(None)).unwrap();
    db.create_collection(schema).unwrap();
    let mut model = BTreeMap::new();
    // 23 operations on 4 ids: upserts take a docid each, deletes none.
    for i in 0..23 {
        let id = format!("n{}", i % 4);
        if i % 5 == 4 {
            db.delete("notes", &id, Durability::Fsync).unwrap();
            model.remove(&id);
        } else {
            let text = format!("version {i}");
            let doc = Document::new(id.as_str()).with(body, Value::String(text.clone()));
            db.put("notes", &doc, Durability::Fsync).unwrap();
            model.insert(id, text);
        }
    }
    let base = db.seq();
    drop(db);
    let file = file.crash(CrashLoss::All);

    let db = Database::open_on(file.clone(), options(Some(5))).unwrap();
    assert!(db.seq() >= base + 3, "{} commits", db.seq() - base);
    let shown = |db: &Database<SimFile>| {
        let snapshot = db.snapshot();
        let notes = snapshot.collection("notes").unwrap();
        (0..4)
            .filter_map(|i| {
                let id = format!("n{i}");
                let d = notes.get(&id).unwrap()?;
                let Some((_, Value::String(text))) = d.fields().next() else {
                    panic!("{id}: no body");
                };
                Some((id, text.clone()))
            })
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(shown(&db), model);
    let seq = db.seq();
    drop(db);
    // The next open replays from the last flush, which fits the memtable: no commit.
    let db = Database::open_on(file.crash(CrashLoss::All), options(Some(5))).unwrap();
    assert_eq!(shown(&db), model);
    assert_eq!(db.seq(), seq);
}

/// Flushes during replay at the docid limit, against a model (ROADMAP 6.6.5): any backlog of puts, deletes and
/// `put_batch`es over two collections, logged `Buffered` or `Fsync` into chunks of any size (so a flush lands in the
/// middle of a multi-record chunk), then made durable without a commit, reopens with a few docids per memtable to
/// exactly the model's documents, losing every unsynced write or none. Opens again with another limit (or none)
/// show the same documents, and so does one after a commit that flushes nothing. Seed: `PROPTEST_RNG_SEED`.
mod docid_limit_model {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use proptest::test_runner::{RngSeed, TestCaseError, TestRunner};

    use super::options;
    use crate::db::{Database, Snapshot};
    use crate::document::{Document, Value};
    use crate::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
    use crate::wal::Durability;
    use persia_storage::container::sim::{CrashLoss, SimFile};

    const COLLECTIONS: [&str; 2] = ["a", "b"];
    const SEED: u64 = 0x0DB6_6005_0002;

    #[derive(Debug, Clone)]
    enum Op {
        Put { c: usize, id: u8, fsync: bool },
        Delete { c: usize, id: u8, fsync: bool },
        Batch { c: usize, ids: Vec<u8>, fsync: bool },
    }

    fn schema(name: &str) -> CollectionSchema {
        SchemaBuilder::new(name)
            .field("body", FieldKind::Text(TextOptions::default()))
            .build()
            .unwrap()
    }

    fn doc(id: &str, text: &str) -> Document {
        let body = schema("x").field("body").unwrap().id();
        Document::new(id).with(body, Value::String(text.into()))
    }

    fn level(fsync: bool) -> Durability {
        if fsync {
            Durability::Fsync
        } else {
            Durability::Buffered
        }
    }

    fn shown(snapshot: &Snapshot) -> BTreeMap<String, BTreeMap<String, String>> {
        let mut all = BTreeMap::new();
        for (name, c) in snapshot.collections() {
            let docs: &mut BTreeMap<String, String> = all.entry(name.to_owned()).or_default();
            for i in 0..8 {
                let id = format!("n{i}");
                if let Some(d) = c.get(&id).unwrap() {
                    let Some((_, Value::String(text))) = d.fields().next() else {
                        panic!("{id}: no body");
                    };
                    docs.insert(id, text.clone());
                }
            }
        }
        all
    }

    fn strategy() -> impl Strategy<Value = (Vec<Op>, usize, u32, Option<u32>)> {
        let c = 0..COLLECTIONS.len();
        let op = prop_oneof![
            4 => (c.clone(), 0..8u8, any::<bool>()).prop_map(|(c, id, fsync)| Op::Put { c, id, fsync }),
            2 => (c.clone(), 0..8u8, any::<bool>()).prop_map(|(c, id, fsync)| Op::Delete { c, id, fsync }),
            2 => (c, prop::collection::vec(0..8u8, 1..12), any::<bool>())
                .prop_map(|(c, ids, fsync)| Op::Batch { c, ids, fsync }),
        ];
        (
            prop::collection::vec(op, 1..48),
            prop_oneof![Just(120usize), Just(1 << 20)],
            1..6u32,
            prop_oneof![Just(None), (1..9u32).prop_map(Some)],
        )
    }

    fn run(
        ops: &[Op],
        chunk_target: usize,
        max: u32,
        other: Option<u32>,
    ) -> Result<(), TestCaseError> {
        let with = |max_docs, chunk| {
            let mut o = options(max_docs);
            o.wal_chunk_target = chunk;
            o
        };
        let file = SimFile::new();
        let mut db = Database::create_on(file.clone(), [4; 16], with(None, chunk_target)).unwrap();
        let mut model: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for c in COLLECTIONS {
            db.create_collection(schema(c)).unwrap();
            model.insert(c.to_owned(), BTreeMap::new());
        }
        let mut version = 0;
        for op in ops {
            match op {
                Op::Put { c, id, fsync } => {
                    version += 1;
                    let (id, text) = (format!("n{id}"), format!("v{version}"));
                    db.put(COLLECTIONS[*c], &doc(&id, &text), level(*fsync))
                        .unwrap();
                    model.get_mut(COLLECTIONS[*c]).unwrap().insert(id, text);
                }
                Op::Delete { c, id, fsync } => {
                    let id = format!("n{id}");
                    db.delete(COLLECTIONS[*c], &id, level(*fsync)).unwrap();
                    model.get_mut(COLLECTIONS[*c]).unwrap().remove(&id);
                }
                Op::Batch { c, ids, fsync } => {
                    let docs: Vec<(String, String)> = ids
                        .iter()
                        .map(|id| {
                            version += 1;
                            (format!("n{id}"), format!("v{version}"))
                        })
                        .collect();
                    let batch: Vec<Document> = docs.iter().map(|(i, t)| doc(i, t)).collect();
                    db.put_batch(COLLECTIONS[*c], &batch, level(*fsync))
                        .unwrap();
                    model.get_mut(COLLECTIONS[*c]).unwrap().extend(docs);
                }
            }
        }
        // Durable without a commit.
        db.delete("a", "never", Durability::Fsync).unwrap();
        drop(db);
        for loss in [CrashLoss::All, CrashLoss::Nothing] {
            let crashed = file.crash(loss);
            let db = Database::open_on(crashed.clone(), with(Some(max), chunk_target))
                .map_err(|e| TestCaseError::fail(format!("{loss:?}: first reopen: {e}")))?;
            prop_assert_eq!(&shown(&db.snapshot()), &model, "{:?}: first reopen", loss);
            drop(db);
            let again = crashed.crash(CrashLoss::All);
            let mut db = Database::open_on(again.clone(), with(other, chunk_target))
                .map_err(|e| TestCaseError::fail(format!("{loss:?}: second reopen: {e}")))?;
            prop_assert_eq!(&shown(&db.snapshot()), &model, "{:?}: second reopen", loss);
            db.create_collection(schema("z")).unwrap();
            drop(db);
            let db = Database::open_on(again.crash(CrashLoss::All), with(Some(max), chunk_target))
                .map_err(|e| TestCaseError::fail(format!("{loss:?}: third reopen: {e}")))?;
            let mut expected = model.clone();
            expected.insert("z".to_owned(), BTreeMap::new());
            prop_assert_eq!(
                &shown(&db.snapshot()),
                &expected,
                "{:?}: after a create",
                loss
            );
        }
        Ok(())
    }

    #[test]
    fn flushes_at_the_docid_limit_match_the_model() {
        let seed = std::env::var("PROPTEST_RNG_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(SEED);
        let cases = std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(if cfg!(feature = "slow") { 512 } else { 48 });
        let config = ProptestConfig {
            cases,
            rng_seed: RngSeed::Fixed(seed),
            source_file: Some(file!()),
            test_name: Some("flushes_at_the_docid_limit_match_the_model"),
            ..ProptestConfig::default()
        };
        let mut runner = TestRunner::new(config);
        if let Err(e) = runner.run(&strategy(), |(ops, chunk, max, other)| {
            run(&ops, chunk, max, other)
        }) {
            panic!("failed with PROPTEST_RNG_SEED={seed}: {e}\n{runner}");
        }
    }
}
