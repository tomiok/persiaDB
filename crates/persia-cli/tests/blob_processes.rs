//! A database and its blob pack across real processes and real files (SPEC §9.3, §9.5, §4.8; ROADMAP 9.2.7, 9.4.5,
//! 9.5.3, 9.5.6; ADR-0053). The children are this test binary re-executed (`blob_support`); they report each step on
//! stdout and wait on stdin, so no test sleeps.
//!
//! Invariants:
//! 1. **One writer** (9.2.7): while a process holds the database read-write, a second process's
//!    `BlobDatabase::open` fails on the container's writer lock (`flock`, which belongs to the open file description,
//!    so another process is the real case), and never opens the pack read-write: a stale pack temporary file, which a
//!    read-write pack open sweeps, is still there, and the pack's bytes are unchanged. A read-only open beside the
//!    writer reads its blob. Once the writer exits, the next read-write open succeeds and sweeps it. A holder that
//!    drops its database but keeps a clone of the pack's `Arc` keeps the pack's own writer lock: a second process
//!    opens the container, then fails on the pack (`PackLocked`) before it sweeps or writes.
//! 2. **A concurrent reader** (9.4.5): a `ReadOnlyBlobDatabase` opened before the writer's first `put_blob` (no pack
//!    file) creates none; each `refresh` with a newer read-only open, while another process writes, shows at least
//!    every document acknowledged before it, and every document it shows has its blob, byte-identical. The last
//!    refresh shows them all.
//! 3. **Copy semantics** (SPEC §4.8): a copy of only `<name>.persia`, restored elsewhere, fails `persia verify` with
//!    exit 3 at every depth, naming the collection, a document id and that document's `blob_id`; copying the pack too
//!    makes it pass. A copy taken beside a live writer, container first and then pack, each read once from start to
//!    end, passes `persia verify --deep`, and every document it shows reads its blob back.
//! 4. **Symlinks** (Unix): a database opened through a symbolic link (absolute, in another directory, or relative)
//!    finds and creates its pack beside the real file, never beside the link, even when a file of that name sits
//!    beside the link; `persia verify` through the link checks the real pack.
//!
//! Seeds: `PERSIA_TEST_SEED` (printed) sets the copy points and the refresh points; contents are a function of the
//! document id. On a failure the temp dir is kept and its path printed.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::too_many_lines
)] // Tests: inputs are generated or fixed.

mod blob_support;

use std::collections::BTreeSet;
use std::io::{BufRead, Read};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use blob_support::{
    COLLECTION, ChildIo, KeptOnFailure, Rng, base_seed, blob_of_id, child_env, content, live_ids,
    parent_gone, report, row, serial, verify_corrupt, verify_ok, wait_for_eof,
};
use persia::internal::documents::{Blob, BlobDatabase, Error, ReadOnlyBlobDatabase};
use persia_engine::db::{DatabaseError, DatabaseOptions, ReadOnlyDatabase};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_storage::container::Error as ContainerError;

/// The share of a bounded writer's blobs that span at least one 4 MiB chunk.
const BIG_PERCENT: u64 = 10;

// ---------------------------------------------------------------------------------------------------------------
// Child roles
// ---------------------------------------------------------------------------------------------------------------

/// The entry point of every child process; a no-op in a normal test run.
#[test]
fn child_role() {
    let Some(env) = child_env() else {
        return;
    };
    match env.role.as_str() {
        "holder" => holder(&env.path),
        "clone-holder" => clone_holder(&env.path),
        "second" => second(&env.path),
        "reader" => reader(&env.path),
        "writer" => writer(&env.path, &env.prefix, env.count),
        "put-one" => put_one(&env.path, &env.prefix),
        other => panic!("unknown child role {other}"),
    }
}

/// The blob of document `id`, as every role derives it.
fn bytes_of(id: &str) -> Vec<u8> {
    let (size, cseed) = blob_of_id(id, BIG_PERCENT);
    content(size, cseed)
}

