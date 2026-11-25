//! The async API (ROADMAP 10.1.11; ADR-0064 items 11 to 14): `AsyncDb` and `AsyncCollection` on the database's own
//! pool, and what dropping a future does to its job, driven by a `block_on` written here (no runtime is a
//! dev-dependency, ROADMAP 0.6.11).
//!
//! Jobs are held where they start by the pool's test hook (`OpenOptions::async_hook`), on a pool of one thread
//! (`OpenOptions::async_threads`), so which job is queued and which runs is decided by the test, not by timing. No
//! test sleeps.
#![cfg(feature = "async")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::future::Future;
use std::num::NonZeroUsize;
use std::path::Path;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use persia::{
    AsyncCollection, AsyncDb, Db, Document, ErrorCode, JobEvent, JobFuture, JobKind, OpenOptions,
    SearchRequest,
};

/// Wakes the thread that polls.
struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives `future` on this thread, parking it while the future is pending.
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// Watches the pool's jobs: every event goes to `events`, and the next job to start after [`Probe::hold_next`] waits
/// on the pool thread until [`Probe::release`].
struct Probe {
    events: Receiver<JobEvent>,
    hold: Arc<AtomicBool>,
    release: Sender<()>,
}

impl Probe {
    /// The probe and the options that install it, on a pool of one thread.
    fn new() -> (Self, OpenOptions) {
        let (send, events) = channel();
        let (release, released) = channel::<()>();
        let hold = Arc::new(AtomicBool::new(false));
        let holding = Arc::clone(&hold);
        let send = Mutex::new(send);
        let released = Mutex::new(released);
        let hook = move |event: &JobEvent| {
            send.lock().unwrap().send(event.clone()).unwrap();
            if matches!(event, JobEvent::Started { .. }) && holding.swap(false, Ordering::SeqCst) {
                released.lock().unwrap().recv().unwrap();
            }
        };
        let options = OpenOptions::new()
            .async_threads(NonZeroUsize::new(1).unwrap())
            .async_hook(Arc::new(hook));
        (
            Self {
                events,
                hold,
                release,
            },
            options,
        )
    }

    /// The next job to start waits for [`Probe::release`].
    fn hold_next(&self) {
        self.hold.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        self.release.send(()).unwrap();
    }

    /// The next event of a job on the pool (the open's own events are skipped: see [`Probe::next_any`]).
    fn next(&self) -> JobEvent {
        loop {
            let event = self.next_any();
            if !matches!(
                event,
                JobEvent::Started {
                    kind: JobKind::Open,
                    ..
                } | JobEvent::Finished {
                    kind: JobKind::Open,
                    ..
                }
            ) {
                return event;
            }
        }
    }

    fn next_any(&self) -> JobEvent {
        self.events.recv().unwrap()
    }
}

/// A database at `path` with the collection `notes` (a `text` field `body`) and the document `seed`, closed again.
fn seeded(path: &Path) {
    let db = Db::open(path).unwrap();
    let notes = db
        .create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)
        .unwrap();
    notes
        .put_json(r#"{"id": "seed", "body": "quick brown fox"}"#)
        .unwrap();
    db.close().unwrap();
}

fn open(path: &Path, options: OpenOptions) -> (AsyncDb, AsyncCollection) {
    let db = block_on(AsyncDb::open_with(options, path)).unwrap();
    let notes = db.collection("notes").unwrap();
    (db, notes)
}

/// The ids the collection holds, read through a plain `Db` after the async one closed.
fn has(path: &Path, id: &str) -> bool {
    let db = OpenOptions::new().read_only(true).open(path).unwrap();
    db.collection("notes").unwrap().get(id).unwrap().is_some()
}

