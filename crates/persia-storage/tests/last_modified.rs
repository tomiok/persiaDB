//! `ObjectMeta::last_modified` (ROADMAP 9.5.5; SPEC §10.1, ADR-0050 item 6): what each backend's `list` reports.
//!
//! Invariants:
//! 1. Memory: the time of the write that created the object's current bytes, from the store's `Clock`; a `put`
//!    replacing it moves the time, a `put_if_absent` that finds equal bytes (`Created`) or other bytes
//!    (`AlreadyExists`) does not.
//! 2. Local: the object file's modification time, exactly (set here with `File::set_modified`).
//! 3. A backend that keeps no times reports `None`: the local backend on `SimFs`.
//! 4. `FaultyStorage` passes the inner backend's times through.
//! 5. Local, on real files: a `put_if_absent` that finds equal or other bytes keeps the time; a `put` (even of equal
//!    bytes) and a re-create after a delete move it to now; times survive a reopen.

#![allow(clippy::unwrap_used, clippy::unnecessary_wraps, missing_docs)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use persia_storage::faulty::FaultyStorage;
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::vfs::sim::SimFs;
use persia_storage::{Key, KeyPrefix, LocalStorage, MemoryStorage, PutOutcome, Storage};

fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

fn times<S: Storage + ?Sized>(storage: &S) -> Vec<(String, Option<SystemTime>)> {
    storage
        .list(&KeyPrefix::default())
        .unwrap()
        .into_iter()
        .map(|m| (m.key.as_str().to_owned(), m.last_modified))
        .collect()
}

fn at(secs: u64) -> Option<SystemTime> {
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

#[test]
fn memory_reports_the_time_its_clock_gave_the_write() {
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_secs(100)));
    let s = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    s.put(&key("a"), Bytes::from_static(b"1")).unwrap();
    clock.advance(Duration::from_secs(10));
    assert_eq!(
        s.put_if_absent(&key("b"), Bytes::from_static(b"2"))
            .unwrap(),
        PutOutcome::Created
    );
    assert_eq!(times(&s), [("a".into(), at(100)), ("b".into(), at(110))]);

    clock.advance(Duration::from_secs(10));
    // Equal bytes (this writer's own earlier attempt) and other bytes leave the object, and its time, alone.
    assert_eq!(
        s.put_if_absent(&key("a"), Bytes::from_static(b"1"))
            .unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        s.put_if_absent(&key("b"), Bytes::from_static(b"x"))
            .unwrap(),
        PutOutcome::AlreadyExists
    );
    assert_eq!(times(&s), [("a".into(), at(100)), ("b".into(), at(110))]);
    // A replacing put writes new bytes: a new time.
    s.put(&key("a"), Bytes::from_static(b"3")).unwrap();
    assert_eq!(times(&s), [("a".into(), at(120)), ("b".into(), at(110))]);
    // A clock set back is what the next write reads.
    clock.set(UNIX_EPOCH);
    s.put(&key("c"), Bytes::new()).unwrap();
    assert_eq!(times(&s)[2], ("c".into(), at(0)));
}

#[test]
fn memory_uses_the_system_clock_by_default() {
    let before = SystemTime::now();
    let s = MemoryStorage::new();
    s.put(&key("a"), Bytes::from_static(b"1")).unwrap();
    let t = s.list(&KeyPrefix::default()).unwrap()[0]
        .last_modified
        .unwrap();
    assert!(t >= before && t <= SystemTime::now(), "{t:?}");
}

