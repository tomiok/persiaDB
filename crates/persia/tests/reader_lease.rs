//! Reader leases with documents, on real files (SPEC §9.3, §9.5; ROADMAP 9.2.16; ADR-0054).
//!
//! Invariants:
//! 1. While a read-only handle holds documents from before a delete, the writer's GC is held back (nothing swept, no
//!    rewrite) and the blob reads; once the handle refreshes to the committed seq the next GC sweeps it.
//! 2. A refresh writes the new seq into the record only once it succeeded; a refused refresh keeps the record.
//! 3. A reader lease ahead of the committed seq holds GC back.
//! 4. `with_lease` refuses documents older than the lease's taken seq (`OpenedBeforeLease`) and a lease of another
//!    pack's directory (`LeaseElsewhere`, a retargeted symbolic link included), and takes equal and newer documents.
//! 5. A reader process killed (SIGKILL) while holding its lease leaves a stale one: the next GC removes it and sweeps.
//! 6. A directory that refuses writes opens without a lease (`lease()` is `None`).
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::expect_used
)] // Tests: inputs are fixed.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use persia::internal::documents::{Blob, BlobDatabase, Error, ReadOnlyBlobDatabase, ReaderLease};
use persia::internal::gc::{GcOptions, GcReport};
use persia_blob::gc::Rewrite;
use persia_blob::pack::lease::{LeaseRecord, leases_dir};
use persia_blob::{BlobRef, PackStore, pack_path};
use persia_engine::db::{DatabaseOptions, ReadOnlyDatabase};
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

fn put(db: &mut BlobDatabase<PackStore>, id: &str, bytes: &'static [u8]) -> BlobRef {
    let r = db
        .put(
            "c",
            &Document::new(id),
            Blob::from_reader(bytes),
            Durability::Fsync,
        )
        .unwrap();
    db.commit().unwrap();
    r
}

fn delete(db: &mut BlobDatabase<PackStore>, id: &str) {
    db.log_mut().delete("c", id, Durability::Fsync).unwrap();
    db.release_durable();
    db.commit().unwrap();
}

fn gc(db: &BlobDatabase<PackStore>) -> GcReport {
    db.gc(&GcOptions {
        rewrite_ratio: 0.0,
        ..GcOptions::default()
    })
    .unwrap()
}

fn reopen(path: &Path) -> ReadOnlyDatabase {
    ReadOnlyDatabase::open(path, &DatabaseOptions::default().registry).unwrap()
}

fn record(reader: &ReadOnlyBlobDatabase) -> LeaseRecord {
    let path = reader.lease().unwrap().path().unwrap();
    LeaseRecord::decode(&std::fs::read(path).unwrap()).unwrap()
}

/// Whether the file system under `dir` lets the writer judge a lease by its lock (`locks_per_handle`, ADR-0054 item
/// 13). Where it does not, every lease holds GC back, and the GC tests here have nothing to show: they are skipped,
/// with the reason printed.
fn locks_judged(dir: &Path) -> bool {
    let probe = dir.join("locks-probe");
    let file = std::fs::File::create(&probe).unwrap();
    let judged = persia_blob::pack::ContainerFile::locks_per_handle(&file);
    drop(file);
    std::fs::remove_file(&probe).unwrap();
    if !judged {
        eprintln!(
            "skipped: locks cannot be judged on the file system under {}",
            dir.display()
        );
    }
    judged
}

