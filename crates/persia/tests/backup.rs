//! `Db::backup` (SPEC §4.8; ROADMAP 10.1.8; ADR-0067): both files, at one commit, with the writer open and not
//! blocked for the copy, and no compaction or GC replacing a file meanwhile.
//!
//! Invariants:
//! 1. A backup opens at exactly the commit it took (the `seq` the hook reports), with every document of that commit
//!    and none written after, and `verify` passes on it, re-hashing every blob (`Scan::Deep`): no dangling reference.
//! 2. While it copies, writes, commits and reads go on; a blob GC marks and sweeps nothing and compaction is held;
//!    both run again once it returns.
//! 3. A database that never stored a blob backs up to one file.
//! 4. Nothing is ever replaced: a destination that exists, or appears before the copy is written, fails the backup
//!    with `AlreadyExists`, and the backup leaves nothing of its own behind.
//! 5. A read-only database backs up the state it shows.
//!
//! The crash points of the writing itself (every mutating call, every crash loss) are tested on `SimFs` in
//! `persia-storage/tests/container_image.rs`; this file uses real files. Steps are ordered by the backup hook, which
//! holds the backup with the writer mutex released; no test sleeps.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)] // Tests: inputs are fixed.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use persia::internal::documents::ReadOnlyBlobDatabase;
use persia::internal::gc::{GcOptions, GcReport};
use persia::internal::verify::Scan;
use persia::{
    BackupHook, BackupStep, Blob, BlobRef, Collection, Db, Document, ErrorCode, OpenOptions,
};
use persia_engine::db::DatabaseOptions;

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn files(db: &Db) -> Collection {
    db.create_collection_json(r#"{"name": "files", "fields": {"title": {"type": "text"}}}"#)
        .unwrap()
}

fn put(c: &Collection, id: &str, seed: u8) -> BlobRef {
    let data = bytes(5000 + usize::from(seed), seed);
    c.put_with_blob(&Document::new(id), Blob::from_reader(&data[..]))
        .unwrap()
}

fn read_blob(db: &Db, blob_ref: &BlobRef) -> Vec<u8> {
    let mut out = Vec::new();
    db.get_blob(blob_ref)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

/// `verify` on the copy at `path`: the engine's checks and every blob re-hashed.
fn verify(path: &Path) {
    let copy = ReadOnlyBlobDatabase::open(path, &DatabaseOptions::default()).unwrap();
    copy.database().verify().unwrap();
    let report = copy.verify_blobs(Scan::Deep).unwrap();
    assert!(report.pack || report.bound == 0, "{report:?}");
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn gc(db: &Db) -> GcReport {
    db.with_writer(|w| {
        w.gc(&GcOptions {
            rewrite_ratio: 0.0,
            ..GcOptions::default()
        })
        .unwrap()
    })
    .unwrap()
}

fn compaction_held(db: &Db) -> bool {
    db.with_writer(|w| w.log().compaction_held()).unwrap()
}

/// Holds the backup at `BackupStep::Taken`: reports its `seq`, then waits for a release.
struct Gate {
    taken: Receiver<u64>,
    release: Sender<()>,
}

impl Gate {
    fn new() -> (Self, BackupHook) {
        let (taken_tx, taken) = channel();
        let (release, released) = channel::<()>();
        let taken_tx = Mutex::new(taken_tx);
        let released = Mutex::new(released);
        let hook = BackupHook(Arc::new(move |step| {
            let BackupStep::Taken { seq } = step else {
                return;
            };
            taken_tx.lock().unwrap().send(seq).unwrap();
            released.lock().unwrap().recv().unwrap();
        }));
        (Self { taken, release }, hook)
    }
}

#[test]
fn a_database_without_a_pack_backs_up_to_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let notes = db
        .create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)
        .unwrap();
    notes.put_json(r#"{"id": "n1", "body": "hello"}"#).unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    // A `file://` URL names the destination as `Db::open` reads one.
    let dest = out.join("copy.persia");
    db.backup(format!("file://{}", dest.display())).unwrap();
    assert_eq!(names(&out), ["copy.persia"]);
    verify(&dest);
    let copy = OpenOptions::new().read_only(true).open(&dest).unwrap();
    assert_eq!(copy.committed_seq().unwrap(), db.committed_seq().unwrap());
    let hit = copy.collection("notes").unwrap().get_json("n1").unwrap();
    assert_eq!(hit.as_deref(), Some(r#"{"id":"n1","body":"hello"}"#));
}

#[test]
fn a_backup_takes_both_files_and_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = files(&db);
    let a = put(&c, "a", 1);
    let b = put(&c, "b", 2);
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let dest = out.join("copy.persia");
    db.backup(&dest).unwrap();
    assert_eq!(names(&out), ["copy.persia", "copy.persia.blobs"]);
    verify(&dest);
    let copy = Db::open(&dest).unwrap();
    assert_eq!(read_blob(&copy, &a), read_blob(&db, &a));
    assert_eq!(read_blob(&copy, &b), bytes(5002, 2));
    // The copy is a database of its own: it writes on.
    let more = copy.collection("files").unwrap();
    put(&more, "c", 3);
    copy.close().unwrap();
    db.close().unwrap();
}

/// Invariants 1 and 2: the [T] of ROADMAP 10.1.8.
#[test]
fn a_backup_taken_during_writes_and_a_gc_is_one_commit_with_no_dangling_reference() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, hook) = Gate::new();
    let db = Arc::new(
        OpenOptions::new()
            .hold_maintenance(true)
            .backup_hook(hook)
            .open(dir.path().join("app.persia"))
            .unwrap(),
    );
    let c = files(&db);
    let a1 = put(&c, "a1", 1);
    let a2 = put(&c, "a2", 2);
    c.delete("a2").unwrap(); // a2's blob is garbage from here on
    let a3 = put(&c, "a3", 3);
    let dest = dir.path().join("backup.persia");

    let backing_up = {
        let db = Arc::clone(&db);
        let dest = dest.clone();
        std::thread::spawn(move || db.backup(&dest))
    };
    let seq = gate.taken.recv().unwrap();
    // The backup holds its images, not the writer: writes, commits and reads go on.
    c.delete("a1").unwrap();
    let a4 = put(&c, "a4", 4);
    db.commit().unwrap();
    db.maintain_now().unwrap();
    assert!(db.committed_seq().unwrap() > seq);
    assert!(c.get("a3").unwrap().is_some());
    // No GC and no compaction replaces a file meanwhile.
    let held = gc(&db);
    assert!(held.held_by_copy, "{held:?}");
    assert_eq!(held.sweep.blobs_deleted, 0);
    assert!(compaction_held(&db));
    gate.release.send(()).unwrap();
    backing_up.join().unwrap().unwrap();

    // Released: GC sweeps a1's and a2's blobs, compaction is free again.
    assert!(!compaction_held(&db));
    let swept = gc(&db);
    assert!(!swept.held_by_copy);
    assert_eq!(swept.sweep.blobs_deleted, 2, "{swept:?}");

    // The copy: exactly the commit it took, and every blob that commit references.
    verify(&dest);
    let copy = OpenOptions::new().read_only(true).open(&dest).unwrap();
    assert_eq!(copy.committed_seq().unwrap(), seq);
    let copied = copy.collection("files").unwrap();
    for (id, blob_ref, seed) in [("a1", &a1, 1), ("a3", &a3, 3)] {
        let doc = copied.get(id).unwrap().unwrap();
        assert_eq!(doc.blob_ref(), Some(blob_ref));
        assert_eq!(
            read_blob(&copy, blob_ref),
            bytes(5000 + usize::from(seed), seed)
        );
    }
    assert!(
        copied.get("a2").unwrap().is_none(),
        "deleted before the backup"
    );
    assert!(
        copied.get("a4").unwrap().is_none(),
        "written after the images"
    );
    // The original GC'd a1's blob; the copy kept it (it was referenced at its commit).
    assert_eq!(db.get_blob(&a1).unwrap_err().code(), ErrorCode::NotFound);
    assert!(c.get("a4").unwrap().is_some());
    drop(a2);
    drop(a4);
    Arc::into_inner(db).unwrap().close().unwrap();
}

/// Invariant 4.
#[test]
fn nothing_is_ever_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let db = Db::open(&path).unwrap();
    let c = files(&db);
    put(&c, "a", 1);
    let seq = db.committed_seq().unwrap();
    for (taken, name) in [
        ("copy.persia", "copy.persia"),
        ("copy.persia", "copy.persia.blobs"),
    ] {
        let out = dir.path().join(format!("out-{name}"));
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join(name), b"mine").unwrap();
        let err = db.backup(out.join(taken)).unwrap_err();
        assert_eq!(err.code(), ErrorCode::AlreadyExists, "{err}");
        assert_eq!(names(&out), [name]);
        assert_eq!(std::fs::read(out.join(name)).unwrap(), b"mine");
    }
    // Refused before anything was done: not even a commit.
    assert_eq!(db.committed_seq().unwrap(), seq);
    // The database itself is a name that exists.
    assert_eq!(
        db.backup(&path).unwrap_err().code(),
        ErrorCode::AlreadyExists
    );
    // A directory that does not exist is not created.
    let missing = dir.path().join("missing").join("copy.persia");
    assert_eq!(db.backup(&missing).unwrap_err().code(), ErrorCode::Io);
    assert!(!dir.path().join("missing").exists());
}

