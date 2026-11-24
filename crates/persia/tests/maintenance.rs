//! The maintenance thread: background commits and merges, and the shutdown of `close` and `Drop` (ROADMAP
//! 10.2.3; SPEC §8.2, §8.5, §14; ADR-0064). Time is a `ManualClock` and rounds run on `maintain_now`: no sleeps.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use persia::{Collection, Db, DetachedSlot, Document, ErrorCode, OpenOptions};
use persia_engine::db::{Clock, ManualClock};

const START: i64 = 1_700_000_000_000_000;

fn held(clock: &Arc<ManualClock>) -> OpenOptions {
    let clock: Arc<dyn Clock> = Arc::clone(clock) as Arc<dyn Clock>;
    OpenOptions::new().clock(clock).hold_maintenance(true)
}

fn collection(db: &Db) -> Collection {
    db.create_collection_json(r#"{"name": "c", "fields": {}}"#)
        .unwrap()
}

fn seq_on_disk(path: &Path) -> u64 {
    OpenOptions::new()
        .read_only(true)
        .open(path)
        .unwrap()
        .committed_seq()
        .unwrap()
}

#[test]
fn an_idle_memtable_is_committed_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let db = held(&clock).open(dir.path().join("app.persia")).unwrap();
    let c = collection(&db);
    let created = db.committed_seq().unwrap();
    c.put(&Document::new("a")).unwrap();
    db.maintain_now().unwrap();
    assert_eq!(db.committed_seq().unwrap(), created, "not idle yet");
    clock.advance(Duration::from_secs(9));
    db.maintain_now().unwrap();
    assert_eq!(db.committed_seq().unwrap(), created, "idle for 9 s of 10");
    clock.advance(Duration::from_secs(1));
    db.maintain_now().unwrap();
    assert_eq!(db.committed_seq().unwrap(), created + 1, "idle for 10 s");
    assert!(c.get("a").unwrap().is_some());
    // Nothing unflushed: a round commits nothing.
    clock.advance(Duration::from_secs(60));
    db.maintain_now().unwrap();
    assert_eq!(db.committed_seq().unwrap(), created + 1);
    db.close().unwrap();
}

#[test]
fn writes_that_never_pause_are_committed_at_the_interval() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let db = held(&clock)
        .commit_interval(Duration::from_secs(30))
        .open(dir.path().join("app.persia"))
        .unwrap();
    let c = collection(&db);
    let created = db.committed_seq().unwrap();
    for i in 0..6 {
        c.put(&Document::new(format!("d{i}"))).unwrap();
        db.maintain_now().unwrap();
        assert_eq!(db.committed_seq().unwrap(), created, "at {} s", i * 5);
        clock.advance(Duration::from_secs(5));
    }
    // 30 s after the first unflushed write, with a write every 5 s.
    c.put(&Document::new("last")).unwrap();
    db.maintain_now().unwrap();
    assert_eq!(db.committed_seq().unwrap(), created + 1);
    db.close().unwrap();
}

#[test]
fn merges_run_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let db = held(&clock)
        .merge_threads(0)
        .open(dir.path().join("app.persia"))
        .unwrap();
    let c = collection(&db);
    // Eight flushed segments of one tier: the policy merges them (SPEC §8.5).
    for i in 0..8 {
        c.put(&Document::new(format!("d{i}"))).unwrap();
        db.commit().unwrap();
    }
    let flushed = db.committed_seq().unwrap();
    db.maintain_now().unwrap();
    assert!(
        db.committed_seq().unwrap() > flushed,
        "the round's merge_step merged and committed"
    );
    for i in 0..8 {
        assert!(c.get(&format!("d{i}")).unwrap().is_some());
    }
    db.close().unwrap();
}

#[test]
fn close_makes_the_final_commit_and_releases_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let clock = Arc::new(ManualClock::new(START));
    let db = held(&clock).open(&path).unwrap();
    let c = collection(&db);
    let created = db.committed_seq().unwrap();
    c.put(&Document::new("a")).unwrap();
    db.close().unwrap();
    assert_eq!(seq_on_disk(&path), created + 1, "the close's commit");
    // A drop on time does the same.
    let db = held(&clock).open(&path).unwrap();
    db.collection("c")
        .unwrap()
        .put(&Document::new("b"))
        .unwrap();
    drop(db);
    assert_eq!(seq_on_disk(&path), created + 2);
    let db = Db::open(&path).unwrap();
    let c = db.collection("c").unwrap();
    assert!(c.get("a").unwrap().is_some() && c.get("b").unwrap().is_some());
    db.close().unwrap();
}

