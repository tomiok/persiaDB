//! What the async API's tests share (ROADMAP 10.1.11, 10.1.26): a `block_on` written here (no runtime is a
//! dev-dependency, ROADMAP 0.6.11), and a probe that holds the pool's one thread where a job starts, so which job is
//! queued and which runs is decided by the test, not by timing.
#![allow(dead_code, clippy::unwrap_used)] // Each test binary uses part of it; inputs are fixed.

use std::future::Future;
use std::path::Path;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use persia::{AsyncCollection, AsyncDb, Db, JobEvent, JobKind, OpenOptions};

/// Wakes the thread that polls.
pub struct Unpark(pub std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives `future` on this thread, parking it while the future is pending.
pub fn block_on<F: Future>(future: F) -> F::Output {
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
pub struct Probe {
    events: Receiver<JobEvent>,
    hold: Arc<AtomicBool>,
    release: Sender<()>,
}

impl Probe {
    /// The probe and the options that install it, on a pool of one thread.
    pub fn new() -> (Self, OpenOptions) {
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
            .async_threads(1)
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
    pub fn hold_next(&self) {
        self.hold.store(true, Ordering::SeqCst);
    }

    pub fn release(&self) {
        self.release.send(()).unwrap();
    }

    /// The next event of a job on the pool (the open's own events are skipped: see [`Probe::next_any`]).
    pub fn next(&self) -> JobEvent {
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

    pub fn next_any(&self) -> JobEvent {
        self.events.recv().unwrap()
    }

    /// Whether no event is waiting: true right after a call that queued no job, once every earlier event was taken.
    pub fn quiet(&self) -> bool {
        matches!(self.events.try_recv(), Err(TryRecvError::Empty))
    }
}

/// A database at `path` with the collection `notes` (a `text` field `body`) and the document `seed`, closed again.
pub fn seeded(path: &Path) {
    let db = Db::open(path).unwrap();
    let notes = db
        .create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)
        .unwrap();
    notes
        .put_json(r#"{"id": "seed", "body": "quick brown fox"}"#)
        .unwrap();
    db.close().unwrap();
}

pub fn open(path: &Path, options: OpenOptions) -> (AsyncDb, AsyncCollection) {
    let db = block_on(AsyncDb::open_with(options, path)).unwrap();
    let notes = db.collection("notes").unwrap();
    (db, notes)
}

/// The ids the collection holds, read through a plain `Db` after the async one closed.
pub fn has(path: &Path, id: &str) -> bool {
    let db = OpenOptions::new().read_only(true).open(path).unwrap();
    db.collection("notes").unwrap().get(id).unwrap().is_some()
}