#[test]
fn a_destination_that_appears_during_the_backup_fails_it_and_nothing_is_left() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let dest = out.join("copy.persia");
    let racer = dest.clone();
    let hook = BackupHook(Arc::new(move |_| {
        std::fs::write(&racer, b"theirs").unwrap();
    }));
    let db = OpenOptions::new()
        .backup_hook(hook)
        .open(dir.path().join("app.persia"))
        .unwrap();
    let c = files(&db);
    put(&c, "a", 1);
    let err = db.backup(&dest).unwrap_err();
    assert_eq!(err.code(), ErrorCode::AlreadyExists, "{err}");
    assert_eq!(names(&out), ["copy.persia"], "no pack, no temporary file");
    assert_eq!(std::fs::read(&dest).unwrap(), b"theirs");
    // The holds went with the failed backup.
    assert!(!compaction_held(&db));
    assert!(!gc(&db).held_by_copy);
}

/// Invariant 5.
#[test]
fn a_read_only_database_backs_up_the_state_it_shows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = Db::open(&path).unwrap();
    let c = files(&writer);
    let a = put(&c, "a", 1);
    writer.commit().unwrap();
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    let shown = reader.committed_seq().unwrap();
    put(&c, "b", 2);
    writer.commit().unwrap();

    let dest = dir.path().join("copy.persia");
    reader.backup(&dest).unwrap();
    verify(&dest);
    let copy = OpenOptions::new().read_only(true).open(&dest).unwrap();
    assert_eq!(copy.committed_seq().unwrap(), shown);
    let copied = copy.collection("files").unwrap();
    assert!(copied.get("a").unwrap().is_some());
    assert!(
        copied.get("b").unwrap().is_none(),
        "the reader had not refreshed"
    );
    assert_eq!(read_blob(&copy, &a), bytes(5001, 1));

    // After a refresh, the reader's backup has the newer commit.
    reader.refresh().unwrap();
    let again = dir.path().join("again.persia");
    reader.backup(&again).unwrap();
    verify(&again);
    let copy = OpenOptions::new().read_only(true).open(&again).unwrap();
    assert!(
        copy.collection("files")
            .unwrap()
            .get("b")
            .unwrap()
            .is_some()
    );
}

