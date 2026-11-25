//! A job's result slot and the future that waits on it (ADR-0064 items 12 and 13).
//!
//! A [`JobFuture`] and the thread that runs its job share one [`Slot`]: the thread moves it from queued to running
//! and stores the result; the future takes the result, or, when it is dropped first, abandons the slot. Abandoning a
//! queued job makes the pool skip it, abandoning a running one discards its result and, for a read, cancels its
//! [`CancelToken`]. The waker is the one of the last poll, woken outside the slot's lock.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;

use persia_engine::db::CancelToken;

use crate::{Error, ErrorCode, Result};

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting in the pool's queue (or, for a job on its own thread, about to start).
    Queued,
    /// A thread runs it.
    Running,
    /// Its result is stored, or was taken.
    Done,
    /// Its future was dropped before the result was taken.
    Abandoned,
}

struct State<T> {
    phase: Phase,
    result: Option<Result<T>>,
    waker: Option<Waker>,
}

/// What a job and its future share.
pub(crate) struct Slot<T> {
    state: Mutex<State<T>>,
    /// Set when a running read's future is dropped; `None` for writes, which run to their end.
    token: Option<CancelToken>,
}

impl<T> Slot<T> {
    /// A slot for a job not started; `token` is the one a read runs under.
    pub(crate) fn new(token: Option<CancelToken>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                phase: Phase::Queued,
                result: None,
                waker: None,
            }),
            token,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        // Every update is one assignment or a `take`, and none can panic halfway: the state stays whole.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The token a read runs under.
    pub(crate) fn token(&self) -> Option<&CancelToken> {
        self.token.as_ref()
    }

    /// Moves a queued job to running; `false` if its future was dropped (the job is skipped) or it already ended.
    pub(crate) fn start(&self) -> bool {
        let mut state = self.lock();
        if state.phase == Phase::Queued {
            state.phase = Phase::Running;
            true
        } else {
            false
        }
    }

    /// Stores the job's result and wakes its future. `false` if the future was dropped: the result is discarded.
    pub(crate) fn finish(&self, result: Result<T>) -> bool {
        let mut state = self.lock();
        if state.phase == Phase::Abandoned {
            drop(state);
            // Dropped outside the lock: a discarded `Db` closes in its `Drop`.
            drop(result);
            return false;
        }
        state.phase = Phase::Done;
        state.result = Some(result);
        let waker = state.waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }

    /// The future is gone: a queued job will be skipped, a running read is cancelled, a stored result is dropped.
    fn abandon(&self) {
        let mut state = self.lock();
        let previous = state.phase;
        state.phase = Phase::Abandoned;
        let result = state.result.take();
        drop(state);
        if previous == Phase::Running
            && let Some(token) = &self.token
        {
            token.cancel();
        }
        drop(result);
    }
}

/// The future of a call of [`AsyncDb`](crate::AsyncDb) or [`AsyncCollection`](crate::AsyncCollection): a handle on
/// a job queued to the database's async pool (ADR-0064 item 12), which resolves to the job's result.
///
/// The job is queued when the method is called, not when the future is first polled. Dropping the future cancels
/// it as far as that is safe (ADR-0064 item 13): a job that has not started is skipped; a write that has started
/// runs to its end and its result is discarded; a search or a get that has started is cancelled (its
/// `CancelToken` is set) and its result discarded. A job still queued when the database is closed resolves to
/// [`ErrorCode::Closed`].
///
/// It needs no runtime: any executor polls it, and so does a `block_on` of a few lines. Polling it again after it
/// returned `Ready` gives [`ErrorCode::InvalidArgument`].
///
/// ```
/// use std::future::Future;
/// use std::pin::pin;
/// use std::sync::Arc;
/// use std::task::{Context, Poll, Wake};
///
/// /// The smallest executor: park the thread until the future wakes it.
/// fn block_on<F: Future>(future: F) -> F::Output {
///     struct Unpark(std::thread::Thread);
///     impl Wake for Unpark {
///         fn wake(self: Arc<Self>) {
///             self.0.unpark();
///         }
///     }
///     let waker = Arc::new(Unpark(std::thread::current())).into();
///     let mut cx = Context::from_waker(&waker);
///     let mut future = pin!(future);
///     loop {
///         match future.as_mut().poll(&mut cx) {
///             Poll::Ready(output) => return output,
///             Poll::Pending => std::thread::park(),
///         }
///     }
/// }
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("app.persia");
/// # let setup = persia::Db::open(&path)?;
/// # setup.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
/// # setup.close()?;
/// let db = block_on(persia::AsyncDb::open(&path))?;
/// let notes = db.collection("notes")?;
/// let put: persia::JobFuture<()> = notes.put(persia::Document::new("a"));
/// block_on(put)?;
/// assert!(block_on(notes.get("a"))?.is_some());
/// block_on(db.close())?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use = "a future does nothing for its caller unless polled; dropping it cancels its job"]
pub struct JobFuture<T> {
    slot: Arc<Slot<T>>,
    /// The thread of a job that runs on its own (an open, a close), joined once its result is taken.
    thread: Option<JoinHandle<()>>,
    /// The result was taken: dropping the future abandons nothing.
    taken: bool,
}

impl<T> fmt::Debug for JobFuture<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobFuture")
            .field("phase", &self.slot.lock().phase)
            .finish_non_exhaustive()
    }
}

impl<T> JobFuture<T> {
    /// The future of the job whose slot is `slot`.
    pub(crate) fn new(slot: Arc<Slot<T>>) -> Self {
        Self {
            slot,
            thread: None,
            taken: false,
        }
    }

    /// The future of the job `slot` runs on its own `thread`.
    pub(crate) fn on_thread(slot: Arc<Slot<T>>, thread: JoinHandle<()>) -> Self {
        Self {
            slot,
            thread: Some(thread),
            taken: false,
        }
    }

    /// A future that is ready at once with `result`, for a call refused before any job was queued.
    pub(crate) fn ready(result: Result<T>) -> Self {
        let slot = Slot::new(None);
        slot.finish(result);
        Self::new(slot)
    }
}

impl<T> Future for JobFuture<T> {
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<T>> {
        let this = self.get_mut();
        if this.taken {
            return Poll::Ready(Err(Error::new(
                ErrorCode::InvalidArgument,
                "a JobFuture was polled again after it completed",
            )));
        }
        let mut state = this.slot.lock();
        let Some(result) = state.result.take() else {
            match &state.waker {
                Some(waker) if waker.will_wake(cx.waker()) => {}
                _ => state.waker = Some(cx.waker().clone()),
            }
            return Poll::Pending;
        };
        drop(state);
        this.taken = true;
        // The thread stored its result as its last step, so this join is a short wait for its exit.
        if let Some(thread) = this.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("a persia async thread panicked after it delivered its result");
        }
        Poll::Ready(result)
    }
}

impl<T> Drop for JobFuture<T> {
    fn drop(&mut self) {
        if !self.taken {
            self.slot.abandon();
        }
    }
}
