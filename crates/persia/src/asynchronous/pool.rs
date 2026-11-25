//! A database's async pool (ADR-0064 item 12): plain threads, started on first use, joined on close, separate from
//! the maintenance thread.

use std::collections::VecDeque;
use std::fmt;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use persia_engine::db::CancelToken;

use super::job::{JobFuture, Slot};
use crate::{Error, ErrorCode, Result};

/// The most threads a pool starts by default (ADR-0064 item 12: `min(4, cores)`).
const MAX_DEFAULT_THREADS: usize = 4;

/// What a job is, for the test hook.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobKind {
    /// `AsyncCollection::put`.
    Put,
    /// `AsyncCollection::put_batch`.
    PutBatch,
    /// `AsyncCollection::delete`.
    Delete,
    /// `AsyncCollection::get`.
    Get,
    /// `AsyncDb::commit`, `AsyncCollection::commit`.
    Commit,
    /// `AsyncCollection::search`.
    Search,
}

impl JobKind {
    /// Whether a started job of this kind is cancelled when its future is dropped (a read) rather than run to its
    /// end (a write).
    const fn is_read(self) -> bool {
        matches!(self, Self::Get | Self::Search)
    }
}

/// What a pool thread did with a job, for the test hook (ADR-0064 item 9's kind of hook).
#[doc(hidden)]
// Read only by a test hook.
#[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum JobEvent {
    /// The job starts: called on the pool thread before it runs, so a hook that blocks holds the job there.
    Started {
        /// The job.
        kind: JobKind,
        /// The token a read runs under; `None` for a write.
        token: Option<CancelToken>,
    },
    /// The job's future was dropped before it started: it was skipped.
    Skipped {
        /// The job.
        kind: JobKind,
    },
    /// The job ran.
    Finished {
        /// The job.
        kind: JobKind,
        /// Its error's code; `None` if it succeeded.
        error: Option<ErrorCode>,
        /// Whether its future took the result (`false`: it was dropped while the job ran).
        delivered: bool,
    },
}

/// Test hook: called by the pool's threads around each job.
#[doc(hidden)]
pub type JobHook = Arc<dyn Fn(&JobEvent) + Send + Sync>;

/// How a pool is set up: from the open's options.
#[derive(Clone, Default)]
pub(crate) struct PoolOptions {
    /// The number of threads; `min(4, cores)` when `None`.
    pub(crate) threads: Option<NonZeroUsize>,
    pub(crate) hook: Option<JobHook>,
}

impl fmt::Debug for PoolOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolOptions")
            .field("threads", &self.threads)
            .field("hook", &self.hook.is_some())
            .finish()
    }
}

/// A queued job, type-erased.
trait Job: Send {
    /// Runs the job on a pool thread, or skips it if its future was dropped.
    fn run(self: Box<Self>, hook: Option<&JobHook>);
    /// Completes the job with [`ErrorCode::Closed`]: the pool closed before it started.
    fn close(self: Box<Self>);
}

struct Task<T, F> {
    kind: JobKind,
    slot: Arc<Slot<T>>,
    work: F,
}

impl<T, F> Job for Task<T, F>
where
    T: Send + 'static,
    F: FnOnce(Option<&CancelToken>) -> Result<T> + Send + 'static,
{
    fn run(self: Box<Self>, hook: Option<&JobHook>) {
        let Self { kind, slot, work } = *self;
        let emit = |event: JobEvent| {
            if let Some(hook) = hook {
                hook(&event);
            }
        };
        if !slot.start() {
            emit(JobEvent::Skipped { kind });
            return;
        }
        emit(JobEvent::Started {
            kind,
            token: slot.token().cloned(),
        });
        // A panic in the job must not end the pool thread, nor leave its future waiting for ever.
        let result = catch_unwind(AssertUnwindSafe(|| work(slot.token()))).unwrap_or_else(|_| {
            // `Panic` is the C ABI's alone; a panic here is a bug, reported as `Io` (ErrorCode docs).
            Err(Error::new(
                ErrorCode::Io,
                "a job panicked on the database's async pool (a bug in persia)",
            ))
        });
        let error = result.as_ref().err().map(Error::code);
        let delivered = slot.finish(result);
        emit(JobEvent::Finished {
            kind,
            error,
            delivered,
        });
    }

    fn close(self: Box<Self>) {
        self.slot.finish(Err(Error::closed()));
    }
}

