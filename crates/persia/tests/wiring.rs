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
//!    same `seq` with fewer replayed WAL records and an earlier WAL end, is `RefreshedOlder`. At the same `seq` the
//!    records and the WAL end move together: more of both, or the same, is taken (a commit in progress, which
//!    appends frames that are not WAL, included); counts and ends that disagree are `RefreshDiverged` (another
//!    history at the path). Neither changes anything.
//! 7. A GC rewrite (`PackReplaced`) reopens the pack; the reader then sees every blob, including one bound after
//!    the rewrite. Any other pack error, and a reopen that fails (a damaged replacement, a pack of another database),
//!    leaves the documents and the pack as they were: the same `seq`, records and WAL end, the old blobs readable.
//! 8. A reader whose documents are behind a delete holds GC back (its reader lease, ROADMAP 9.2.16): the blob stays
//!    until the reader refreshes past the delete, and the next GC sweeps it. A writer that ignores reader leases (one
//!    from before ADR-0054) leaves a pack newer than the documents: a document whose blob is gone reads as
//!    `BlobNotFound`, never a panic, and every other blob reads on. The reader lease itself is tested in
//!    `reader_lease.rs`.
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
use persia_storage::container::{Container, Error as ContainerError};

fn create(path: &Path) -> BlobDatabase<PackStore> {
    let mut db = BlobDatabase::create(path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    db
}

/// Runs `f` with the reader leases beside the pack of `path` moved aside: a GC then sees none, as a writer from
/// before ADR-0054 (which ignores them, item 16) would. For tests of what such a GC leaves a reader.
fn ignoring_reader_leases<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    let leases = persia_blob::pack::lease::leases_dir(&pack_path(path));
    let mut aside = leases.as_os_str().to_owned();
    aside.push(".aside");
    std::fs::rename(&leases, &aside).unwrap();
    let out = f();
    std::fs::rename(&aside, &leases).unwrap();
    out
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

/// The same manifest `seq` with fewer replayed WAL records and an earlier WAL end is an older open: nothing changes.
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
    assert!(reader.database().wal_end() > earlier.wal_end());
    let replayed = reader.database().replayed();
    let (seq, end) = (reader.database().seq(), reader.database().wal_end());

    let refreshed = reader.refresh(earlier);
    assert!(
        matches!(
            refreshed,
            Err(Error::RefreshedOlder {
                current_replayed,
                reopened_replayed,
                current_end,
                reopened_end,
                ..
            }) if current_replayed == replayed && reopened_replayed < replayed && current_end == end && reopened_end < end
        ),
        "{refreshed:?}"
    );
    assert_eq!(reader.database().replayed(), replayed);
    assert_eq!(reader.database().wal_end(), end);
    assert_eq!(reader.database().seq(), seq);
}

/// What a refresh must leave alone when it fails: the documents' `seq`, replayed WAL records and WAL end, and the
/// pack handle's `(generation, pack_id)`.
fn paired_state(reader: &ReadOnlyBlobDatabase) -> (u64, u64, u64, Option<(u64, u128)>) {
    let db = reader.database();
    let pack = reader.blobs().store().superblock();
    (
        db.seq(),
        db.replayed(),
        db.wal_end(),
        pack.map(|sb| (sb.generation, sb.pack_id)),
    )
}

fn has_document(reader: &ReadOnlyBlobDatabase, id: &str) -> bool {
    let snapshot = reader.database().snapshot();
    snapshot.collection("c").unwrap().get(id).unwrap().is_some()
}

fn not_found(reader: &ReadOnlyBlobDatabase, blob: &persia_blob::BlobRef) -> bool {
    matches!(
        reader.get_blob(blob),
        Err(Error::Blob(persia_blob::Error::BlobNotFound { blob_id })) if blob_id == blob.blob_id()
    )
}

