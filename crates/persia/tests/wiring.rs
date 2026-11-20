//! A database opened with its blob pack (SPEC §9.3; ROADMAP 9.2.7, 9.4.5; ADR-0053).
//!
//! Invariants:
//! 1. Creating or opening a database creates no `<name>.persia.blobs`; the first `put_blob` creates it, with the
//!    container's `db_uuid` in its header.
//! 2. A reopened database reads the blobs its documents reference; a second writer fails on the container's lock, so
//!    no second read-write pack exists.
//! 3. A read-only database opens its pack read-only and never creates it: with no pack, `get_blob` fails as not
//!    bound, and its refresh (a newer read-only open of the documents, then the pack) finds a pack created since.
//! 4. The pack is found beside the container's canonical path, whatever name opened it.
//! 5. A create with a file at the pack's path fails before creating anything.
//! 6. A refresh takes only the same database at the same canonical path and no older open: a lower `seq`, or the
//!    same `seq` with fewer replayed WAL records or an earlier end.
//! 7. A GC rewrite (`PackReplaced`) reopens the pack; the reader then sees every blob, including one bound after
//!    the rewrite. Any other pack error leaves the documents and the pack as they were.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::io::Read;
use std::path::Path;

use persia::internal::blob::PutBlobOptions;
use persia::internal::documents::{Blob, BlobDatabase, Error, ReadOnlyBlobDatabase};
use persia::internal::gc::GcOptions;
use persia_blob::gc::Rewrite;
use persia_blob::{BlobStore, PackMode, PackStore, pack_path};
use persia_engine::db::{Database, DatabaseError, DatabaseOptions, ReadOnlyDatabase};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_storage::container::Error as ContainerError;

fn create(path: &Path) -> BlobDatabase<PackStore> {
    let mut db = BlobDatabase::create(path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    db
}

fn read_all(db: &ReadOnlyBlobDatabase, blob: &persia_blob::BlobRef) -> Vec<u8> {
    let mut out = Vec::new();
    db.get_blob(blob).unwrap().read_to_end(&mut out).unwrap();
    out
}

#[test]
fn creating_or_opening_a_database_creates_no_pack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let pack = pack_path(&path);
    let db = create(&path);
    assert_eq!(db.blobs().store().mode(), PackMode::ReadWrite);
    assert!(!db.blobs().store().exists());
    assert!(!pack.exists(), "a create makes no pack");
    drop(db);
    let db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
    assert!(!pack.exists(), "an open makes no pack");

    // The first put_blob creates it, with the container's id.
    let (blob, _pins) = db
        .put_blob(&b"first"[..], &PutBlobOptions::default())
        .unwrap();
    assert!(pack.exists());
    let db_uuid = db.log().container().header().db_uuid;
    assert_eq!(db.blobs().store().header().unwrap().db_uuid, db_uuid);
    assert_eq!(db.get_blob_range(&blob, 0..5).unwrap(), b"first");
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        names.iter().all(|n| !n.contains(".tmp.")),
        "no temporary file left: {names:?}"
    );
}

#[test]
fn a_reopened_database_reads_its_documents_blobs_and_has_one_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut db = create(&path);
    let blob = db
        .put(
            "c",
            &Document::new("a"),
            Blob::from_reader(&b"kept bytes"[..]),
            Durability::Fsync,
        )
        .unwrap();
    // A second writer fails on the container's lock: no second proof, no second read-write pack.
    let second = BlobDatabase::open(&path, DatabaseOptions::default());
    assert!(
        matches!(
            &second,
            Err(Error::Database(e)) if matches!(**e, DatabaseError::Container(ContainerError::Locked))
        ),
        "{second:?}"
    );
    assert_eq!(
        db.log().writer_proof().db_uuid(),
        db.log().container().header().db_uuid
    );
    drop(db);

    let db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
    let doc = db
        .log()
        .snapshot()
        .collection("c")
        .unwrap()
        .get("a")
        .unwrap()
        .unwrap();
    assert_eq!(doc.blob_ref(), Some(&blob));
    assert_eq!(db.get_blob_range(&blob, 0..10).unwrap(), b"kept bytes");
}

