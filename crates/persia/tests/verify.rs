//! The blob checks of `persia verify` (SPEC §9.5; ROADMAP 9.5.3; ADR-0053), over a database and its pack.
//!
//! Invariants:
//! 1. A sound database passes at every depth: every reference counted, the scan covering every committed frame, and
//!    `Deep` re-hashing every referenced blob.
//! 2. A reference whose blob is bound under its id but does not match it (`check_ref`: size or chunk size) is
//!    dangling, as one whose blob is not bound.
//! 3. Only live documents count in a segment: a dangling reference a committed delete removed is not an error. Every
//!    put of the WAL from `wal_start` on counts, replaced since or not (what GC's mark keeps).
//! 4. `Deep` finds a bound blob whose bytes, every CRC right, hash to another id; the default scan does not.
//! 5. Every failure is a corruption (`VerifyError::is_corruption`).
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)] // Tests: inputs are fixed.

use std::path::Path;

use persia::internal::documents::{Blob, BlobDatabase, ReadOnlyBlobDatabase};
use persia::internal::verify::{Missing, Scan, VerifyError};
use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobRef, BlobStore, ChunkSize, PackStore, UploadId,
    blob_id_of, chunk_all,
};
use persia_engine::db::{BlobRefSite, DatabaseOptions};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;

fn create(path: &Path) -> BlobDatabase<PackStore> {
    let mut db = BlobDatabase::create(path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    db
}

fn verify(path: &Path, scan: Scan) -> Result<persia::internal::verify::BlobReport, VerifyError> {
    ReadOnlyBlobDatabase::open(path, &DatabaseOptions::default())
        .unwrap()
        .verify_blobs(scan)
}

fn reference(blob_id: BlobId, size: u64, chunk: u32) -> BlobRef {
    BlobRef::new(
        blob_id,
        size,
        "a/b",
        chunk,
        std::collections::BTreeMap::new(),
    )
    .unwrap()
}

/// Logs `id` with `blob_ref` below the public API (`Document::set_blob_ref`), at `Fsync`.
fn put_raw(db: &mut BlobDatabase<PackStore>, id: &str, blob_ref: BlobRef) {
    let mut doc = Document::new(id);
    doc.set_blob_ref(Some(blob_ref));
    db.log_mut().put("c", &doc, Durability::Fsync).unwrap();
}

fn put(db: &mut BlobDatabase<PackStore>, id: &str, bytes: &[u8]) -> BlobRef {
    db.put(
        "c",
        &Document::new(id),
        Blob::from_reader(bytes),
        Durability::Fsync,
    )
    .unwrap()
}

#[test]
fn a_sound_database_passes_at_every_depth() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut db = create(&path);
    let shared = put(&mut db, "a", &[1; 5000]);
    db.commit().unwrap();
    put(&mut db, "b", &vec![2; 70_000]);
    // A second document of the same blob, in the WAL: two references, one blob.
    db.put(
        "c",
        &Document::new("c"),
        Blob::Stored(shared),
        Durability::Fsync,
    )
    .unwrap();
    drop(db);
    for scan in [Scan::Quick, Scan::Checksums, Scan::Deep] {
        let report = verify(&path, scan).unwrap();
        assert_eq!(
            (report.references, report.blobs, report.pack, report.bound),
            (3, 2, true, 2),
            "{scan:?}"
        );
        assert_eq!(report.scan.is_some(), scan != Scan::Quick, "{scan:?}");
        assert_eq!(
            report.rehashed,
            (scan == Scan::Deep).then_some(2),
            "{scan:?}"
        );
    }
    let scan = verify(&path, Scan::Checksums).unwrap().scan.unwrap();
    assert_eq!((scan.frames, scan.chunks, scan.indexes), (4, 2, 2));
}

#[test]
fn a_reference_its_bound_manifest_does_not_match_is_dangling() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut db = create(&path);
    let good = put(&mut db, "a", b"twelve bytes");
    // The right id, the wrong size.
    put_raw(
        &mut db,
        "b",
        reference(good.blob_id(), 13, good.chunk_size()),
    );
    drop(db);
    let err = verify(&path, Scan::Quick).unwrap_err();
    assert!(err.is_corruption());
    let VerifyError::Dangling {
        collection,
        id,
        blob_id,
        site,
        missing,
    } = err
    else {
        panic!("{err}");
    };
    assert_eq!((collection.as_str(), id.as_str()), ("c", "b"));
    assert_eq!(blob_id, good.blob_id());
    assert!(matches!(site, BlobRefSite::Wal { .. }));
    assert!(matches!(missing, Missing::Mismatch(_)), "{missing}");
}

#[test]
fn only_live_segment_documents_and_every_wal_put_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut db = create(&path);
    put(&mut db, "keep", b"kept");
    let ghost = BlobId::from_bytes([3; 32]);
    put_raw(&mut db, "gone", reference(ghost, 1, ChunkSize::MIN));
    db.commit().unwrap();
    db.log_mut().delete("c", "gone", Durability::Fsync).unwrap();
    db.commit().unwrap();
    drop(db);
    // Deleted and committed: not live, so not checked.
    assert_eq!(verify(&path, Scan::Checksums).unwrap().references, 1);

    // A WAL put replaced since still counts: a reopen before the next commit replays it.
    let mut db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
    put_raw(&mut db, "late", reference(ghost, 1, ChunkSize::MIN));
    put(&mut db, "late", b"replaced");
    drop(db);
    let err = verify(&path, Scan::Quick).unwrap_err();
    assert!(
        matches!(&err, VerifyError::Dangling { id, blob_id, missing: Missing::NotBound, .. }
            if id == "late" && *blob_id == ghost),
        "{err}"
    );
}

#[test]
fn deep_finds_bytes_that_hash_to_another_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut db = create(&path);
    put(&mut db, "a", b"real");
    // Bound under an id that is not its bytes' hash: every CRC32C is right, so only a re-hash tells.
    let bytes = b"not what the id says";
    let liar = BlobId::from_bytes([9; 32]);
    let chunk = ChunkSize::new(ChunkSize::MIN).unwrap();
    let mut crcs = Vec::new();
    chunk_all(&bytes[..], chunk, |c| {
        crcs.push(c.crc32c());
        Ok(())
    })
    .unwrap();
    let m = BlobManifest::new(liar, bytes.len() as u64, chunk, UploadId::new(77), crcs).unwrap();
    let store = db.blobs().store();
    store.stage_chunk(m.upload_id(), 0, bytes).unwrap();
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    put_raw(
        &mut db,
        "b",
        reference(liar, bytes.len() as u64, chunk.get()),
    );
    drop(db);

    verify(&path, Scan::Checksums).unwrap();
    let err = verify(&path, Scan::Deep).unwrap_err();
    assert!(err.is_corruption());
    assert!(
        matches!(&err, VerifyError::HashMismatch { collection, id, blob_id, actual }
            if collection == "c" && id == "b" && *blob_id == liar && *actual == blob_id_of(bytes)),
        "{err}"
    );
}