/// A backup of a database that holds a WAL tail after its last commit (writes not yet committed) commits them first:
/// every write that returned is in it.
#[test]
fn a_backup_holds_every_write_that_returned() {
    let dir = tempfile::tempdir().unwrap();
    let db = OpenOptions::new()
        .hold_maintenance(true)
        .open(dir.path().join("app.persia"))
        .unwrap();
    let c = files(&db);
    for i in 0..5 {
        c.put(&Document::new(format!("d{i}"))).unwrap();
    }
    let dest: PathBuf = dir.path().join("copy.persia");
    db.backup(&dest).unwrap();
    let copy = OpenOptions::new().read_only(true).open(&dest).unwrap();
    let copied = copy.collection("files").unwrap();
    for i in 0..5 {
        assert!(copied.get(&format!("d{i}")).unwrap().is_some());
    }
    assert_eq!(copy.committed_seq().unwrap(), db.committed_seq().unwrap());
}

/// A dangling symbolic link at `dest` is a name that exists: refused before the commit, the link left alone.
#[cfg(unix)]
#[test]
fn a_dangling_symbolic_link_at_dest_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    put(&files(&db), "a", 1);
    let seq = db.committed_seq().unwrap();
    for name in ["copy.persia", "copy.persia.blobs"] {
        let out = dir.path().join(format!("out-{name}"));
        std::fs::create_dir(&out).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), out.join(name)).unwrap();
        let err = db.backup(out.join("copy.persia")).unwrap_err();
        assert_eq!(err.code(), ErrorCode::AlreadyExists, "{err}");
        assert_eq!(names(&out), [name]);
        assert!(
            !dir.path().join("nowhere").exists(),
            "nothing written through the link"
        );
    }
    assert_eq!(
        db.committed_seq().unwrap(),
        seq,
        "refused before the commit"
    );
}