/// Puts document `$id` with its blob (`bytes_of`) at `Fsync`: a macro, as the pack store type of
/// `BlobDatabase::open` lives in `persia-blob`, which this crate does not depend on.
macro_rules! put {
    ($db:expr, $id:expr) => {
        $db.put(
            COLLECTION,
            &Document::new($id),
            Blob::from_reader(&bytes_of($id)[..]),
            Durability::Fsync,
        )
        .unwrap()
    };
}

/// Holds the database read-write with one document `held` and its blob, until stdin closes.
fn holder(path: &Path) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    let blob = put!(db, "held");
    report(&format!("ready {}", blob.blob_id()));
    wait_for_eof();
}

/// Opens the database read-write, puts `held`, then drops the database and keeps only a clone of its pack's `Arc`
/// until stdin closes: the container's lock is released, the pack's is not.
fn clone_holder(path: &Path) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    let blob = put!(db, "held");
    let kept = std::sync::Arc::clone(db.blobs().store());
    drop(db);
    report(&format!("ready {}", blob.blob_id()));
    wait_for_eof();
    drop(kept);
}

/// Tries a second read-write open, reports `locked`, `opened` or `error <e>`; when locked, reads `held`'s blob
/// through a read-only open beside the writer and reports `read <len>`.
fn second(path: &Path) {
    match BlobDatabase::open(path, DatabaseOptions::default()) {
        Err(Error::Database(e))
            if matches!(*e, DatabaseError::Container(ContainerError::Locked)) =>
        {
            report("locked");
        }
        // The pack's writer lock (persia-blob's `PackLocked`, named by its message: this crate does not depend on
        // persia-blob).
        Err(e @ Error::Blob(_)) if e.to_string().contains("locked by another writer") => {
            report("pack-locked");
            return;
        }
        Ok(db) => {
            drop(db);
            report("opened");
            return;
        }
        Err(e) => {
            report(&format!("error {e}"));
            return;
        }
    }
    let ro = ReadOnlyBlobDatabase::open(path, &DatabaseOptions::default()).unwrap();
    let doc = ro
        .database()
        .snapshot()
        .collection(COLLECTION)
        .unwrap()
        .get("held")
        .unwrap()
        .unwrap();
    let mut out = Vec::new();
    ro.get_blob(doc.blob_ref().unwrap())
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    assert!(out == bytes_of("held"), "held's blob differs");
    report(&format!("read {}", out.len()));
}

/// Opens read-only (reporting whether a pack existed), then on each `refresh` line refreshes with a newer read-only
/// open and checks every document's blob, reporting `refreshed <documents> <found new> <bytes checked>`.
fn reader(path: &Path) {
    let options = DatabaseOptions::default();
    let pack = path.with_extension("persia.blobs");
    let mut ro = ReadOnlyBlobDatabase::open(path, &options).unwrap();
    report(&format!("opened {}", pack.exists()));
    assert!(!pack.exists(), "a read-only open created the pack");
    for line in std::io::stdin().lock().lines() {
        assert_eq!(line.unwrap(), "refresh");
        let found = ro
            .refresh(ReadOnlyDatabase::open(path, &options.registry).unwrap())
            .unwrap();
        let snapshot = ro.database().snapshot();
        let c = snapshot.collection(COLLECTION).unwrap();
        let mut checked = 0;
        let ids = live_ids(&snapshot, COLLECTION);
        for id in &ids {
            let doc = c.get(id).unwrap().unwrap();
            let blob = doc.blob_ref().unwrap();
            let mut out = Vec::new();
            ro.get_blob(blob)
                .unwrap_or_else(|e| panic!("reader: get_blob({id}) after refresh: {e}"))
                .read_to_end(&mut out)
                .unwrap();
            assert!(out == bytes_of(id), "reader: {id}'s blob differs");
            checked += out.len();
        }
        report(&format!("refreshed {} {found} {checked}", ids.len()));
    }
}