#[test]
fn local_reports_the_file_modification_time() {
    let dir = tempfile::tempdir().unwrap();
    let s = LocalStorage::open(dir.path()).unwrap();
    s.put(&key("blobs/ab/x"), Bytes::from_static(b"1")).unwrap();
    assert_eq!(
        s.put_if_absent(&key("blobs/y"), Bytes::from_static(b"2"))
            .unwrap(),
        PutOutcome::Created
    );
    // `blobs/ab/x` is the file `blobs@/ab@/x` (SPEC §10.1.1).
    for (path, secs) in [("blobs@/ab@/x", 1_000_000), ("blobs@/y", 2_000_000)] {
        let file = std::fs::File::options()
            .write(true)
            .open(dir.path().join(path))
            .unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }
    assert_eq!(
        times(&s),
        [
            ("blobs/ab/x".into(), at(1_000_000)),
            ("blobs/y".into(), at(2_000_000))
        ]
    );
    let prefixed = s.list(&KeyPrefix::new("blobs/ab/").unwrap()).unwrap();
    assert_eq!(prefixed[0].last_modified, at(1_000_000));
}

#[test]
fn a_backend_without_times_reports_none() {
    let fs = SimFs::new();
    let s = LocalStorage::open_in(fs, "/root").unwrap();
    s.put(&key("a"), Bytes::from_static(b"1")).unwrap();
    assert_eq!(times(&s), [("a".into(), None)]);
}

#[test]
fn faulty_storage_passes_the_times_through() {
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_secs(7)));
    let faulty = FaultyStorage::new(MemoryStorage::with_clock(clock), 1);
    faulty.put(&key("a"), Bytes::from_static(b"1")).unwrap();
    assert_eq!(times(&faulty), [("a".into(), at(7))]);
}

/// Sets the modification time of the local object file at `path` (relative to the store root).
fn set_mtime(root: &std::path::Path, path: &str, secs: u64) {
    std::fs::File::options()
        .write(true)
        .open(root.join(path))
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

#[test]
fn local_keeps_the_time_on_put_if_absent_and_moves_it_on_put() {
    // A sweep ages objects by this time (SPEC §9.5): a retried `put_if_absent` must not make an object look new, and
    // a replacing `put` must (its bytes are new).
    let dir = tempfile::tempdir().unwrap();
    let s = LocalStorage::open(dir.path()).unwrap();
    let k = key("blobs/uploads/x");
    assert_eq!(
        s.put_if_absent(&k, Bytes::from_static(b"one")).unwrap(),
        PutOutcome::Created
    );
    set_mtime(dir.path(), "blobs@/uploads@/x", 1_000_000);
    // Equal bytes (the caller's own earlier attempt) and other bytes leave the file alone.
    assert_eq!(
        s.put_if_absent(&k, Bytes::from_static(b"one")).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        s.put_if_absent(&k, Bytes::from_static(b"two")).unwrap(),
        PutOutcome::AlreadyExists
    );
    assert_eq!(times(&s), [("blobs/uploads/x".into(), at(1_000_000))]);
    assert_eq!(&s.get(&k).unwrap()[..], b"one");

    // A `put`, even of equal bytes, writes a new file: its time is now, by the file system's (coarser) clock.
    let before = SystemTime::now() - Duration::from_secs(2);
    for bytes in [&b"one"[..], b"three"] {
        set_mtime(dir.path(), "blobs@/uploads@/x", 1_000_000);
        s.put(&k, Bytes::copy_from_slice(bytes)).unwrap();
        let t = times(&s)[0].1.unwrap();
        assert!(
            t >= before && t <= SystemTime::now() + Duration::from_secs(2),
            "{t:?}"
        );
    }
    // A delete and a re-create is a new object with a new time.
    s.delete(&k).unwrap();
    assert_eq!(times(&s), []);
    assert_eq!(
        s.put_if_absent(&k, Bytes::from_static(b"one")).unwrap(),
        PutOutcome::Created
    );
    assert!(times(&s)[0].1.unwrap() >= before);
}

#[test]
fn local_times_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = LocalStorage::open(dir.path()).unwrap();
        s.put(&key("a"), Bytes::from_static(b"1")).unwrap();
    }
    set_mtime(dir.path(), "a", 3_000_000);
    let s = LocalStorage::open(dir.path()).unwrap();
    assert_eq!(times(&s), [("a".into(), at(3_000_000))]);
}
