//! One pack writer (SPEC §9.3; ROADMAP 9.2.7; ADR-0049 item 8, ADR-0053): the pack opens read-write only with the
//! container's `WriterProof`, which only a read-write container handle mints.
//!
//! Invariants:
//! 1. A read-only container mints no proof; a read-write one mints a proof naming its `db_uuid`.
//! 2. A pack opened with a proof carries that proof's `db_uuid`: it creates the pack with it, and a pack of another
//!    database fails to open, read-write or read-only.
//! 3. A pack opened without a proof is read-only: it refuses every write and never creates the file.
//! 4. Two writers are impossible: a second read-write open of the container fails on its lock, so no second proof (and
//!    no second read-write pack) exists until the first container is closed.
//! 5. The pack's own writer lock, `<name>.persia.blobs.lock` beside it on every platform, allows one read-write
//!    handle whatever the proof, from the open (with or without a pack file) until the handle is dropped (ROADMAP
//!    9.2.15).
#![allow(clippy::unwrap_used, clippy::panic, missing_docs)] // Tests: inputs are fixed.

mod pack_support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_blob::{
    BindOutcome, BlobManifest, BlobStore, ChunkSize, Error, PackMode, PackStore, UploadId,
    blob_id_of, pack_path,
};
use persia_format::checksum::crc32c;
use persia_storage::container::sim::SimFile;
use persia_storage::container::{Container, ContainerFile, Error as ContainerError, Mode};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};

const DB: &str = "/db/app.persia";

fn fs() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    fs
}

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

/// Stages and binds `data` as one chunk.
fn store_blob(store: &PackStore<SimFsFile>, data: &[u8], upload: u128) -> BlobManifest {
    let m = BlobManifest::new(
        blob_id_of(data),
        u64::try_from(data.len()).unwrap(),
        ChunkSize::new(ChunkSize::MIN).unwrap(),
        UploadId::new(upload),
        vec![crc32c(data)],
    )
    .unwrap();
    store.stage_chunk(m.upload_id(), 0, data).unwrap();
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    m
}

#[test]
fn only_a_read_write_container_mints_a_proof() {
    let file = SimFile::new();
    let writer = Container::create_on(file.clone(), [3; 16]).unwrap();
    let proof = writer
        .writer_proof()
        .expect("a read-write container is the writer");
    assert_eq!(proof.db_uuid(), [3; 16]);
    let reader = Container::open_on(file.clone(), Mode::ReadOnly).unwrap();
    assert!(reader.writer_proof().is_none());
    drop((writer, reader));
    // Reopened read-write, it is the writer again.
    let writer = Container::open_on(file, Mode::ReadWrite).unwrap();
    assert_eq!(writer.writer_proof().unwrap().db_uuid(), [3; 16]);
}

#[test]
fn the_pack_carries_the_proofs_db_uuid() {
    let fs = fs();
    let container = Container::create_in(vfs(&fs), DB).unwrap();
    let db_uuid = container.header().db_uuid;
    let path = pack_path(Path::new(DB));
    let store =
        PackStore::open_writer_in(vfs(&fs), &path, &container.writer_proof().unwrap()).unwrap();
    assert_eq!(store.mode(), PackMode::ReadWrite);
    let m = store_blob(&store, b"carried", 1);
    assert_eq!(store.header().unwrap().db_uuid, db_uuid);
    drop(store);

    // Read-only with the container's id: the blob is there.
    let reader = PackStore::open_in(vfs(&fs), &path, db_uuid).unwrap();
    assert_eq!(reader.mode(), PackMode::ReadOnly);
    assert!(reader.manifest(m.blob_id()).unwrap().is_some());
    // Another database's id fails, read-only or with another container's proof.
    let other = [db_uuid[0] ^ 1; 16];
    assert!(matches!(
        PackStore::open_in(vfs(&fs), &path, other),
        Err(Error::PackOfAnotherDatabase { found }) if found == db_uuid
    ));
    let foreign = Container::create_on(SimFile::new(), other).unwrap();
    assert!(matches!(
        PackStore::open_writer_in(vfs(&fs), &path, &foreign.writer_proof().unwrap()),
        Err(Error::PackOfAnotherDatabase { found }) if found == db_uuid
    ));
}

