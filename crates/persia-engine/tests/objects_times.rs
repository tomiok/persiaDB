//! The container's object view reports no object times (ROADMAP 9.5.5; ADR-0050 item 6): its objects are frames of
//! one real file, which have no modification times of their own, so `list` says `None` (unknown), never the file's
//! time, for every object listed, also after a reopen.

#![allow(clippy::unwrap_used, missing_docs)]

use persia_engine::db::{Database, DatabaseOptions};
use persia_engine::document::{Document, Value};
use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::Durability;
use persia_storage::{KeyPrefix, Storage};

fn listed_times(db: &Database<std::fs::File>) -> Vec<(String, bool)> {
    db.objects()
        .list(&KeyPrefix::new("").unwrap())
        .unwrap()
        .into_iter()
        .map(|m| (m.key.as_str().to_owned(), m.last_modified.is_none()))
        .collect()
}

#[test]
fn container_objects_on_a_real_file_report_no_times() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let schema = SchemaBuilder::new("a")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    for (id, text) in [("x", "one"), ("y", "two")] {
        let doc = Document::new(id).with(body, Value::String(text.into()));
        db.put("a", &doc, Durability::Fsync).unwrap();
    }
    db.commit().unwrap();
    db.put(
        "a",
        &Document::new("z").with(body, Value::String("three".into())),
        Durability::Fsync,
    )
    .unwrap();
    let before = listed_times(&db);
    // WAL chunks are listed once a manifest names them (`database.rs` covers that); these two always are.
    let kinds = ["manifests/", "segments/"];
    for kind in kinds {
        assert!(
            before.iter().any(|(k, _)| k.starts_with(kind)),
            "no {kind} object in {before:?}"
        );
    }
    assert!(before.iter().all(|(_, none)| *none), "{before:?}");
    drop(db);

    let db = Database::open(&path, DatabaseOptions::default()).unwrap();
    let after = listed_times(&db);
    assert!(
        !after.is_empty() && after.iter().all(|(_, none)| *none),
        "{after:?}"
    );
}