/// Invariant 6: the same `seq` with more replayed WAL records and a later WAL end is taken; so is an open in the same
/// state (the return value reports only the pack), and one of a commit cut before its slot write (a commit in
/// progress), whose segment and Manifest frames move the container's end but not the WAL's.
#[test]
fn a_refresh_at_the_same_seq_takes_a_longer_wal_and_the_same_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let options = DatabaseOptions::default();
    let mut writer = create(&path);
    let mut reader = ReadOnlyBlobDatabase::open(&path, &options).unwrap();
    let (seq, replayed, end, _) = paired_state(&reader);
    writer
        .log_mut()
        .put("c", &Document::new("later"), Durability::Fsync)
        .unwrap();
    let longer = ReadOnlyDatabase::open(&path, &options.registry).unwrap();
    assert_eq!(longer.seq(), seq);
    assert!(longer.replayed() > replayed && longer.wal_end() > end);
    assert!(
        !reader.refresh(longer).unwrap(),
        "no pack: nothing new in it"
    );
    assert!(has_document(&reader, "later"));
    let (_, replayed, end, _) = paired_state(&reader);

    // The same state again: taken, and still nothing new in the pack.
    assert!(
        !reader
            .refresh(ReadOnlyDatabase::open(&path, &options.registry).unwrap())
            .unwrap()
    );
    assert_eq!(paired_state(&reader), (seq, replayed, end, None));

    // A commit cut before its slot write: its segment and Manifest frames follow the WAL, and the slots still name
    // the commit before (what a reader sees while a commit is in progress). The file is replaced whole, so the
    // reader keeps the one it opened.
    let before = std::fs::read(&path).unwrap();
    writer.commit().unwrap();
    drop(writer);
    let mut cut = std::fs::read(&path).unwrap();
    let slots_end = usize::try_from(
        Container::create(dir.path().join("scratch.persia"))
            .unwrap()
            .superblock()
            .log_end,
    )
    .unwrap();
    cut[..slots_end].copy_from_slice(&before[..slots_end]);
    let staged = dir.path().join("cut.persia");
    std::fs::write(&staged, &cut).unwrap();
    std::fs::rename(&staged, &path).unwrap();
    let in_progress = ReadOnlyDatabase::open(&path, &options.registry).unwrap();
    assert_eq!(
        (
            in_progress.seq(),
            in_progress.replayed(),
            in_progress.wal_end()
        ),
        (seq, replayed, end)
    );
    assert!(in_progress.container().end() > reader.database().container().end());
    assert!(!reader.refresh(in_progress).unwrap());
    assert_eq!(paired_state(&reader), (seq, replayed, end, None));
    assert!(has_document(&reader, "later"));
}

/// Two histories of one `seq` at the same path: the reader after writer A logged `a` on the database, and a read-only
/// open after the file there was replaced by a copy of its earlier state on which writer B logged `b` (what a restore
/// of a copy, or a copy put back, leaves). The reader keeps its file open, so it reads A's history on.
fn two_histories(dir: &Path, a: &[&str], b: &[&str]) -> (ReadOnlyBlobDatabase, ReadOnlyDatabase) {
    let path = dir.join("app.persia");
    let copy = dir.join("copy.persia");
    let options = DatabaseOptions::default();
    drop(create(&path));
    std::fs::copy(&path, &copy).unwrap();
    let log = |ids: &[&str]| {
        let mut db = Database::open(&path, DatabaseOptions::default()).unwrap();
        for id in ids {
            db.put("c", &Document::new(*id), Durability::Fsync).unwrap();
        }
    };
    log(a);
    let reader = ReadOnlyBlobDatabase::open(&path, &options).unwrap();
    std::fs::rename(&copy, &path).unwrap();
    log(b);
    let reopened = ReadOnlyDatabase::open(&path, &options.registry).unwrap();
    (reader, reopened)
}

/// Invariant 6: at the same `seq`, replayed WAL records and a WAL end that disagree are another history at the path,
/// whichever way they disagree: `RefreshDiverged`, and nothing changes. Both the same (the same count at the same
/// end, other records) cannot be told apart from the same state.
#[test]
fn a_refresh_with_another_history_of_the_same_seq_is_refused() {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let long = "x".repeat(500);
    let cases: [(&[&str], &[&str], _); 4] = [
        // The same count, a later end; then an earlier one.
        (&["a"], &["a-longer-id"], (Equal, Greater)),
        (&["a-longer-id"], &["a"], (Equal, Less)),
        // More records at an earlier end; fewer at a later one.
        (&[&long], &["b", "c"], (Greater, Less)),
        (&["b", "c"], &[&long], (Less, Greater)),
    ];
    for (a, b, order) in cases {
        let dir = tempfile::tempdir().unwrap();
        let (mut reader, reopened) = two_histories(dir.path(), a, b);
        let before = paired_state(&reader);
        let (seq, replayed, end, _) = before;
        assert_eq!(reopened.seq(), seq, "{order:?}");
        assert_eq!(
            (
                reopened.replayed().cmp(&replayed),
                reopened.wal_end().cmp(&end)
            ),
            order
        );
        let (reopened_replayed, reopened_end) = (reopened.replayed(), reopened.wal_end());

        let refreshed = reader.refresh(reopened);
        assert!(
            matches!(
                refreshed,
                Err(Error::RefreshDiverged {
                    current,
                    reopened,
                    current_replayed,
                    reopened_replayed: r,
                    current_end,
                    reopened_end: e,
                }) if current == seq && reopened == seq && current_replayed == replayed && current_end == end
                    && r == reopened_replayed && e == reopened_end
            ),
            "{order:?}: {refreshed:?}"
        );
        assert!(!refreshed.unwrap_err().is_corruption());
        assert_eq!(paired_state(&reader), before, "{order:?}");
        for id in a {
            assert!(has_document(&reader, id), "{order:?}: {id}");
        }
    }
}