#[test]
fn a_pack_opened_without_a_proof_never_writes() {
    let fs = fs();
    let path = pack_path(Path::new(DB));
    let reader = PackStore::open_in(vfs(&fs), &path, [5; 16]).unwrap();
    assert_eq!(reader.mode(), PackMode::ReadOnly);
    assert!(matches!(
        reader.stage_chunk(UploadId::new(1), 0, b"x"),
        Err(Error::PackReadOnly)
    ));
    assert!(!reader.exists());
    assert!(fs.read(&path).is_none(), "a read-only open creates no pack");
}

#[test]
fn a_second_writer_fails_on_the_containers_lock() {
    let fs = fs();
    let container = Container::create_in(vfs(&fs), DB).unwrap();
    let path = pack_path(Path::new(DB));
    let store =
        PackStore::open_writer_in(vfs(&fs), &path, &container.writer_proof().unwrap()).unwrap();
    store_blob(&store, b"first writer", 1);
    // No second read-write container, so no second proof and no second read-write pack.
    assert!(matches!(
        Container::open_in(vfs(&fs), DB, Mode::ReadWrite),
        Err(ContainerError::Locked)
    ));
    // A reader opens beside the writer, and mints nothing.
    let reader = Container::open_in(vfs(&fs), DB, Mode::ReadOnly).unwrap();
    assert!(reader.writer_proof().is_none());
    drop((store, container));
    // Once the writer is closed, the next one opens and keeps the blobs.
    let next = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    let store = PackStore::open_writer_in(vfs(&fs), &path, &next.writer_proof().unwrap()).unwrap();
    assert_eq!(store.blob_ids(), [blob_id_of(b"first writer")]);
}

/// The same on the operating system's file system, whose lock is `flock` (or `<name>.lock` on Windows).
#[test]
fn a_second_writer_fails_on_a_real_containers_lock() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("app.persia");
    let container = Container::create(&db).unwrap();
    let store = PackStore::open_writer(pack_path(&db), &container.writer_proof().unwrap()).unwrap();
    assert_eq!(store.mode(), PackMode::ReadWrite);
    assert!(matches!(
        Container::open(&db, Mode::ReadWrite),
        Err(ContainerError::Locked)
    ));
    let reader = Container::open(&db, Mode::ReadOnly).unwrap();
    assert!(reader.writer_proof().is_none());
    let read_only = PackStore::open(pack_path(&db), reader.header().db_uuid).unwrap();
    assert_eq!(read_only.mode(), PackMode::ReadOnly);
}

/// The proof is the gate, the pack's writer lock the enforcement (ROADMAP 9.2.7): the same proof cannot open a second
/// read-write handle, and a handle an `Arc` clone keeps alive after its container closed keeps the lock with it.
#[test]
fn the_pack_lock_allows_one_writer_whatever_the_proof() {
    let fs = fs();
    let path = pack_path(Path::new(DB));
    let container = Container::create_in(vfs(&fs), DB).unwrap();
    let proof = container.writer_proof().unwrap();
    let first = PackStore::open_writer_in(vfs(&fs), &path, &proof).unwrap();
    store_blob(&first, b"locked", 1);
    for _ in 0..2 {
        assert!(matches!(
            PackStore::open_writer_in(vfs(&fs), &path, &proof),
            Err(Error::PackLocked)
        ));
    }
    // A reader is never locked out.
    assert_eq!(
        PackStore::open_in(vfs(&fs), &path, proof.db_uuid())
            .unwrap()
            .blob_ids()
            .len(),
        1
    );

    let first = Arc::new(first);
    let kept = Arc::clone(&first);
    drop((first, container));
    // The container's lock is free, the pack's is not.
    let next = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    assert!(matches!(
        PackStore::open_writer_in(vfs(&fs), &path, &next.writer_proof().unwrap()),
        Err(Error::PackLocked)
    ));
    drop(kept);
    let store = PackStore::open_writer_in(vfs(&fs), &path, &next.writer_proof().unwrap()).unwrap();
    assert_eq!(store.blob_ids(), [blob_id_of(b"locked")]);
}