#[test]
fn a_read_only_database_never_creates_its_pack_and_its_refresh_finds_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let pack = pack_path(&path);
    let options = DatabaseOptions::default();
    let mut writer = create(&path);
    let mut reader = ReadOnlyBlobDatabase::open(&path, &options).unwrap();
    assert_eq!(reader.blobs().store().mode(), PackMode::ReadOnly);
    assert!(reader.database().container().writer_proof().is_none());
    assert!(!pack.exists(), "a read-only open makes no pack");
    // Nothing new, and still no pack.
    assert!(
        !reader
            .refresh(ReadOnlyDatabase::open(&path, &options.registry).unwrap())
            .unwrap()
    );
    assert!(!pack.exists());

    let blob = writer
        .put(
            "c",
            &Document::new("a"),
            Blob::from_reader(&b"made later"[..]),
            Durability::Fsync,
        )
        .unwrap();
    // The reader's pack had none: not bound.
    assert!(matches!(
        reader.get_blob(&blob),
        Err(Error::Blob(persia_blob::Error::BlobNotFound { blob_id })) if blob_id == blob.blob_id()
    ));
    assert!(
        reader
            .blobs()
            .store()
            .manifest(blob.blob_id())
            .unwrap()
            .is_none()
    );

    // Refreshed: the newer documents, then the pack the writer created since.
    assert!(
        reader
            .refresh(ReadOnlyDatabase::open(&path, &options.registry).unwrap())
            .unwrap()
    );
    let doc = reader
        .database()
        .snapshot()
        .collection("c")
        .unwrap()
        .get("a")
        .unwrap()
        .unwrap();
    assert_eq!(doc.blob_ref(), Some(&blob));
    assert_eq!(read_all(&reader, &blob), b"made later");
    assert_eq!(reader.get_blob_range(&blob, 5..10).unwrap(), b"later");

    // A reader that opens after the pack exists finds it at once.
    let late = ReadOnlyBlobDatabase::open(&path, &options).unwrap();
    assert_eq!(read_all(&late, &blob), b"made later");
}

#[test]
fn a_refresh_with_another_database_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let other = dir.path().join("other.persia");
    let options = DatabaseOptions::default();
    drop(create(&path));
    drop(Database::create(&other, DatabaseOptions::default()).unwrap());
    let mut reader = ReadOnlyBlobDatabase::open(&path, &options).unwrap();
    let before = reader.database().container().header().db_uuid;
    let refreshed = reader.refresh(ReadOnlyDatabase::open(&other, &options.registry).unwrap());
    assert!(matches!(
        refreshed,
        Err(Error::Blob(
            persia_blob::Error::PackOfAnotherDatabase { .. }
        ))
    ));
    assert_eq!(reader.database().container().header().db_uuid, before);
}

#[test]
fn a_pack_of_another_database_fails_both_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let other = dir.path().join("other.persia");
    drop(create(&path));
    let db = create(&other);
    let (_, pins) = db
        .put_blob(&b"foreign"[..], &PutBlobOptions::default())
        .unwrap();
    drop((pins, db));
    // Only the other database's pack is copied beside this one (SPEC §4.8 copies both files, together).
    std::fs::copy(pack_path(&other), pack_path(&path)).unwrap();
    assert!(matches!(
        BlobDatabase::open(&path, DatabaseOptions::default()),
        Err(Error::Blob(
            persia_blob::Error::PackOfAnotherDatabase { .. }
        ))
    ));
    assert!(matches!(
        ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default()),
        Err(Error::Blob(
            persia_blob::Error::PackOfAnotherDatabase { .. }
        ))
    ));
}

#[cfg(unix)]
#[test]
fn the_pack_is_found_beside_the_containers_canonical_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let link = dir.path().join("link.persia");
    drop(create(&path));
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let db = BlobDatabase::open(&link, DatabaseOptions::default()).unwrap();
    let (blob, _pins) = db
        .put_blob(&b"through a link"[..], &PutBlobOptions::default())
        .unwrap();
    drop(db);
    assert!(pack_path(&path).exists());
    assert!(!pack_path(&link).exists());
    let reader = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default()).unwrap();
    assert_eq!(read_all(&reader, &blob), b"through a link");
}

/// Invariant 5: a create with a file already at the pack's path fails before it creates anything (the database it
/// would leave could never open), and succeeds once that file is gone.
#[test]
fn a_create_over_a_leftover_pack_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let other = dir.path().join("other.persia");
    let db = create(&other);
    let (_, pins) = db
        .put_blob(&b"leftover"[..], &PutBlobOptions::default())
        .unwrap();
    drop((pins, db));
    std::fs::copy(pack_path(&other), pack_path(&path)).unwrap();
    let created = BlobDatabase::create(&path, DatabaseOptions::default());
    assert!(
        matches!(&created, Err(Error::PackExists { path: p }) if *p == pack_path(&path)),
        "{created:?}"
    );
    assert!(!path.exists(), "no container is left behind");
    std::fs::remove_file(pack_path(&path)).unwrap();
    drop(create(&path));
    BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
}