/// Puts `<prefix>-0` .. `<prefix>-<count - 1>` at `Fsync`, committing every 4, reporting `start <id>` and
/// `ack <id>`, then `done` (or stops early once stdin closes).
fn writer(path: &Path, prefix: &str, count: u64) {
    let gone = parent_gone();
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    report("opened");
    for n in 0..count {
        if gone.load(Ordering::Relaxed) {
            return;
        }
        let id = format!("{prefix}-{n}");
        report(&format!("start {id}"));
        put!(db, &id);
        report(&format!("ack {id}"));
        if n % 4 == 3 {
            db.commit().unwrap();
        }
    }
    report("done");
}

/// Opens `path` read-write and puts one document `<prefix>` with its blob; reports `ack <blob_id>`.
fn put_one(path: &Path, id: &str) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    let blob = put!(db, id);
    report(&format!("ack {}", blob.blob_id()));
}

// ---------------------------------------------------------------------------------------------------------------
// Parent helpers
// ---------------------------------------------------------------------------------------------------------------

/// A database at `<dir>/app.persia` with the collection, committed, and no pack.
fn create(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("app.persia");
    let mut db = BlobDatabase::create(&path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new(COLLECTION).build().unwrap())
        .unwrap();
    db.commit().unwrap();
    path
}

fn pack_of(path: &Path) -> std::path::PathBuf {
    path.with_extension("persia.blobs")
}

/// Every live document of the database at `path`, read-only, with its blob checked byte for byte; returns the ids.
fn check_blobs(path: &Path, ctx: &str) -> BTreeSet<String> {
    let ro = ReadOnlyBlobDatabase::open(path, &DatabaseOptions::default())
        .unwrap_or_else(|e| panic!("{ctx}: read-only open: {e}"));
    let snapshot = ro.database().snapshot();
    let c = snapshot.collection(COLLECTION).unwrap();
    let ids = live_ids(&snapshot, COLLECTION);
    for id in &ids {
        let doc = c.get(id).unwrap().unwrap();
        let Some(blob) = doc.blob_ref() else {
            assert!(id.starts_with("plain"), "{ctx}: {id} has no blob");
            continue;
        };
        let mut out = Vec::new();
        ro.get_blob(blob)
            .unwrap_or_else(|e| panic!("{ctx}: get_blob({id}): {e}"))
            .read_to_end(&mut out)
            .unwrap();
        assert!(out == bytes_of(id), "{ctx}: {id}'s blob differs");
    }
    ids
}

/// The id of the `n`th document of writer `prefix`.
fn nth(prefix: &str, n: u64) -> String {
    format!("{prefix}-{n}")
}