/// The same on the operating system's file system: `<name>.persia.blobs.lock`, locked with `flock` on Unix and
/// `LockFileEx` on Windows (ROADMAP 9.2.15).
#[test]
fn the_pack_lock_allows_one_writer_on_a_real_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("app.persia");
    let container = Container::create(&db).unwrap();
    let proof = container.writer_proof().unwrap();
    let first = PackStore::open_writer(pack_path(&db), &proof).unwrap();
    let m = BlobManifest::new(
        blob_id_of(b"real"),
        4,
        ChunkSize::new(ChunkSize::MIN).unwrap(),
        UploadId::new(1),
        vec![crc32c(b"real")],
    )
    .unwrap();
    first.stage_chunk(m.upload_id(), 0, b"real").unwrap();
    assert_eq!(first.bind(&m).unwrap(), BindOutcome::Bound);
    assert!(matches!(
        PackStore::open_writer(pack_path(&db), &proof),
        Err(Error::PackLocked)
    ));
    drop(first);
    let again = PackStore::open_writer(pack_path(&db), &proof).unwrap();
    assert_eq!(again.blob_ids(), [m.blob_id()]);
}

const LOCK: &str = "/db/app.persia.blobs.lock";

/// ROADMAP 9.2.15: the lock is beside the pack, so it exists before the pack does. A second read-write handle opened
/// while there is no pack file fails with `PackLocked`, and the first creates the pack and binds as usual; the lock is
/// released when that handle is dropped, never before, and the lock file stays.
#[test]
fn a_second_writer_fails_on_the_pack_lock_before_the_pack_exists() {
    let fs = fs();
    let path = pack_path(Path::new(DB));
    let container = Container::create_in(vfs(&fs), DB).unwrap();
    let proof = container.writer_proof().unwrap();
    let first = PackStore::open_writer_in(vfs(&fs), &path, &proof).unwrap();
    assert!(!first.exists());
    assert!(fs.read(&path).is_none(), "no pack file yet");
    assert!(fs.read(Path::new(LOCK)).is_some(), "the lock file exists");
    assert!(matches!(
        PackStore::open_writer_in(vfs(&fs), &path, &proof),
        Err(Error::PackLocked)
    ));
    // The lock is the file's, whoever asks for it.
    let other = fs.open(Path::new(LOCK), true).unwrap();
    assert!(!other.try_lock().unwrap(), "held by the first writer");
    store_blob(&first, b"first writer", 1);
    assert!(matches!(
        PackStore::open_writer_in(vfs(&fs), &path, &proof),
        Err(Error::PackLocked)
    ));
    drop(first);
    assert!(
        fs.read(Path::new(LOCK)).is_some(),
        "the lock file is never removed"
    );
    assert!(other.try_lock().unwrap(), "released on drop");
    other.unlock().unwrap();
    let again = PackStore::open_writer_in(vfs(&fs), &path, &proof).unwrap();
    assert_eq!(again.blob_ids(), [blob_id_of(b"first writer")]);
}

/// A failed read-write open releases the lock at once: a pack of another database, or a damaged one, leaves no lock
/// behind for the next writer.
#[test]
fn a_failed_writer_open_releases_the_pack_lock() {
    let fs = fs();
    let path = pack_path(Path::new(DB));
    let container = Container::create_in(vfs(&fs), DB).unwrap();
    let proof = container.writer_proof().unwrap();
    let f = fs.create_new(&path).unwrap();
    f.write_all_at(&[0xaa; 12288], 0).unwrap();
    assert!(
        PackStore::open_writer_in(vfs(&fs), &path, &proof)
            .unwrap_err()
            .is_corruption()
    );
    let lock = fs.open(Path::new(LOCK), true).unwrap();
    assert!(lock.try_lock().unwrap(), "released after the failed open");
}

