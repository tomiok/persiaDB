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
