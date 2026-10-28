//! The golden databases (ROADMAP 6.3.8, 6.4.3; SPEC §4.4, §8.3, §9.1, §15): containers written by the commit path. The
//! first holds a Segment frame whose `BlobRefs` section has a `BlobRef` with non-empty `user_meta`, and one WAL record
//! after the last commit; the second, tombstone sidecars.
//!
//! Invariants:
//! 1. Writing the database again, with the same ids, clock and documents, gives exactly the bytes of
//!    `tests/fixtures/database/v1.0/blob-ref.persia`: a change to the commit path, the segment writer or any
//!    encoding inside shows up here. The file is never regenerated once released (SPEC §15);
//!    `PERSIA_WRITE_DATABASE_FIXTURES=1` writes it when missing.
//! 2. Opening the fixture shows the committed segment with the blob reference and its `user_meta`, and the replayed
//!    document after it.
//! 3. `tombstones.persia` (ROADMAP 6.4.3, ADR-0030), written the same way, holds three segments, a second sidecar
//!    version that deletes all of the first segment, and a delete in the WAL; it is stable byte for byte and opens to
//!    the documents the commits left, the replayed delete hiding a segment's document.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use persia_engine::db::{Database, DatabaseOptions, ManualClock, SegmentIds};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::IdState;
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_format::blob::{BlobId, BlobRef};
use persia_format::frame::FrameKind;
use persia_storage::container::sim::SimFile;
use persia_storage::container::{Container, Mode};

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/database/v1.0/blob-ref.persia")
}

fn schema() -> CollectionSchema {
    SchemaBuilder::new("files")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("kind", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap()
}

fn blob_ref() -> BlobRef {
    let meta = BTreeMap::from([
        ("camera".to_owned(), "X100V".to_owned()),
        ("taken".to_owned(), "2026-10-01".to_owned()),
    ]);
    BlobRef::new(
        BlobId::from_bytes([0xab; 32]),
        3_000_000,
        "image/jpeg",
        1 << 20,
        meta,
    )
    .unwrap()
}

fn docs() -> (Document, Document, Document) {
    let s = schema();
    let title = s.field("title").unwrap().id();
    let kind = s.field("kind").unwrap().id();
    let mut photo = Document::new("photo-1")
        .with(title, Value::String("Sunset over the harbour".into()))
        .with(kind, Value::String("image".into()));
    photo.set_blob_ref(Some(blob_ref()));
    let note = Document::new("note-1")
        .with(title, Value::String("Shopping list".into()))
        .with(kind, Value::String("text".into()));
    let late = Document::new("note-2")
        .with(title, Value::String("Written after the commit".into()))
        .with(kind, Value::String("text".into()));
    (photo, note, late)
}

fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_790_000_000_000_000)),
        segment_ids: SegmentIds::seeded(2026),
        ..DatabaseOptions::default()
    }
}

/// The database, written by the commit path: create, collection, two puts and a flush, then one put left in the WAL.
fn write() -> Vec<u8> {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), *b"persia-golden-db", options()).unwrap();
    db.create_collection(schema()).unwrap();
    let (photo, note, late) = docs();
    db.put_batch("files", &[photo, note], Durability::Fsync)
        .unwrap();
    assert_eq!(db.commit().unwrap(), 3);
    db.put("files", &late, Durability::Fsync).unwrap();
    drop(db);
    file.durable_bytes()
}

