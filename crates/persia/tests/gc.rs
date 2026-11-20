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
use std::time::Duration;

use persia::internal::blob::MAX_BIND_BUDGET;
use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::{Blob, BlobDatabase};
use persia::internal::gc::{BlobReferences, GcError, GcOptions};
use persia::internal::pins::Pinned;
use persia_blob::gc::Rewrite;
use persia_blob::{BlobId, BlobStore, ChunkSize, MemoryBlobStore, PackStore, UploadId};
use persia_engine::db::{Database, DatabaseError, DatabaseOptions, SegmentIds};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::{Durability, WalMark};

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

/// Default options, the pack rewritten whatever it drops.
const FORCE: GcOptions = GcOptions {
    grace: persia::internal::gc::DEFAULT_GRACE,
    rewrite_ratio: 0.0,
    now: None,
};

fn pack_database(dir: &Path) -> BlobDatabase<PackStore> {
    let log = database(&dir.join("app.persia"));
    let pack = PackStore::open_writer(dir.join("app.persia.blobs"), &log.writer_proof()).unwrap();
    let blobs = Blobs::with_upload_ids(Arc::new(pack), SegmentIds::seeded(5));
    BlobDatabase::new(log, blobs)
}

fn put_doc(db: &mut BlobDatabase<PackStore>, id: &str, seed: u8) -> persia_blob::BlobRef {
    db.put(
        "c",
        &Document::new(id),
        Blob::from_reader(&bytes(70 << 10, seed)[..]).with_options(options()),
        Durability::Fsync,
    )
    .unwrap()
}

#[test]
fn gc_drops_the_blobs_of_deleted_documents_and_released_pins_and_keeps_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = pack_database(dir.path());
    let kept = put_doc(&mut db, "kept", 1);
    let deleted = put_doc(&mut db, "deleted", 2);
    let replaced = put_doc(&mut db, "replaced", 3);
    db.commit().unwrap();
    let replacement = put_doc(&mut db, "replaced", 4);
    db.log_mut()
        .delete("c", "deleted", Durability::Fsync)
        .unwrap();
    let (held, pins) = db.put_blob(&bytes(80 << 10, 5)[..], &options()).unwrap();
    let (released, gone) = db.put_blob(&bytes(90 << 10, 6)[..], &options()).unwrap();
    drop(gone);
    // Before the commit, the memtable still holds the replaced and deleted documents' puts: GC keeps their blobs.
    let report = db.gc(&FORCE).unwrap();
    assert_eq!(report.sweep.blobs_deleted, 1, "{report:?}");
    let store = Arc::clone(db.blobs().store());
    assert!(!store.contains(&released).unwrap());
    for r in [&kept, &deleted, &replaced, &replacement, &held] {
        assert!(store.contains(r).unwrap());
    }
    db.commit().unwrap();
    let report = db.gc(&FORCE).unwrap();
    assert_eq!(report.sweep.blobs_deleted, 2, "{report:?}");
    for r in [&deleted, &replaced] {
        assert!(!store.contains(r).unwrap());
    }
    for r in [&kept, &replacement, &held] {
        assert_eq!(
            db.get_blob_range(r, 0..r.size()).unwrap().len() as u64,
            r.size()
        );
    }
    drop(pins);
    let report = db.gc(&FORCE).unwrap();
    assert_eq!(report.sweep.blobs_deleted, 1);
    assert!(!store.contains(&held).unwrap());
    assert!(matches!(report.sweep.rewrite, Some(Rewrite::Done { .. })));
}

#[test]
fn gc_rejects_a_grace_below_twice_the_bind_budget_and_a_bad_ratio() {
    let dir = tempfile::tempdir().unwrap();
    let db = pack_database(dir.path());
    // One hour is the longest bind budget, and still too short: a `put_if_absent` started just before the deadline
    // can land after it (SPEC §9.4). Two hours is the floor.
    let at_budget = GcOptions {
        grace: MAX_BIND_BUDGET,
        ..GcOptions::default()
    };
    assert!(matches!(
        db.gc(&at_budget),
        Err(GcError::GraceBelowBindBudget { grace, floor })
            if grace == MAX_BIND_BUDGET && floor == 2 * MAX_BIND_BUDGET
    ));
    let at_floor = GcOptions {
        grace: 2 * MAX_BIND_BUDGET,
        ..GcOptions::default()
    };
    db.gc(&at_floor).unwrap();
    assert_eq!(MAX_BIND_BUDGET, Duration::from_hours(1));
    assert_eq!(2 * MAX_BIND_BUDGET, Duration::from_hours(2));
    for ratio in [f64::NAN, -0.1] {
        let bad = GcOptions {
            rewrite_ratio: ratio,
            ..GcOptions::default()
        };
        assert!(matches!(db.gc(&bad), Err(GcError::InvalidRewriteRatio(_))));
    }
    assert_eq!(GcOptions::default().grace, Duration::from_hours(24));
    assert!((GcOptions::default().rewrite_ratio - 0.5).abs() < f64::EPSILON);
}

/// A log that refuses to list its references, as a poisoned database does.
struct Poisoned(Database);

impl persia::internal::documents::DocumentLog for Poisoned {
    fn put(&mut self, _: &str, _: &Document, _: Durability) -> Result<(), DatabaseError> {
        Err(DatabaseError::Poisoned)
    }
    fn wal_mark(&self) -> WalMark {
        self.0.wal_mark()
    }
    fn durable_mark(&self) -> WalMark {
        self.0.durable_mark()
    }
    fn commit(&mut self) -> Result<u64, DatabaseError> {
        Err(DatabaseError::Poisoned)
    }
}

impl BlobReferences for Poisoned {
    fn referenced_blobs(&self) -> Result<HashSet<BlobId>, DatabaseError> {
        Err(DatabaseError::Poisoned)
    }
}

#[test]
fn gc_stops_when_the_log_or_the_store_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = pack_database(dir.path());
    let blob = put_doc(&mut db, "a", 1);
    // A poisoned log: nothing is swept, whatever the pins say.
    let store = Arc::clone(db.blobs().store());
    let log = Poisoned(database(&dir.path().join("poisoned.persia")));
    let poisoned = BlobDatabase::new(log, Blobs::new(Arc::clone(&store)));
    assert!(matches!(
        poisoned.gc(&FORCE),
        Err(GcError::Database(e)) if matches!(*e, DatabaseError::Poisoned)
    ));
    assert!(store.contains(&blob).unwrap());
    // A read-only pack cannot be swept.
    let db_uuid = db.log().container().header().db_uuid;
    let ro = PackStore::open(dir.path().join("app.persia.blobs"), db_uuid).unwrap();
    let ro_db = BlobDatabase::new(
        database(&dir.path().join("other.persia")),
        Blobs::new(Arc::new(ro)),
    );
    assert!(matches!(
        ro_db.gc(&FORCE),
        Err(GcError::Blob(persia_blob::Error::PackReadOnly))
    ));
}