#[test]
fn every_call_runs_on_the_pool() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let db = block_on(AsyncDb::open(&path)).unwrap();
    let notes = db.collection("notes").unwrap();
    assert_eq!(
        db.collection("other").unwrap_err().code(),
        ErrorCode::NotFound
    );

    block_on(notes.put(Document::new("a"))).unwrap();
    block_on(notes.put_batch(vec![Document::new("b"), Document::new("c")])).unwrap();
    block_on(notes.delete("b")).unwrap();
    assert!(block_on(notes.get("a")).unwrap().is_some());
    assert!(block_on(notes.get("b")).unwrap().is_none());
    assert!(block_on(notes.get("c")).unwrap().is_some());
    block_on(notes.commit()).unwrap();
    block_on(db.commit()).unwrap();
    let response = block_on(notes.search(SearchRequest::new("fox"))).unwrap();
    assert_eq!(response.hits.len(), 1);
    assert_eq!(response.hits[0].document.id(), "seed");
    // A job's own error comes back through its future.
    let err = block_on(notes.search(SearchRequest::new("fox").fields(["nope"]))).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument);

    block_on(db.close()).unwrap();
    assert!(has(&path, "a") && !has(&path, "b") && has(&path, "c"));
}

#[test]
fn an_open_reports_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.persia");
    let err = block_on(AsyncDb::open_with(
        OpenOptions::new().create_if_missing(false),
        &missing,
    ))
    .unwrap_err();
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert!(!missing.exists());
}

#[test]
fn a_future_dropped_before_its_job_starts_skips_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let (probe, options) = Probe::new();
    let (db, notes) = open(&path, options);

    probe.hold_next();
    let first = notes.put(Document::new("first"));
    assert!(matches!(
        probe.next(),
        JobEvent::Started {
            kind: JobKind::Put,
            ..
        }
    ));
    // The pool's one thread is held in `first`: this job is queued behind it.
    let second = notes.put(Document::new("second"));
    drop(second);
    probe.release();
    assert!(matches!(
        probe.next(),
        JobEvent::Finished {
            kind: JobKind::Put,
            error: None,
            delivered: true
        }
    ));
    assert!(matches!(
        probe.next(),
        JobEvent::Skipped { kind: JobKind::Put }
    ));
    block_on(first).unwrap();
    block_on(db.close()).unwrap();
    assert!(has(&path, "first"));
    assert!(!has(&path, "second"), "the skipped put never ran");
}

#[test]
fn a_started_write_runs_to_its_end_and_its_result_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let (probe, options) = Probe::new();
    let (db, notes) = open(&path, options);

    let writes: [(JobKind, Submit<'_, ()>); 3] = [
        (JobKind::Put, Box::new(|| notes.put(Document::new("put")))),
        (
            JobKind::PutBatch,
            Box::new(|| notes.put_batch(vec![Document::new("b1"), Document::new("b2")])),
        ),
        (JobKind::Delete, Box::new(|| notes.delete("seed"))),
    ];
    for (kind, submit) in writes {
        probe.hold_next();
        let future = submit();
        let started = probe.next();
        assert!(
            matches!(&started, JobEvent::Started { kind: k, token: None } if *k == kind),
            "{started:?}"
        );
        drop(future);
        probe.release();
        let finished = probe.next();
        assert!(
            matches!(&finished, JobEvent::Finished { kind: k, error: None, delivered: false } if *k == kind),
            "{finished:?}"
        );
    }
    block_on(db.close()).unwrap();
    assert!(has(&path, "put") && has(&path, "b1") && has(&path, "b2"));
    assert!(!has(&path, "seed"), "the dropped delete ran");
}