struct State {
    queue: VecDeque<Box<dyn Job>>,
    closed: bool,
    /// The threads, once the first job started them.
    threads: Vec<JoinHandle<()>>,
}

struct Shared {
    state: Mutex<State>,
    work: Condvar,
    options: PoolOptions,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Updates are a push, a pop or one assignment: the state stays whole across a panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A database's async pool.
#[derive(Clone)]
pub(crate) struct Pool {
    shared: Arc<Shared>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.shared.lock();
        f.debug_struct("Pool")
            .field("queued", &state.queue.len())
            .field("threads", &state.threads.len())
            .field("closed", &state.closed)
            .finish()
    }
}

impl Pool {
    /// A pool with no thread yet: the first job starts them.
    pub(crate) fn new(options: PoolOptions) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    queue: VecDeque::new(),
                    closed: false,
                    threads: Vec::new(),
                }),
                work: Condvar::new(),
                options,
            }),
        }
    }

    /// The number of threads the pool starts.
    fn thread_count(&self) -> usize {
        self.shared.options.threads.map_or_else(
            || {
                std::thread::available_parallelism()
                    .map_or(1, NonZeroUsize::get)
                    .min(MAX_DEFAULT_THREADS)
            },
            NonZeroUsize::get,
        )
    }

    /// Queues `work` as a job of `kind` and returns its future; a closed pool returns one ready with
    /// [`ErrorCode::Closed`]. A read runs under a fresh token, which dropping its future sets.
    pub(crate) fn submit<T, F>(&self, kind: JobKind, work: F) -> JobFuture<T>
    where
        T: Send + 'static,
        F: FnOnce(Option<&CancelToken>) -> Result<T> + Send + 'static,
    {
        let slot = Slot::new(kind.is_read().then(CancelToken::new));
        let mut state = self.shared.lock();
        if state.closed {
            return JobFuture::ready(Err(Error::closed()));
        }
        if state.threads.is_empty()
            && let Err(e) = self.start_threads(&mut state)
        {
            return JobFuture::ready(Err(e));
        }
        state.queue.push_back(Box::new(Task {
            kind,
            slot: Arc::clone(&slot),
            work,
        }));
        drop(state);
        self.shared.work.notify_one();
        JobFuture::new(slot)
    }

    /// Starts the pool's threads. If the OS refuses one, those started stay (they serve the queue), and only a
    /// pool with none fails.
    fn start_threads(&self, state: &mut State) -> Result<()> {
        let mut refused = None;
        for n in 0..self.thread_count() {
            let shared = Arc::clone(&self.shared);
            match std::thread::Builder::new()
                .name(format!("persia-async-{n}"))
                .spawn(move || serve(&shared))
            {
                Ok(handle) => state.threads.push(handle),
                Err(source) => {
                    refused = Some(source);
                    break;
                }
            }
        }
        match refused {
            Some(source) if state.threads.is_empty() => Err(Error::with_source(
                ErrorCode::Io,
                "starting the database's async pool",
                source,
            )),
            _ => Ok(()),
        }
    }

    /// Closes the pool: every later job is refused with [`ErrorCode::Closed`], every queued one completes with it,
    /// and the threads stop after the job each runs. Returns the threads for the caller to join (none the second
    /// time).
    pub(crate) fn close(&self) -> Vec<JoinHandle<()>> {
        let mut state = self.shared.lock();
        state.closed = true;
        let queued: Vec<Box<dyn Job>> = state.queue.drain(..).collect();
        let threads = std::mem::take(&mut state.threads);
        drop(state);
        self.shared.work.notify_all();
        for job in queued {
            job.close();
        }
        threads
    }
}

/// A pool thread: runs queued jobs in order until the pool closes.
fn serve(shared: &Shared) {
    loop {
        let mut state = shared.lock();
        let job = loop {
            if let Some(job) = state.queue.pop_front() {
                break job;
            }
            if state.closed {
                return;
            }
            state = shared
                .work
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        };
        drop(state);
        job.run(shared.options.hook.as_ref());
    }
}

/// Joins the pool's `threads`; a thread can only have panicked in the test hook, which is logged.
pub(crate) fn join(threads: Vec<JoinHandle<()>>) {
    for thread in threads {
        if thread.join().is_err() {
            tracing::error!("a thread of the database's async pool panicked");
        }
    }
}