/// Invariant 6: a refresh takes only this database opened again: a copy at another path (same `db_uuid`) or an open
/// older than the current one fails, and the reader keeps what it had.
#[test]
fn a_refresh_with_a_copy_or_an_older_open_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let copy = dir.path().join("copy.persia");
    let options = DatabaseOptions::default();
    let mut writer = create(&path);
    let older = ReadOnlyDatabase::open(&path, &options.registry).unwrap();
    writer
        .put(
            "c",
            &Document::new("a"),
            Blob::from_reader(&b"newer"[..]),
            Durability::Fsync,
        )
        .unwrap();
    writer.commit().unwrap();
    std::fs::copy(&path, &copy).unwrap();
    let mut reader = ReadOnlyBlobDatabase::open(&path, &options).unwrap();
    let seq = reader.database().seq();
    assert!(older.seq() < seq);

    let refreshed = reader.refresh(ReadOnlyDatabase::open(&copy, &options.registry).unwrap());
    assert!(
        matches!(&refreshed, Err(Error::RefreshedElsewhere { reopened: Some(p), .. }) if p.ends_with("copy.persia")),
        "{refreshed:?}"
    );
    let refreshed = reader.refresh(older);
    assert!(
        matches!(refreshed, Err(Error::RefreshedOlder { current, reopened, .. }) if current == seq && reopened < seq),
        "{refreshed:?}"
    );
    assert_eq!(reader.database().seq(), seq);
    assert!(
        reader
            .database()
            .container()
            .path()
            .unwrap()
            .ends_with("app.persia")
    );
    // The same database at the same seq, with the same replayed WAL, is fine.
    assert!(
        !reader
            .refresh(ReadOnlyDatabase::open(&path, &options.registry).unwrap())
            .unwrap()
    );
}

/// The same manifest `seq` with fewer replayed WAL records is an older open: nothing changes.
#[test]
fn a_refresh_with_fewer_replayed_wal_records_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let options = DatabaseOptions::default();
    let mut writer = create(&path);
    let earlier = ReadOnlyDatabase::open(&path, &options.registry).unwrap();
    writer
        .put(
            "c",
            &Document::new("a"),
            Blob::from_reader(&b"wal"[..]),
            Durability::Fsync,
        )
        .unwrap();
    let mut reader = ReadOnlyBlobDatabase::open(&path, &options).unwrap();
    assert_eq!(reader.database().seq(), earlier.seq());
    assert!(reader.database().replayed() > earlier.replayed());
    assert!(reader.database().container().end() > earlier.container().end());
    let replayed = reader.database().replayed();
    let end = reader.database().container().end();

    let refreshed = reader.refresh(earlier);
    assert!(
        matches!(
            refreshed,
            Err(Error::RefreshedOlder {
                current_replayed,
                reopened_replayed,
                ..
            }) if current_replayed == replayed && reopened_replayed < replayed
        ),
        "{refreshed:?}"
    );
    assert_eq!(reader.database().replayed(), replayed);
    assert_eq!(reader.database().container().end(), end);
}

/// A GC rewrite replaces the pack. Refresh reopens it and finds every blob, including one bound after the rewrite.
#[test]
fn a_refresh_reopens_a_pack_a_gc_rewrite_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let options = DatabaseOptions::default();
    let mut writer = create(&path);
    let kept = writer
        .put(
            "c",
            &Document::new("kept"),
            Blob::from_reader(&b"kept"[..]),
            Durability::Fsync,
        )
        .unwrap();
    writer.commit().unwrap();
    let mut reader = ReadOnlyBlobDatabase::open(&path, &options).unwrap();
    let report = writer
        .gc(&GcOptions {
            rewrite_ratio: 0.0,
            ..GcOptions::default()
        })
        .unwrap();
    assert!(
        matches!(report.sweep.rewrite, Some(Rewrite::Done { .. })),
        "{report:?}"
    );
    let after = writer
        .put(
            "c",
            &Document::new("after"),
            Blob::from_reader(&b"after"[..]),
            Durability::Fsync,
        )
        .unwrap();
    writer.commit().unwrap();

    assert!(
        reader
            .refresh(ReadOnlyDatabase::open(&path, &options.registry).unwrap())
            .unwrap()
    );
    assert_eq!(read_all(&reader, &kept), b"kept");
    assert_eq!(read_all(&reader, &after), b"after");
}

/// A pack error during refresh restores the documents, so they stay paired with the pack this reader opened.
#[cfg(unix)]
#[test]
fn a_pack_error_on_refresh_leaves_the_reader_paired() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let options = DatabaseOptions::default();
    let mut writer = create(&path);
    let blob = writer
        .put(
            "c",
            &Document::new("a"),
            Blob::from_reader(&b"kept"[..]),
            Durability::Fsync,
        )
        .unwrap();
    writer.commit().unwrap();
    let mut reader = ReadOnlyBlobDatabase::open(&path, &options).unwrap();
    let seq = reader.database().seq();
    let replayed = reader.database().replayed();
    writer
        .put(
            "c",
            &Document::new("b"),
            Blob::from_reader(&b"newer"[..]),
            Durability::Fsync,
        )
        .unwrap();
    writer.commit().unwrap();
    let newer = ReadOnlyDatabase::open(&path, &options.registry).unwrap();
    assert!(newer.seq() > seq);
    // A new file at the pack's path. The reader's descriptor keeps the pack it opened (Unix).
    let pack = pack_path(&path);
    std::fs::remove_file(&pack).unwrap();
    std::fs::write(&pack, b"not a pack").unwrap();

    let refreshed = reader.refresh(newer);
    assert!(refreshed.is_err(), "{refreshed:?}");
    assert_eq!(reader.database().seq(), seq);
    assert_eq!(reader.database().replayed(), replayed);
    assert_eq!(read_all(&reader, &blob), b"kept");
}