#[test]
fn a_started_read_has_its_token_set_when_its_future_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let (probe, options) = Probe::new();
    let (db, notes) = open(&path, options);

    let reads: [(JobKind, Box<dyn Fn() -> AnyFuture>); 2] = [
        (
            JobKind::Search,
            Box::new(|| -> AnyFuture { Box::new(notes.search(SearchRequest::new("fox"))) }),
        ),
        (
            JobKind::Get,
            Box::new(|| -> AnyFuture { Box::new(notes.get("seed")) }),
        ),
    ];
    for (kind, submit) in reads {
        probe.hold_next();
        let future = submit();
        let JobEvent::Started {
            kind: started,
            token: Some(token),
        } = probe.next()
        else {
            panic!("a read starts with a token");
        };
        assert_eq!(started, kind);
        assert!(!token.is_cancelled());
        drop(future);
        assert!(token.is_cancelled(), "dropping the future set the token");
        probe.release();
        let finished = probe.next();
        assert!(
            matches!(
                &finished,
                JobEvent::Finished { kind: k, error: Some(ErrorCode::Cancelled), delivered: false } if *k == kind
            ),
            "the read saw the token: {finished:?}"
        );
    }

    // A read whose future stays is not cancelled.
    probe.hold_next();
    let kept = notes.get("seed");
    let JobEvent::Started {
        token: Some(token), ..
    } = probe.next()
    else {
        panic!("a read starts with a token");
    };
    probe.release();
    assert!(block_on(kept).unwrap().is_some());
    assert!(!token.is_cancelled());
    block_on(db.close()).unwrap();
}

/// What queues one job.
type Submit<'a, T> = Box<dyn Fn() -> JobFuture<T> + 'a>;

/// A future of any output: what the reads' loop holds and drops.
type AnyFuture = Box<dyn std::any::Any>;

#[test]
fn jobs_still_queued_at_close_complete_with_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let (probe, options) = Probe::new();
    let (db, notes) = open(&path, options);

    probe.hold_next();
    let running = notes.put(Document::new("running"));
    assert!(matches!(probe.next(), JobEvent::Started { .. }));
    let queued_put = notes.put(Document::new("queued"));
    let queued_get = notes.get("seed");
    let closing = db.close();
    // Completed by the close itself, while the running job is still held.
    assert_eq!(block_on(queued_put).unwrap_err().code(), ErrorCode::Closed);
    assert_eq!(block_on(queued_get).unwrap_err().code(), ErrorCode::Closed);
    // A call after the close is refused at once.
    assert_eq!(
        block_on(notes.put(Document::new("late")))
            .unwrap_err()
            .code(),
        ErrorCode::Closed
    );
    assert_eq!(
        block_on(notes.search(SearchRequest::new("fox")))
            .unwrap_err()
            .code(),
        ErrorCode::Closed
    );
    probe.release();
    // The close joins the pool (the running job ends) before it closes the database.
    block_on(closing).unwrap();
    block_on(running).unwrap();
    assert!(has(&path, "running"));
    assert!(!has(&path, "queued"));
}

#[test]
fn dropping_the_database_refuses_and_completes_what_is_queued() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let (probe, options) = Probe::new();
    let (db, notes) = open(&path, options);

    probe.hold_next();
    let running = notes.put(Document::new("running"));
    assert!(matches!(probe.next(), JobEvent::Started { .. }));
    let queued = notes.put(Document::new("queued"));
    // The drop does not join the pool (the held job keeps its thread): it closes the queue and drops the database.
    let dropping = std::thread::spawn(move || drop(db));
    assert_eq!(block_on(queued).unwrap_err().code(), ErrorCode::Closed);
    probe.release();
    dropping.join().unwrap();
    // The running put ends, against a database that is closed or closing.
    let ran = block_on(running);
    assert!(
        ran.is_ok() || ran.as_ref().unwrap_err().code() == ErrorCode::Closed,
        "{ran:?}"
    );
    assert_eq!(
        block_on(notes.get("seed")).unwrap_err().code(),
        ErrorCode::Closed
    );
}

#[test]
fn a_future_polled_after_it_completed_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let db = block_on(AsyncDb::open(&path)).unwrap();
    let notes = db.collection("notes").unwrap();
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(notes.get("seed"));
    let first = loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => break output,
            Poll::Pending => std::thread::park(),
        }
    };
    assert!(first.unwrap().is_some());
    let Poll::Ready(again) = future.as_mut().poll(&mut cx) else {
        panic!("a completed future is ready");
    };
    assert_eq!(again.unwrap_err().code(), ErrorCode::InvalidArgument);
    block_on(db.close()).unwrap();
}