#[test]
fn a_read_only_database_has_no_round_to_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    Db::open(&path).unwrap().close().unwrap();
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    reader.maintain_now().unwrap();
    reader.close().unwrap();
}

#[test]
fn a_drop_past_its_bound_detaches_the_thread_which_keeps_the_lock_until_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let slot: DetachedSlot = Arc::new(Mutex::new(None));
    let clock = Arc::new(ManualClock::new(START));
    let db = held(&clock)
        .drop_wait(Duration::ZERO)
        .detached_into(Arc::clone(&slot))
        .open(&path)
        .unwrap();
    let c = collection(&db);
    let created = db.committed_seq().unwrap();
    c.put(&Document::new("a")).unwrap();

    // Another thread holds the writer, as a long write would.
    let (ready, is_ready) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let holder = c.clone();
    let stall = std::thread::spawn(move || {
        holder
            .hold_writer_while(|| {
                ready.send(()).unwrap();
                released.recv().unwrap();
            })
            .unwrap();
    });
    is_ready.recv().unwrap();
    drop(db); // returns at once: the thread waits for the writer
    let detached = slot
        .lock()
        .unwrap()
        .take()
        .expect("the drop detached the thread");
    assert_eq!(c.get("a").unwrap_err().code(), ErrorCode::Closed);
    let err = Db::open(&path).unwrap_err();
    assert_eq!(
        err.code(),
        ErrorCode::Conflict,
        "the detached thread holds the lock: {err}"
    );

    release.send(()).unwrap();
    stall.join().unwrap();
    detached.join().unwrap();
    // The thread made the final commit and released the lock.
    assert_eq!(seq_on_disk(&path), created + 1);
    let db = Db::open(&path).unwrap();
    assert!(db.collection("c").unwrap().get("a").unwrap().is_some());
    db.close().unwrap();
}

#[test]
fn a_panic_holding_the_writer_poisons_writes_and_close_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let clock = Arc::new(ManualClock::new(START));
    let db = held(&clock).open(&path).unwrap();
    let c = collection(&db);
    c.put(&Document::new("before")).unwrap();
    let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
        c.hold_writer_while(|| panic!("a write panicked half-way"))
    }));
    assert!(panicked.is_err());
    for err in [
        c.put(&Document::new("after")).unwrap_err(),
        db.commit().unwrap_err(),
        db.maintain_now().unwrap_err(),
    ] {
        assert_eq!(err.code(), ErrorCode::Poisoned, "{err}");
    }
    // Reads keep working on the last published snapshot.
    assert!(c.get("before").unwrap().is_some());
    let err = db.close().unwrap_err();
    assert_eq!(err.code(), ErrorCode::Poisoned, "{err}");
    // No final commit, but the lock is released and the logged write is replayed.
    let db = Db::open(&path).unwrap();
    assert!(db.collection("c").unwrap().get("before").unwrap().is_some());
    db.close().unwrap();
}

#[test]
fn a_maintenance_round_after_close_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = collection(&db);
    db.close().unwrap();
    assert_eq!(
        c.hold_writer_while(|| ()).unwrap_err().code(),
        ErrorCode::Closed
    );
}

#[test]
fn a_zero_idle_flush_or_interval_commits_at_every_round_with_unflushed_writes() {
    for (idle, interval) in [
        (Duration::ZERO, Duration::from_secs(60)),
        (Duration::from_secs(10), Duration::ZERO),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new(START));
        let db = held(&clock)
            .idle_flush(idle)
            .commit_interval(interval)
            .open(dir.path().join("app.persia"))
            .unwrap();
        let c = collection(&db);
        let created = db.committed_seq().unwrap();
        db.maintain_now().unwrap();
        assert_eq!(
            db.committed_seq().unwrap(),
            created,
            "nothing unflushed: no commit"
        );
        c.put(&Document::new("a")).unwrap();
        db.maintain_now().unwrap();
        assert_eq!(
            db.committed_seq().unwrap(),
            created + 1,
            "{idle:?} {interval:?}"
        );
        db.close().unwrap();
    }
}