/// A read-only open neither takes nor creates the lock.
#[test]
fn a_reader_takes_no_pack_lock() {
    let fs = fs();
    let path = pack_path(Path::new(DB));
    let reader = PackStore::open_in(vfs(&fs), &path, [5; 16]).unwrap();
    assert!(fs.read(Path::new(LOCK)).is_none());
    drop(reader);
    let container = Container::create_in(vfs(&fs), DB).unwrap();
    let writer = PackStore::open_writer_in(vfs(&fs), &path, &container.writer_proof().unwrap());
    let _reader = PackStore::open_in(vfs(&fs), &path, container.header().db_uuid).unwrap();
    assert!(writer.is_ok());
}

/// ROADMAP 9.2.15 with an `Arc` of the store that outlives its container, before any pack file exists: a new
/// read-write container of the same file mints a proof, and its pack open still fails until the old store is dropped.
#[test]
fn an_arc_of_the_store_keeps_the_lock_after_its_container_closes_with_no_pack() {
    let fs = fs();
    let path = pack_path(Path::new(DB));
    let container = Container::create_in(vfs(&fs), DB).unwrap();
    let first = Arc::new(
        PackStore::open_writer_in(vfs(&fs), &path, &container.writer_proof().unwrap()).unwrap(),
    );
    let kept = Arc::clone(&first);
    drop((first, container));
    let next = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    assert!(matches!(
        PackStore::open_writer_in(vfs(&fs), &path, &next.writer_proof().unwrap()),
        Err(Error::PackLocked)
    ));
    assert!(!kept.exists(), "still no pack file");
    drop(kept);
    let store = PackStore::open_writer_in(vfs(&fs), &path, &next.writer_proof().unwrap()).unwrap();
    store_blob(&store, b"after the arc", 1);
}

/// The same on the operating system's file system, with no pack file: two writers cannot both live, and the lock file
/// is `<name>.persia.blobs.lock` beside the pack.
#[test]
fn two_writers_with_no_pack_file_on_a_real_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("app.persia");
    let container = Container::create(&db).unwrap();
    let proof = container.writer_proof().unwrap();
    let first = PackStore::open_writer(pack_path(&db), &proof).unwrap();
    assert!(!first.exists());
    assert!(dir.path().join("app.persia.blobs.lock").is_file());
    assert!(matches!(
        PackStore::open_writer(pack_path(&db), &proof),
        Err(Error::PackLocked)
    ));
    drop(first);
    assert!(PackStore::open_writer(pack_path(&db), &proof).is_ok());
}

/// ROADMAP 9.2.10 with a real pack writer: the container's sweep sees the lock a live pack handle holds (one an `Arc`
/// kept after its container closed) and keeps the pack's temporary files; once the handle drops, it removes them.
#[test]
fn the_container_sweep_keeps_the_packs_temporary_files_while_its_writer_lives() {
    let fs = fs();
    let path = pack_path(Path::new(DB));
    let container = Container::create_in(vfs(&fs), DB).unwrap();
    let store = Arc::new(
        PackStore::open_writer_in(vfs(&fs), &path, &container.writer_proof().unwrap()).unwrap(),
    );
    let stale = "/db/app.persia.blobs.tmp.0123456789abcdef.3";
    fs.create_new(Path::new(stale)).unwrap();
    let kept = Arc::clone(&store);
    drop((store, container));
    let next = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    assert_eq!(next.sweep_stale_files().unwrap(), Vec::<PathBuf>::new());
    assert!(fs.read(Path::new(stale)).is_some());
    drop(kept);
    assert_eq!(next.sweep_stale_files().unwrap(), [PathBuf::from(stale)]);
}