// ---------------------------------------------------------------------------------------------------------------
// 1. One writer
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_second_process_fails_on_the_lock_and_never_opens_the_pack_read_write() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let dir = KeptOnFailure::new();
    let path = create(dir.path());
    let pack = pack_of(&path);

    let holder = ChildIo::spawn("holder", &path, 0, "", 0);
    let ready = holder.expect("holder");
    assert!(ready.starts_with("ready "), "{ready}");
    // A stale temporary of a crashed pack creation: a read-write pack open removes it (SPEC §9.3).
    let stale = dir.path().join("app.persia.blobs.tmp.0123456789abcdef.7");
    std::fs::write(&stale, b"stale").unwrap();
    let pack_before = std::fs::read(&pack).unwrap();

    for attempt in 0..3 {
        let ctx = format!("second writer {attempt}");
        let second = ChildIo::spawn("second", &path, 0, "", 0);
        assert_eq!(second.expect(&ctx), "locked", "{ctx}");
        assert_eq!(
            second.expect(&ctx),
            format!("read {}", bytes_of("held").len())
        );
        second.finish();
        assert!(
            stale.exists(),
            "{ctx}: the locked-out open swept the pack's temporaries"
        );
        assert!(
            std::fs::read(&pack).unwrap() == pack_before,
            "{ctx}: the locked-out open changed the pack"
        );
    }
    // The CLI's read-only verify beside the writer passes too.
    let rows = verify_ok(&["--deep"], &path, "verify beside the holder");
    assert_eq!(row(&rows, "pack"), "1 blobs bound");
    assert!(stale.exists());

    holder.finish();
    // Now the lock is free: a read-write open succeeds and sweeps the temporary, so the check above was meaningful.
    let third = ChildIo::spawn("second", &path, 0, "", 0);
    assert_eq!(third.expect("third"), "opened");
    third.finish();
    assert!(
        !stale.exists(),
        "a read-write open after the writer left kept the stale temporary"
    );
    assert_eq!(
        check_blobs(&path, "after"),
        BTreeSet::from(["held".to_owned()])
    );
}
/// The pack's own writer lock (9.2.7): a holder that drops its database but keeps a clone of the pack's `Arc` has
/// released the container's lock, not the pack's. A second process opens the container read-write, then fails on the
/// pack's lock before it sweeps or writes anything; once the clone is dropped, the next open succeeds.
#[test]
fn a_second_process_fails_on_the_pack_lock_a_cloned_store_keeps() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let dir = KeptOnFailure::new();
    let path = create(dir.path());
    let pack = pack_of(&path);

    let holder = ChildIo::spawn("clone-holder", &path, 0, "", 0);
    let ready = holder.expect("clone holder");
    assert!(ready.starts_with("ready "), "{ready}");
    let stale = dir.path().join("app.persia.blobs.tmp.0123456789abcdef.7");
    std::fs::write(&stale, b"stale").unwrap();
    let pack_before = std::fs::read(&pack).unwrap();
    for attempt in 0..2 {
        let ctx = format!("second writer {attempt}");
        let second = ChildIo::spawn("second", &path, 0, "", 0);
        assert_eq!(second.expect(&ctx), "pack-locked", "{ctx}");
        second.finish();
        assert!(
            stale.exists(),
            "{ctx}: the locked-out open swept the pack's temporaries"
        );
        assert!(
            std::fs::read(&pack).unwrap() == pack_before,
            "{ctx}: the locked-out open changed the pack"
        );
    }
    holder.finish();
    let third = ChildIo::spawn("second", &path, 0, "", 0);
    assert_eq!(third.expect("third"), "opened");
    third.finish();
    assert!(
        !stale.exists(),
        "the read-write open kept the stale temporary"
    );
    assert_eq!(
        check_blobs(&path, "after"),
        BTreeSet::from(["held".to_owned()])
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 2. A concurrent reader
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_reader_opened_before_the_pack_existed_refreshes_beside_a_writer_process() {
    const COUNT: u64 = 16;
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let seed = base_seed(0x5eed_0945_4ead);
    let mut rng = Rng(seed);
    let dir = KeptOnFailure::new();
    let path = create(dir.path());

    let mut reader = ChildIo::spawn("reader", &path, 0, "", 0);
    assert_eq!(reader.expect("reader"), "opened false");
    assert!(!pack_of(&path).exists());
    // Before the writer stores anything: nothing new, still no pack.
    reader.send("refresh");
    assert_eq!(reader.expect("reader"), "refreshed 0 false 0");
    assert!(!pack_of(&path).exists());

    let started = Instant::now();
    let writer = ChildIo::spawn("writer", &path, 0, "w", COUNT);
    assert_eq!(writer.expect("writer"), "opened");
    // Refresh right after the first `start` (the pack may be mid-creation), right after the first `ack`, and at
    // seeded reports after those.
    let mut points: BTreeSet<u64> = BTreeSet::from([1, 2]);
    while points.len() < 8 {
        points.insert(rng.range(3, 2 * COUNT + 1));
    }
    eprintln!("refresh after writer reports {points:?}");
    let (mut acked, mut reports, mut refreshes, mut grew) = (0_u64, 0_u64, 0, 0);
    let mut last = 0_u64;
    loop {
        let msg = writer.expect("writer");
        if msg == "done" {
            break;
        }
        reports += 1;
        if msg.starts_with("ack ") {
            acked += 1;
        }
        if points.contains(&reports) {
            reader.send("refresh");
            let reply = reader.expect("reader");
            let parts: Vec<&str> = reply.split(' ').collect();
            assert_eq!(parts[0], "refreshed", "{reply}");
            let docs: u64 = parts[1].parse().unwrap();
            assert!(
                docs >= acked,
                "refresh at report {reports} shows {docs} documents, {acked} were acknowledged before it"
            );
            assert!(
                docs >= last,
                "a refresh lost documents: {docs} after {last}"
            );
            grew += u64::from(docs > last);
            last = docs;
            refreshes += 1;
        }
    }
    reader.send("refresh");
    let reply = reader.expect("reader");
    assert!(reply.starts_with(&format!("refreshed {COUNT} ")), "{reply}");
    writer.finish();
    reader.finish();
    eprintln!(
        "{refreshes} refreshes beside the writer ({grew} found more documents), {COUNT} puts in {:.1?}",
        started.elapsed()
    );
    let ids = check_blobs(&path, "after");
    assert_eq!(ids.len(), usize::try_from(COUNT).unwrap());
    assert!(ids.contains(&nth("w", 0)));
}

// ---------------------------------------------------------------------------------------------------------------
// 3. Copy semantics
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_copy_of_only_the_container_dangles_and_a_copy_of_both_files_verifies() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let dir = KeptOnFailure::new();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let path = create(&src);
    // One committed document with a blob, one only in the WAL, one with none.
    let mut db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
    let a = put!(db, "a");
    db.commit().unwrap();
    let b = put!(db, "b");
    db.log_mut()
        .put(COLLECTION, &Document::new("plain"), Durability::Fsync)
        .unwrap();
    drop(db);
    let expected = verify_ok(&["--deep"], &path, "source");
    assert_eq!(row(&expected, "references"), "2 to 2 blobs");

    // Restored elsewhere under the same name, container only.
    let only = dir.path().join("only");
    std::fs::create_dir(&only).unwrap();
    let only_db = only.join("app.persia");
    std::fs::copy(&path, &only_db).unwrap();
    for args in [&[][..], &["--quick"][..], &["--deep"][..]] {
        let err = verify_corrupt(args, &only_db);
        assert!(
            err.contains(&format!("\"{COLLECTION}\"")),
            "{args:?}: {err}"
        );
        assert!(err.contains("no blob pack"), "{args:?}: {err}");
        let named = [("a", &a), ("b", &b)]
            .into_iter()
            .filter(|(id, blob)| {
                err.contains(&format!("document \"{id}\""))
                    && err.contains(&blob.blob_id().to_string())
            })
            .count();
        assert_eq!(
            named, 1,
            "{args:?}: one document and its own blob_id: {err}"
        );
        assert!(!err.contains("\"plain\""), "{args:?}: {err}");
    }
    assert!(
        !pack_of(&only_db).exists(),
        "verify created a pack beside the copy"
    );

    // Copy the pack beside it: it verifies, at every depth, as the source does.
    std::fs::copy(pack_of(&path), pack_of(&only_db)).unwrap();
    for args in [&[][..], &["--quick"][..]] {
        verify_ok(args, &only_db, "both files");
    }
    let deep = verify_ok(&["--deep"], &only_db, "both files");
    assert_eq!(row(&deep, "references"), "2 to 2 blobs");
    assert_eq!(row(&deep, "rehashed"), "2 blobs");
    assert_eq!(
        check_blobs(&only_db, "both files"),
        BTreeSet::from(["a".to_owned(), "b".to_owned(), "plain".to_owned()])
    );
}