/// Invariant 7: a GC rewrite replaces the pack. Refresh reopens it and finds every blob, including one bound after
/// the rewrite, with the documents that reference them.
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
    let opened = reader.blobs().store().superblock().unwrap();
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
    let rewritten = writer.blobs().store().superblock().unwrap();
    assert!(rewritten.generation > opened.generation);

    assert!(
        reader
            .refresh(ReadOnlyDatabase::open(&path, &options.registry).unwrap())
            .unwrap()
    );
    let reopened = reader.blobs().store().superblock().unwrap();
    assert_eq!(
        (reopened.generation, reopened.pack_id),
        (rewritten.generation, rewritten.pack_id)
    );
    assert_eq!(reader.database().seq(), writer.log().seq());
    assert!(has_document(&reader, "kept") && has_document(&reader, "after"));
    assert_eq!(read_all(&reader, &kept), b"kept");
    assert_eq!(read_all(&reader, &after), b"after");
}

/// A reader with blob `kept` committed, and a writer that has committed `newer` since (the reader's pack lacks it).
struct Lagging {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    writer: BlobDatabase<PackStore>,
    reader: ReadOnlyBlobDatabase,
    kept: persia_blob::BlobRef,
    newer: persia_blob::BlobRef,
}

fn lagging() -> Lagging {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut writer = create(&path);
    let mut put = |id: &str, bytes: &'static [u8]| {
        let blob = writer
            .put(
                "c",
                &Document::new(id),
                Blob::from_reader(bytes),
                Durability::Fsync,
            )
            .unwrap();
        writer.commit().unwrap();
        blob
    };
    let kept = put("a", b"kept");
    let reader = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default()).unwrap();
    let newer = put("b", b"newer");
    Lagging {
        _dir: dir,
        path,
        writer,
        reader,
        kept,
        newer,
    }
}

impl Lagging {
    /// Refreshes with a newer open that `check` accepts the error of, and checks the reader kept its documents and
    /// its pack: the same state, `kept` read through the old pack, `newer` not in it.
    fn refresh_fails(mut self, check: impl Fn(&Error) -> bool) {
        let before = paired_state(&self.reader);
        let newer_open =
            ReadOnlyDatabase::open(&self.path, &DatabaseOptions::default().registry).unwrap();
        assert!(newer_open.seq() > before.0);
        let refreshed = self.reader.refresh(newer_open);
        assert!(matches!(&refreshed, Err(e) if check(e)), "{refreshed:?}");
        assert_eq!(paired_state(&self.reader), before);
        assert!(has_document(&self.reader, "a"));
        assert!(!has_document(&self.reader, "b"));
        assert_eq!(read_all(&self.reader, &self.kept), b"kept");
        assert!(not_found(&self.reader, &self.newer));
    }
}

/// Invariant 7: a pack error during refresh (not `PackReplaced`) restores the documents, so they stay paired with
/// the pack this reader opened.
#[cfg(unix)]
#[test]
fn a_pack_error_on_refresh_leaves_the_reader_paired() {
    let lagging = lagging();
    // A new file at the pack's path. The reader's descriptor keeps the pack it opened (Unix).
    let pack = pack_path(&lagging.path);
    std::fs::remove_file(&pack).unwrap();
    std::fs::write(&pack, b"not a pack").unwrap();
    lagging.refresh_fails(
        |e| matches!(e, Error::Blob(e) if !matches!(e, persia_blob::Error::PackReplaced { .. })),
    );
}

