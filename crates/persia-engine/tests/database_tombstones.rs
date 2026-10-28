//! Upserts and deletes across segments (ROADMAP 6.4; SPEC §8.3, §8.7.2, ADR-0030).
//!
//! Invariants:
//! 1. `CollectionSnapshot::lookup` finds an id's live version: the memtable's version or delete hides every segment's,
//!    then the segments newest first, deleted documents left out (6.4.1).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_engine::db::{Database, DatabaseOptions, Located, ManualClock, SegmentIds};
use persia_engine::document::{Document, Value};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

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

fn text(d: &Document) -> &str {
    let body = schema("x").field("body").unwrap().id();
    match d.get(body) {
        Some(Value::String(s)) => s,
        other => panic!("{other:?}"),
    }
}

fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    }
}

fn database() -> Database<SimFile> {
    let mut db = Database::create_on(SimFile::new(), [3; 16], options()).unwrap();
    db.create_collection(schema("a")).unwrap();
    db
}

#[test]
fn lookup_prefers_the_memtable_then_the_newest_segment() {
    let mut db = database();
    db.put("a", &doc("x", "one"), Durability::Fsync).unwrap();
    db.put("a", &doc("y", "one"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("a", &doc("x", "two"), Durability::Fsync).unwrap();
    db.commit().unwrap();

    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    let view = a.memtable();
    let x = a.lookup(&view, "x").unwrap().unwrap();
    assert!(
        matches!(x, Located::Segment { index: 1, docid: 0 }),
        "{x:?}"
    );
    assert_eq!(text(&a.get("x").unwrap().unwrap()), "two");
    assert_eq!(
        a.lookup(&view, "y").unwrap(),
        Some(Located::Segment { index: 0, docid: 1 })
    );
    assert_eq!(a.lookup(&view, "never").unwrap(), None);

    // The memtable's version and its delete hide the segments'.
    db.put("a", &doc("x", "three"), Durability::Fsync).unwrap();
    db.delete("a", "never", Durability::Fsync).unwrap();
    let a = db.snapshot();
    let a = a.collection("a").unwrap();
    let view = a.memtable();
    assert_eq!(a.lookup(&view, "x").unwrap(), Some(Located::Memtable(0)));
    assert_eq!(text(&a.get("x").unwrap().unwrap()), "three");
    assert_eq!(a.lookup(&view, "never").unwrap(), None);
    // A view taken before a write does not see it.
    let before = a.memtable();
    db.put("a", &doc("z", "four"), Durability::Fsync).unwrap();
    assert_eq!(a.lookup(&before, "z").unwrap(), None);
    assert_eq!(text(&a.get("z").unwrap().unwrap()), "four");
}
