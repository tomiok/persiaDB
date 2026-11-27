//! No dangling blob reference (SPEC §9.5, §9.6; ROADMAP 9.5.6).
//!
//! Invariants:
//! 1. A document committed with a `BlobRef` that has no blob behind it, set below the public API
//!    (`Document::set_blob_ref`), is reported by `verify` as an error naming the collection, the document id and the
//!    `blob_id`: in a segment and in the WAL. `get_blob` fails for it.
//! 2. So is every referenced blob of a container copied without its pack (SPEC §4.8 copies both).
//! 3. The public `put(doc, blob)` given a `BlobRef` its database never bound (with no pack yet, with a pack that lacks
//!    it, or bound under the id with another size) is rejected and logs nothing: `wal_mark` is unchanged, and
//!    `verify` still passes.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::path::Path;

use persia::internal::documents::{Blob, BlobDatabase, Error, ReadOnlyBlobDatabase};
use persia::internal::verify::{Missing, Scan, VerifyError};
use persia_blob::{BlobId, BlobRef, PackStore};
use persia_engine::db::{BlobRefSite, DatabaseOptions};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;

fn create(path: &Path) -> BlobDatabase<PackStore> {
    let mut db = BlobDatabase::create(path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new("notes").build().unwrap())
        .unwrap();
    db
}

fn reader(path: &Path) -> ReadOnlyBlobDatabase {
    ReadOnlyBlobDatabase::open(path, &DatabaseOptions::default()).unwrap()
}

fn unbound() -> BlobRef {
    BlobRef::new(
        BlobId::from_bytes([0x42; 32]),
        5,
        "text/plain",
        64 << 10,
        BTreeMap::new(),
    )
    .unwrap()
}

/// `verify`'s error for `path`, at every depth: the same at each, a dangling reference.
fn dangling(path: &Path) -> (String, String, BlobId, BlobRefSite, Missing) {
    let mut found = Vec::new();
    for scan in [Scan::Quick, Scan::Checksums, Scan::Deep] {
        let err = reader(path).verify_blobs(scan).unwrap_err();
        assert!(err.is_corruption(), "{scan:?}: {err}");
        let VerifyError::Dangling {
            collection,
            id,
            blob_id,
            site,
            missing,
        } = err
        else {
            panic!("{scan:?}: {err}");
        };
        found.push((collection, id, blob_id, site, missing));
    }
    assert!(found.windows(2).all(|w| w[0] == w[1]), "{found:?}");
    found.remove(0)
}

#[test]
fn verify_names_a_dangling_reference_in_a_segment_and_in_the_wal() {
    for commit in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let mut db = create(&path);
        // A sound document beside it, so the pack exists: the dangling one is "not bound", not "no pack".
        db.put(
            "notes",
            &Document::new("sound"),
            Blob::from_reader(&b"bytes"[..]),
            Durability::Fsync,
        )
        .unwrap();
        let mut doc = Document::new("ghost");
        doc.set_blob_ref(Some(unbound()));
        db.log_mut().put("notes", &doc, Durability::Fsync).unwrap();
        if commit {
            db.commit().unwrap();
        }
        drop(db);

        let (collection, id, blob_id, site, missing) = dangling(&path);
        assert_eq!((collection.as_str(), id.as_str()), ("notes", "ghost"));
        assert_eq!(blob_id, unbound().blob_id());
        assert_eq!(missing, Missing::NotBound);
        assert_eq!(
            matches!(site, BlobRefSite::Segment { .. }),
            commit,
            "{site:?}"
        );
        assert!(matches!(
            reader(&path).get_blob(&unbound()),
            Err(Error::Blob(persia_blob::Error::BlobNotFound { .. }))
        ));
    }
}

#[test]
fn a_container_copied_without_its_pack_has_dangling_references() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut db = create(&path);
    let blob = db
        .put(
            "notes",
            &Document::new("a"),
            Blob::from_reader(&b"only in the pack"[..]),
            Durability::Fsync,
        )
        .unwrap();
    db.commit().unwrap();
    drop(db);
    let copy = dir.path().join("copy.persia");
    std::fs::copy(&path, &copy).unwrap();

    let (collection, id, blob_id, site, missing) = dangling(&copy);
    assert_eq!((collection.as_str(), id.as_str()), ("notes", "a"));
    assert_eq!(blob_id, blob.blob_id());
    assert!(matches!(site, BlobRefSite::Segment { .. }));
    assert_eq!(missing, Missing::NoPack);
    assert!(matches!(
        reader(&copy).get_blob(&blob),
        Err(Error::Blob(persia_blob::Error::BlobNotFound { .. }))
    ));
    // The original, with its pack, is sound.
    reader(&path).verify_blobs(Scan::Deep).unwrap();
}

#[test]
fn put_rejects_a_reference_its_database_never_bound_and_logs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let other = dir.path().join("other.persia");
    let mut db = create(&path);
    // Bound in another database only.
    let mut theirs = create(&other);
    let foreign = theirs
        .put(
            "notes",
            &Document::new("x"),
            Blob::from_reader(&b"elsewhere"[..]),
            Durability::Fsync,
        )
        .unwrap();
    drop(theirs);

    let rejected = |db: &mut BlobDatabase<PackStore>, r: &BlobRef, what: &str| {
        let before = db.log().wal_mark();
        let err = db
            .put(
                "notes",
                &Document::new("d"),
                Blob::Stored(r.clone()),
                Durability::Fsync,
            )
            .unwrap_err();
        assert_eq!(db.log().wal_mark(), before, "{what}: something was logged");
        err
    };
    // No pack yet.
    assert!(!db.blobs().store().exists());
    for r in [unbound(), foreign.clone()] {
        let err = rejected(&mut db, &r, "no pack");
        assert!(
            matches!(err, Error::BlobNotStored { blob_id } if blob_id == r.blob_id()),
            "{err}"
        );
    }
    assert!(
        !db.blobs().store().exists(),
        "a rejected put creates no pack"
    );

    // A pack that lacks them.
    let ours = db
        .put(
            "notes",
            &Document::new("ours"),
            Blob::from_reader(&b"ours"[..]),
            Durability::Fsync,
        )
        .unwrap();
    for r in [unbound(), foreign] {
        let err = rejected(&mut db, &r, "a pack without it");
        assert!(
            matches!(err, Error::BlobNotStored { blob_id } if blob_id == r.blob_id()),
            "{err}"
        );
    }
    // Bound under the id, but not with this size: the reference does not match it, the caller's error (ROADMAP
    // 10.1.33), not damage.
    let wrong = BlobRef::new(
        ours.blob_id(),
        ours.size() + 1,
        ours.mime(),
        ours.chunk_size(),
        BTreeMap::new(),
    )
    .unwrap();
    let err = rejected(&mut db, &wrong, "a mismatched reference");
    assert!(
        matches!(&err, Error::Blob(e @ persia_blob::Error::BlobRefMismatch { .. }) if !e.is_corruption()),
        "{err}"
    );
    assert!(!err.is_corruption(), "{err}");
    // The reference it bound itself is taken.
    db.put(
        "notes",
        &Document::new("again"),
        Blob::Stored(ours),
        Durability::Fsync,
    )
    .unwrap();
    drop(db);
    let report = reader(&path).verify_blobs(Scan::Deep).unwrap();
    assert_eq!((report.references, report.blobs), (2, 1));
}