fn leases(path: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(leases_dir(&pack_path(path))) {
        Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn a_reader_behind_a_delete_holds_gc_back_until_it_refreshes() {
    let dir = tempfile::tempdir().unwrap();
    if !locks_judged(dir.path()) {
        return;
    }
    let path = dir.path().join("app.persia");
    let mut writer = create(&path);
    let gone = put(&mut writer, "gone", b"swept later");
    let kept = put(&mut writer, "kept", b"kept");
    let mut reader = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default()).unwrap();
    assert_eq!(
        record(&reader).manifest_seq(),
        Some(reader.database().seq())
    );
    let held = reader.database().seq();

    delete(&mut writer, "gone");
    let report = gc(&writer);
    let readers = report.readers.clone().unwrap();
    assert!(readers.held_back, "{report:?}");
    assert_eq!(
        (readers.live, readers.behind, readers.oldest_seq),
        (1, 1, Some(held))
    );
    assert_eq!(report.sweep.blobs_deleted, 0, "{report:?}");
    assert_eq!(report.sweep.rewrite, None, "no rewrite");
    assert_eq!(reader.get_blob_range(&gone, 0..5).unwrap(), b"swept");

    reader.refresh(reopen(&path)).unwrap();
    let now = writer.log().seq();
    assert_eq!(
        record(&reader).manifest_seq(),
        Some(now),
        "the refresh advanced it"
    );

    let report = gc(&writer);
    let readers = report.readers.clone().unwrap();
    assert!(!readers.held_back && readers.current == 1, "{report:?}");
    assert_eq!(report.sweep.blobs_deleted, 1, "{report:?}");
    assert!(
        matches!(report.sweep.rewrite, Some(Rewrite::Done { .. })),
        "{report:?}"
    );
    assert_eq!(reader.get_blob_range(&kept, 0..4).unwrap(), b"kept");
}

#[test]
fn a_refused_refresh_keeps_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut writer = create(&path);
    put(&mut writer, "a", b"a");
    let older = reopen(&path);
    put(&mut writer, "b", b"b");
    let mut reader = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default()).unwrap();
    let before = record(&reader);
    assert!(matches!(
        reader.refresh(older),
        Err(Error::RefreshedOlder { .. })
    ));
    assert_eq!(record(&reader), before);
}

#[test]
fn a_lease_ahead_of_the_committed_seq_holds_gc_back() {
    let dir = tempfile::tempdir().unwrap();
    if !locks_judged(dir.path()) {
        return;
    }
    let path = dir.path().join("app.persia");
    let mut writer = create(&path);
    put(&mut writer, "a", b"a");
    let mut lease = ReaderLease::take_beside(&path).unwrap();
    let uuid = writer.log().container().header().db_uuid;
    lease.set_open(uuid, writer.log().seq() + 5);
    let report = gc(&writer);
    let readers = report.readers.unwrap();
    assert!(readers.held_back && readers.ahead == 1, "{readers:?}");
}

#[test]
fn with_lease_takes_equal_and_newer_documents_and_refuses_older_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut writer = create(&path);
    put(&mut writer, "a", b"a");

    // Opened before the lease, nothing committed since: equal, taken.
    let docs = reopen(&path);
    let lease = ReaderLease::take_beside(&path).unwrap();
    assert_eq!(lease.taken_seq(), docs.seq());
    let reader = ReadOnlyBlobDatabase::with_lease(lease, docs, &path).unwrap();
    assert!(reader.lease().is_some());
    drop(reader);

    // Opened after the lease and a commit: newer, taken.
    let lease = ReaderLease::take_beside(&path).unwrap();
    put(&mut writer, "b", b"b");
    let docs = reopen(&path);
    assert!(docs.seq() > lease.taken_seq());
    ReadOnlyBlobDatabase::with_lease(lease, docs, &path).unwrap();

    // Opened before the lease, a commit in between: refused.
    let docs = reopen(&path);
    put(&mut writer, "c", b"c");
    let lease = ReaderLease::take_beside(&path).unwrap();
    let (opened, taken) = (docs.seq(), lease.taken_seq());
    match ReadOnlyBlobDatabase::with_lease(lease, docs, &path) {
        Err(Error::OpenedBeforeLease {
            opened: o,
            taken: t,
        }) => assert_eq!((o, t), (opened, taken)),
        other => panic!("{other:?}"),
    }
    // `with_pack_beside` takes its lease after the documents, so it is refused the same way.
    let docs = reopen(&path);
    put(&mut writer, "d", b"d");
    assert!(matches!(
        ReadOnlyBlobDatabase::with_pack_beside(docs, &path),
        Err(Error::OpenedBeforeLease { .. })
    ));

    // A lease beside another database's pack.
    let other = dir.path().join("other.persia");
    drop(create(&other));
    let lease = ReaderLease::take_beside(&other).unwrap();
    assert!(matches!(
        ReadOnlyBlobDatabase::with_lease(lease, reopen(&path), &path),
        Err(Error::LeaseElsewhere { .. })
    ));
    drop(writer);
    assert!(
        leases(&path).is_empty(),
        "every refused handle dropped its lease"
    );
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_retargeted_after_the_lease_is_lease_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    // macOS puts the temp dir under `/var`, a symlink to `/private/var`. `take_beside` records the
    // canonical leases directory (`Container` canonicalizes the path), so this test works under the
    // canonical directory and compares the recorded path exactly.
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (a, b) = (root.join("a.persia"), root.join("b.persia"));
    drop(create(&a));
    drop(create(&b));
    let link = root.join("link.persia");
    std::os::unix::fs::symlink(&a, &link).unwrap();
    let lease = ReaderLease::take_beside(&link).unwrap();
    assert_eq!(lease.dir(), leases_dir(&pack_path(&a)));
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&b, &link).unwrap();
    assert!(matches!(
        ReadOnlyBlobDatabase::with_lease(lease, reopen(&link), &link),
        Err(Error::LeaseElsewhere { .. })
    ));
}