/// Invariant 7: the replacement is the pack of another database. The pack refresh reports `PackReplaced` (another
/// pack id), and the reopen, with this reader's `db_uuid`, refuses it: nothing changes.
#[test]
fn a_reopen_of_another_databases_pack_leaves_the_reader_paired() {
    let lagging = lagging();
    let dir = lagging.path.parent().unwrap();
    let other = dir.join("other.persia");
    let db = create(&other);
    let (_, pins) = db
        .put_blob(&b"foreign"[..], &PutBlobOptions::default())
        .unwrap();
    drop((pins, db));
    // Renamed over the pack, as a rewrite is: the reader keeps the file it opened.
    let staged = dir.join("staged.blobs");
    std::fs::copy(pack_path(&other), &staged).unwrap();
    std::fs::rename(&staged, pack_path(&lagging.path)).unwrap();
    let own = lagging.writer.log().container().header().db_uuid;
    lagging.refresh_fails(|e| {
        matches!(
            e,
            Error::Blob(persia_blob::Error::PackOfAnotherDatabase { found }) if *found != own
        )
    });
}

/// Invariant 7: a GC rewrite replaced the pack, then the new file lost its last byte. The pack refresh reads only its
/// prefix and reports `PackReplaced`; the reopen finds a committed frame past the file's end: corruption, and
/// nothing changes.
#[test]
fn a_reopen_of_a_damaged_replacement_leaves_the_reader_paired() {
    let lagging = lagging();
    // The lagging reader's lease would hold this GC back; the rewrite is what this test needs.
    let report = ignoring_reader_leases(&lagging.path, || {
        lagging
            .writer
            .gc(&GcOptions {
                rewrite_ratio: 0.0,
                ..GcOptions::default()
            })
            .unwrap()
    });
    assert!(
        matches!(report.sweep.rewrite, Some(Rewrite::Done { .. })),
        "{report:?}"
    );
    let pack = std::fs::OpenOptions::new()
        .write(true)
        .open(pack_path(&lagging.path))
        .unwrap();
    pack.set_len(pack.metadata().unwrap().len() - 1).unwrap();
    drop(pack);
    lagging.refresh_fails(Error::is_corruption);
}

/// Invariant 8 (SPEC §9.5): the documents were opened before the writer deleted `gone`, and a GC that ignores reader
/// leases (a writer from before ADR-0054) swept its blob; the refresh then reopens the rewritten pack. The reader
/// shows `gone`, whose blob is not bound: `BlobNotFound`, and the other blob reads on.
#[test]
fn a_pack_newer_than_the_documents_misses_a_swept_blob() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let options = DatabaseOptions::default();
    let mut writer = create(&path);
    let mut put = |id: &str, bytes: &'static [u8]| {
        writer
            .put(
                "c",
                &Document::new(id),
                Blob::from_reader(bytes),
                Durability::Fsync,
            )
            .unwrap()
    };
    let gone = put("gone", b"swept");
    let kept = put("kept", b"kept");
    writer.commit().unwrap();
    let mut reader = ReadOnlyBlobDatabase::open(&path, &options).unwrap();
    let documents = ReadOnlyDatabase::open(&path, &options.registry).unwrap();

    writer
        .log_mut()
        .delete("c", "gone", Durability::Fsync)
        .unwrap();
    writer.release_durable();
    writer.commit().unwrap();
    let report = ignoring_reader_leases(&path, || {
        writer
            .gc(&GcOptions {
                rewrite_ratio: 0.0,
                ..GcOptions::default()
            })
            .unwrap()
    });
    assert!(
        matches!(report.sweep.rewrite, Some(Rewrite::Done { .. })),
        "{report:?}"
    );
    assert_eq!(report.sweep.blobs_deleted, 1, "{report:?}");

    assert!(reader.refresh(documents).unwrap(), "the rewritten pack");
    let snapshot = reader.database().snapshot();
    let doc = snapshot.collection("c").unwrap().get("gone").unwrap();
    assert_eq!(doc.unwrap().blob_ref(), Some(&gone));
    assert!(not_found(&reader, &gone));
    assert!(matches!(
        reader.get_blob_range(&gone, 0..1),
        Err(Error::Blob(persia_blob::Error::BlobNotFound { .. }))
    ));
    assert_eq!(read_all(&reader, &kept), b"kept");
}