#[test]
fn a_copy_container_first_then_pack_beside_a_live_writer_verifies() {
    const COUNT: u64 = 24;
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let seed = base_seed(0x5eed_0408_c0b1);
    let mut rng = Rng(seed);
    let dir = KeptOnFailure::new();
    let path = create(dir.path());

    let writer = ChildIo::spawn("writer", &path, 0, "w", COUNT);
    assert_eq!(writer.expect("writer"), "opened");
    let mut points = BTreeSet::new();
    while points.len() < 4 {
        points.insert(rng.range(1, COUNT));
    }
    eprintln!("copy after acks {points:?}");
    let mut copies = Vec::new();
    let mut acked = 0_u64;
    loop {
        let msg = writer.expect("writer");
        if msg == "done" {
            break;
        }
        if !msg.starts_with("ack ") {
            continue;
        }
        acked += 1;
        if points.contains(&acked) {
            let to = dir.path().join(format!("copy-{acked}"));
            std::fs::create_dir(&to).unwrap();
            let copy = to.join("app.persia");
            // SPEC §4.8: the container first, then the pack, each read once from its start to its end.
            blob_support::copy_by_reading(&path, &copy);
            blob_support::copy_by_reading(&pack_of(&path), &pack_of(&copy));
            copies.push((acked, copy));
        }
    }
    writer.finish();
    assert_eq!(copies.len(), points.len());
    for (acked, copy) in &copies {
        let ctx = format!("copy after {acked} acks");
        let rows = verify_ok(&["--deep"], copy, &ctx);
        eprintln!(
            "{ctx}: references {}, tail {}",
            row(&rows, "references"),
            row(&rows, "tail")
        );
        let ids = check_blobs(copy, &ctx);
        for n in 0..*acked {
            assert!(ids.contains(&nth("w", n)), "{ctx}: lost {}", nth("w", n));
        }
        // The copy's first read-write open recovers it (a torn tail is truncated); it still verifies.
        drop(BlobDatabase::open(copy, DatabaseOptions::default()).unwrap());
        verify_ok(&["--deep"], copy, &format!("{ctx}, reopened"));
        assert_eq!(
            check_blobs(copy, &ctx),
            ids,
            "{ctx}: the reopen changed the documents"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 4. Symlinks
// ---------------------------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_symlinked_container_finds_its_pack_beside_the_real_file() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let dir = KeptOnFailure::new();
    let real = dir.path().join("real");
    let links = dir.path().join("links");
    std::fs::create_dir(&real).unwrap();
    std::fs::create_dir(&links).unwrap();
    let path = create(&real);
    let alias = links.join("alias.persia");
    let relative = links.join("rel.persia");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    std::os::unix::fs::symlink("../real/app.persia", &relative).unwrap();
    // A file where a pack beside the link would be: opening through the link must neither read nor replace it.
    let decoy = pack_of(&alias);
    std::fs::write(&decoy, b"not a pack").unwrap();

    let one = ChildIo::spawn("put-one", &alias, 0, "via-alias", 0);
    assert!(one.expect("alias").starts_with("ack "));
    one.finish();
    assert!(pack_of(&path).exists(), "no pack beside the real file");
    assert_eq!(std::fs::read(&decoy).unwrap(), b"not a pack");
    let rows = verify_ok(&["--deep"], &alias, "verify through the alias");
    assert_eq!(row(&rows, "pack"), "1 blobs bound");

    let two = ChildIo::spawn("put-one", &relative, 0, "via-relative", 0);
    assert!(two.expect("relative").starts_with("ack "));
    two.finish();
    assert!(
        !pack_of(&relative).exists(),
        "a pack beside the relative link"
    );
    assert_eq!(std::fs::read(&decoy).unwrap(), b"not a pack");
    for through in [&path, &alias, &relative] {
        let rows = verify_ok(&["--deep"], through, "verify");
        assert_eq!(row(&rows, "pack"), "2 blobs bound", "{}", through.display());
        assert_eq!(row(&rows, "rehashed"), "2 blobs");
        assert_eq!(
            check_blobs(through, "read"),
            BTreeSet::from(["via-alias".to_owned(), "via-relative".to_owned()])
        );
    }
    let names: BTreeSet<String> = std::fs::read_dir(&real)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    // The reader leases directory, beside the real pack, is empty once every reader has gone (SPEC §9.3).
    // The pack's writer lock file is beside the real pack too (ROADMAP 9.2.15).
    assert_eq!(
        names,
        BTreeSet::from([
            "app.persia".to_owned(),
            "app.persia.blobs".to_owned(),
            "app.persia.blobs.lock".to_owned(),
            "app.persia.blobs.leases".to_owned(),
        ])
    );
    assert_eq!(
        std::fs::read_dir(real.join("app.persia.blobs.leases"))
            .unwrap()
            .count(),
        0
    );
}