#[cfg(unix)]
#[test]
fn a_directory_that_refuses_writes_opens_without_a_lease() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("ro");
    std::fs::create_dir(&sub).unwrap();
    let path = sub.join("app.persia");
    drop(create(&path));
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root ignores directory permissions: nothing to check there.
    if std::fs::write(sub.join("probe"), b"").is_ok() {
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let reader = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default());
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let reader = reader.unwrap();
    assert!(reader.lease().is_none());
    assert!(!leases_dir(&pack_path(&path)).exists());
}

// -- 5. a reader process killed while holding its lease --

const ROLE: &str = "PERSIA_LEASE_CHILD";

/// The child: opens a read-only handle of the database at `ROLE`'s path, prints its lease file, then waits on stdin
/// until it is killed. In a normal run it does nothing.
#[test]
fn child_role() {
    let Ok(path) = std::env::var(ROLE) else {
        return;
    };
    let reader = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default()).unwrap();
    println!(
        "LEASED {}",
        reader.lease().unwrap().path().unwrap().display()
    );
    let mut line = String::new();
    // Until killed; a parent that died closes the pipe, and the child exits.
    std::io::stdin().read_to_string(&mut line).unwrap();
}

#[test]
fn a_reader_killed_while_holding_its_lease_leaves_a_stale_one_gc_removes() {
    let dir = tempfile::tempdir().unwrap();
    if !locks_judged(dir.path()) {
        return;
    }
    let path = dir.path().join("app.persia");
    let mut writer = create(&path);
    put(&mut writer, "gone", b"gone");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE, &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let lease = stdout
        .lines()
        .find_map(|l| {
            l.unwrap()
                .split_once("LEASED ")
                .map(|(_, p)| PathBuf::from(p))
        })
        .expect("the child printed its lease");
    delete(&mut writer, "gone");
    let report = gc(&writer);
    let readers = report.readers.unwrap();
    assert!(
        readers.held_back && readers.behind == 1,
        "the live child holds GC back: {readers:?}"
    );
    assert!(lease.exists());

    child.kill().unwrap();
    child.wait().unwrap();
    assert!(lease.exists(), "a killed reader never removes its lease");
    let report = gc(&writer);
    let readers = report.readers.clone().unwrap();
    assert_eq!((readers.stale_removed, readers.live), (1, 0), "{report:?}");
    assert!(!readers.held_back);
    assert_eq!(report.sweep.blobs_deleted, 1, "{report:?}");
    assert!(!lease.exists());
}

/// The reader lease opens the container first; its failure is reported as the documents' open reports it: a file that
/// is not a container, and a damaged one, are `Error::Database` corruption, and nothing is created beside them.
#[test]
fn an_open_of_a_file_that_is_not_a_container_or_a_damaged_one_is_database_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let garbage = dir.path().join("notes.persia");
    std::fs::write(&garbage, b"this is not a database").unwrap();
    let damaged = dir.path().join("app.persia");
    drop(create(&damaged));
    let mut bytes = std::fs::read(&damaged).unwrap();
    // The magic intact, a header byte flipped: the header's CRC no longer matches.
    bytes[20] ^= 0xFF;
    std::fs::write(&damaged, &bytes).unwrap();
    for path in [&garbage, &damaged] {
        let e = ReadOnlyBlobDatabase::open(path, &DatabaseOptions::default()).unwrap_err();
        assert!(
            matches!(e, Error::Database(_)) && e.is_corruption(),
            "{}: {e:?}",
            path.display()
        );
        assert!(!leases_dir(&pack_path(path)).exists(), "{}", path.display());
    }
}