/// A writer a panic poisoned cannot commit: its backup fails with `Poisoned` and writes nothing.
#[test]
fn a_poisoned_writer_cannot_back_up() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = files(&db);
    put(&c, "a", 1);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        c.hold_writer_while(|| panic!("a write that panics"))
            .unwrap();
    }));
    assert!(panicked.is_err());
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let err = db.backup(out.join("copy.persia")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Poisoned, "{err}");
    assert_eq!(names(&out), Vec::<String>::new());
}

/// Where locks belong to the process (NFS-style, ADR-0019), closing any descriptor of the container drops its writer
/// lock: `Db::backup`'s steps, run on a `SimFs` (a `Db` opens real files only), leave both writer locks held.
#[test]
fn with_per_process_locks_a_backup_keeps_both_writer_locks() {
    use persia::internal::documents::BlobDatabase;
    use persia_blob::{PackStore, pack_path};
    use persia_engine::db::{Database, write_backup_in};
    use persia_engine::wal::Durability;
    use persia_storage::container::sim::SimFile;
    use persia_storage::container::{Container, Error as ContainerError, Mode};
    use persia_storage::vfs::Vfs;
    use persia_storage::vfs::sim::{SimFs, SimFsFile};

    const DB: &str = "/db/app.persia";
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
    let log = Database::create_container(
        Container::create_in(Arc::clone(&vfs), DB).unwrap(),
        DatabaseOptions::default(),
    )
    .unwrap();
    let mut db =
        BlobDatabase::with_pack_in(log, Arc::clone(&vfs), pack_path(Path::new(DB))).unwrap();
    db.log_mut()
        .create_collection(persia::SchemaBuilder::new("files").build().unwrap())
        .unwrap();
    db.put(
        "files",
        &Document::new("a"),
        Blob::from_reader(&bytes(3000, 1)[..]),
        Durability::Fsync,
    )
    .unwrap();
    let db_uuid = db.log().container().header().db_uuid;

    fs.set_per_process_locks(true);
    for dest in ["/db/one.persia", "/db/two.persia"] {
        db.commit().unwrap();
        let _compaction = db.log().hold_compaction();
        let _gc = db.hold_gc();
        let container = db.log().committed_image().unwrap();
        let pack = db.blobs().store().committed_image().unwrap().unwrap();
        let pack_dest = pack_path(Path::new(dest));
        write_backup_in(
            &fs,
            &[(pack_dest.as_path(), &pack), (Path::new(dest), &container)],
        )
        .unwrap();
    }
    fs.set_per_process_locks(false);

    let err = Container::open_in(Arc::clone(&vfs), DB, Mode::ReadWrite).unwrap_err();
    assert!(matches!(err, ContainerError::Locked), "{err:?}");
    let other = Container::create_on(SimFile::new(), db_uuid).unwrap();
    let err = PackStore::open_writer_in(
        Arc::clone(&vfs),
        pack_path(Path::new(DB)),
        &other.writer_proof().unwrap(),
    )
    .unwrap_err();
    assert!(matches!(err, persia_blob::Error::PackLocked), "{err:?}");
    // Both released with the database.
    drop(db);
    drop(Container::open_in(vfs, DB, Mode::ReadWrite).unwrap());
}

/// A destination that cannot be looked up (under a regular file) fails before the commit.
#[cfg(unix)]
#[test]
fn a_destination_that_cannot_be_looked_up_fails_before_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let db = Db::open(&path).unwrap();
    put(&files(&db), "a", 1);
    let seq = db.committed_seq().unwrap();
    let err = db.backup(path.join("copy.persia")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Io, "{err}");
    assert_eq!(
        db.committed_seq().unwrap(),
        seq,
        "refused before the commit"
    );
}