#[test]
fn golden_database_is_stable_and_opens() {
    let path = path();
    let written = write();
    if std::env::var_os("PERSIA_WRITE_DATABASE_FIXTURES").is_some() && !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &written).unwrap();
    }
    let stored = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} (PERSIA_WRITE_DATABASE_FIXTURES=1 writes it)",
            path.display()
        )
    });
    assert_eq!(
        written, stored,
        "the commit path changed the database's bytes"
    );

    // The frames, as a container reader sees them.
    let container =
        Container::open_on(SimFile::from_bytes(stored.clone()), Mode::ReadOnly).unwrap();
    assert_eq!(container.superblock().seq, 3);
    let kinds: Vec<FrameKind> = container.frames().iter().map(|f| f.kind).collect();
    assert_eq!(kinds, [FrameKind::Manifest, FrameKind::WalChunk]);
    drop(container);

    let db = Database::open_on(SimFile::from_bytes(stored), options()).unwrap();
    let snapshot = db.snapshot();
    let files = snapshot.collection("files").unwrap();
    assert_eq!(files.segments().len(), 1);
    let segment = &files.segments()[0];
    let (photo, note, late) = docs();
    let (docid, found) = segment.document_by_id("photo-1").unwrap().unwrap();
    assert_eq!(found, photo);
    let blob = segment.blob_ref(docid).unwrap().unwrap();
    assert_eq!(blob, blob_ref());
    assert!(!blob.user_meta().is_empty());
    assert_eq!(segment.document_by_id("note-1").unwrap().unwrap().1, note);
    let memtable = files.memtable();
    let Some(IdState::Live(late_id)) = memtable.lookup("note-2") else {
        panic!("note-2 is not replayed");
    };
    assert_eq!(memtable.document(late_id).unwrap(), late);
}

fn tombstones_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/database/v1.0/tombstones.persia")
}

/// The database with tombstone sidecars (ROADMAP 6.4.3), written by the commit path: `photo-1` and `note-1` flushed
/// into S1; `note-1` deleted and `note-2` put (S1's sidecar v1, S2); `photo-1` replaced (S3, S1's sidecar v2, which
/// deletes all of S1); then a delete of `note-2` left in the WAL.
fn write_tombstones() -> Vec<u8> {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), *b"persia-golden-tb", options()).unwrap();
    db.create_collection(schema()).unwrap();
    let (photo, note, late) = docs();
    db.put_batch("files", &[photo.clone(), note], Durability::Fsync)
        .unwrap();
    assert_eq!(db.commit().unwrap(), 3);
    db.delete("files", "note-1", Durability::Fsync).unwrap();
    db.put("files", &late, Durability::Fsync).unwrap();
    assert_eq!(db.commit().unwrap(), 4);
    db.put("files", &replaced(&photo), Durability::Fsync)
        .unwrap();
    assert_eq!(db.commit().unwrap(), 5);
    db.delete("files", "note-2", Durability::Fsync).unwrap();
    drop(db);
    file.durable_bytes()
}

fn replaced(photo: &Document) -> Document {
    let title = schema().field("title").unwrap().id();
    photo
        .clone()
        .with(title, Value::String("Sunrise over the harbour".into()))
}

#[test]
fn golden_database_with_tombstones_is_stable_and_opens() {
    let path = tombstones_path();
    let written = write_tombstones();
    if std::env::var_os("PERSIA_WRITE_DATABASE_FIXTURES").is_some() && !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &written).unwrap();
    }
    let stored = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} (PERSIA_WRITE_DATABASE_FIXTURES=1 writes it)",
            path.display()
        )
    });
    assert_eq!(
        written, stored,
        "the commit path changed the database's bytes"
    );

    let db = Database::open_on(SimFile::from_bytes(stored), options()).unwrap();
    assert_eq!(db.seq(), 5);
    let entries = db
        .manifest()
        .collection("files")
        .unwrap()
        .segments()
        .to_vec();
    let sidecars: Vec<Option<(u64, u32)>> = entries
        .iter()
        .map(|e| e.tombstones.map(|t| (t.version, t.deleted)))
        .collect();
    assert_eq!(sidecars, [Some((2, 2)), None, None]);
    let snapshot = db.snapshot();
    let files = snapshot.collection("files").unwrap();
    // The decoded sidecar, not only the manifest's counts.
    let deleted: Vec<u32> = files.tombstones()[0]
        .as_ref()
        .unwrap()
        .docids()
        .iter()
        .collect();
    assert_eq!(deleted, [0, 1]);
    let (photo, _, _) = docs();
    assert_eq!(files.get("photo-1").unwrap(), Some(replaced(&photo)));
    assert_eq!(files.get("note-1").unwrap(), None);
    // In S2, hidden by the replayed delete.
    assert_eq!(files.get("note-2").unwrap(), None);
    assert!(
        files.segments()[1]
            .document_by_id("note-2")
            .unwrap()
            .is_some()
    );
    assert_eq!(files.memtable().lookup("note-2"), Some(IdState::Deleted));
}