#[test]
fn a_read_only_database_reads_and_refuses_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let db = block_on(AsyncDb::open_with(
        OpenOptions::new().read_only(true),
        &path,
    ))
    .unwrap();
    let notes = db.collection("notes").unwrap();
    assert!(block_on(notes.get("seed")).unwrap().is_some());
    assert_eq!(
        block_on(notes.put(Document::new("x"))).unwrap_err().code(),
        ErrorCode::ReadOnly
    );
    assert_eq!(
        block_on(db.commit()).unwrap_err().code(),
        ErrorCode::ReadOnly
    );
    block_on(db.close()).unwrap();
}

/// Counts its wakes.
#[derive(Default)]
struct Count(std::sync::atomic::AtomicUsize);

impl Wake for Count {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn the_waker_of_the_latest_poll_is_woken() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let (probe, options) = Probe::new();
    let (db, notes) = open(&path, options);

    probe.hold_next();
    let mut future = notes.get("seed");
    assert!(matches!(probe.next(), JobEvent::Started { .. }));
    let (first, second) = (Arc::new(Count::default()), Arc::new(Count::default()));
    for count in [&first, &second] {
        let waker = Waker::from(Arc::clone(count));
        let polled = std::pin::Pin::new(&mut future).poll(&mut Context::from_waker(&waker));
        assert!(polled.is_pending());
    }
    // An executor that moved the task polls with another waker: that one is woken, not the first.
    probe.release();
    assert!(matches!(
        probe.next(),
        JobEvent::Finished {
            delivered: true,
            ..
        }
    ));
    assert_eq!(first.0.load(Ordering::SeqCst), 0);
    assert_eq!(second.0.load(Ordering::SeqCst), 1);
    assert!(block_on(future).unwrap().is_some());
    block_on(db.close()).unwrap();
}

#[test]
fn handles_say_what_they_are() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let (_probe, options) = Probe::new();
    assert!(format!("{options:?}").contains("hook: true"), "{options:?}");
    let (db, notes) = open(&path, options);
    let shown = format!("{db:?}");
    assert!(
        shown.contains("AsyncDb") && shown.contains("app.persia") && shown.contains("queued: 0"),
        "{shown}"
    );
    let shown = format!("{notes:?}");
    assert!(
        shown.contains("AsyncCollection") && shown.contains("\"notes\""),
        "{shown}"
    );
    let future = notes.get("seed");
    assert!(format!("{future:?}").starts_with("JobFuture"), "{future:?}");
    block_on(future).unwrap();
    block_on(db.close()).unwrap();
}

/// Handles cross threads, and a future can be stored and moved without pinning.
#[test]
fn handles_and_futures_are_send_sync_and_unpin() {
    fn send_sync<T: Send + Sync>() {}
    fn unpin<T: Unpin>() {}
    send_sync::<AsyncDb>();
    send_sync::<AsyncCollection>();
    send_sync::<JobFuture<()>>();
    send_sync::<JobFuture<Option<Document>>>();
    send_sync::<JobFuture<persia::SearchResponse>>();
    send_sync::<JobFuture<AsyncDb>>();
    unpin::<JobFuture<()>>();
    unpin::<JobFuture<AsyncDb>>();
}

/// An open whose future is dropped while it runs finishes on its thread and closes what it opened: the writer lock
/// is free once that thread reports the open finished.
#[test]
fn an_open_whose_future_is_dropped_closes_what_it_opened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let (probe, options) = Probe::new();
    probe.hold_next();
    let opening = AsyncDb::open_with(options, &path);
    assert!(matches!(
        probe.next_any(),
        JobEvent::Started {
            kind: JobKind::Open,
            token: None
        }
    ));
    drop(opening);
    probe.release();
    let finished = probe.next_any();
    assert!(
        matches!(
            finished,
            JobEvent::Finished {
                kind: JobKind::Open,
                error: None,
                delivered: false
            }
        ),
        "{finished:?}"
    );
    // The dropped open's database is closed: a read-write open takes the lock.
    Db::open(&path).unwrap().close().unwrap();
}
