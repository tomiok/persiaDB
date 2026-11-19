//! GC of the blob store (SPEC §9.5; ROADMAP 9.5.1, 9.5.2, 9.5.4, 9.5.8; ADR-0052): the mark over the engine's
//! references and the pin set, the sweep, and the safety of both against documents.
//!
//! Invariants:
//! 1. The mark holds every blob a document references (memtable, segments, held snapshots, the WAL), every pinned
//!    blob and upload, and the upload each marked blob's manifest names; nothing else.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)] // Tests: inputs are fixed.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::{Blob, BlobDatabase};
use persia::internal::pins::Pinned;
use persia_blob::{BlobId, BlobStore, ChunkSize, MemoryBlobStore, UploadId};
use persia_engine::db::{Database, DatabaseOptions, SegmentIds};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed))
        .collect()
}

fn options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(ChunkSize::MIN).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn database(path: &Path) -> Database {
    let mut db = Database::create(path, DatabaseOptions::default()).unwrap();
    db.create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    db
}

fn upload_of(pins: &persia::internal::pins::Pins) -> UploadId {
    match pins.ids().first() {
        Some(Pinned::Upload(u)) => *u,
        other => panic!("first pin is not the upload: {other:?}"),
    }
}

#[test]
fn the_mark_holds_referenced_and_pinned_ids_and_the_uploads_their_manifests_name() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryBlobStore::new());
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(3));
    let mut db = BlobDatabase::new(database(&dir.path().join("app.persia")), blobs);

    // Referenced by a committed document, and by one still in the memtable.
    let committed = db
        .put(
            "c",
            &Document::new("a"),
            Blob::from_reader(&bytes(100 << 10, 1)[..]).with_options(options()),
            Durability::Fsync,
        )
        .unwrap();
    db.commit().unwrap();
    let logged = db
        .put(
            "c",
            &Document::new("b"),
            Blob::from_reader(&bytes(70 << 10, 2)[..]).with_options(options()),
            Durability::Fsync,
        )
        .unwrap();
    // Pinned by a put_blob whose caller holds the guard; and one whose guard is gone (unreferenced).
    let (pinned, pins) = db.put_blob(&bytes(80 << 10, 3)[..], &options()).unwrap();
    let (dropped, gone) = db.put_blob(&bytes(90 << 10, 4)[..], &options()).unwrap();
    drop(gone);
    // A small blob: its manifest names no upload.
    let small = db
        .put(
            "c",
            &Document::new("s"),
            Blob::from_reader(&b"tiny"[..]),
            Durability::Fsync,
        )
        .unwrap();

    let mark = db.mark().unwrap();
    let expected: HashSet<BlobId> = [&committed, &logged, &pinned, &small]
        .into_iter()
        .map(persia_blob::BlobRef::blob_id)
        .collect();
    assert_eq!(mark.blobs, expected);
    assert!(!mark.blobs.contains(&dropped.blob_id()));
    let upload =
        |r: &persia_blob::BlobRef| store.manifest(r.blob_id()).unwrap().unwrap().upload_id();
    let mut uploads: HashSet<UploadId> = [&committed, &logged, &pinned]
        .into_iter()
        .map(upload)
        .collect();
    // The upload of the pinned put is pinned too; the small blob's (bound by `bind_small` on the memory store, which
    // stages it) is named by its manifest.
    uploads.insert(upload_of(&pins));
    uploads.insert(upload(&small));
    assert_eq!(mark.uploads, uploads);
    assert!(mark.missing.is_empty() && mark.unreadable.is_empty());
}
