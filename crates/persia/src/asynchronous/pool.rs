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
    /// `AsyncDb::open`, on its own thread.
    Open,
    /// `AsyncDb::create_collection`, `AsyncDb::create_collection_json`.
    CreateCollection,
    /// `AsyncDb::drop_collection`.
    DropCollection,
    /// `AsyncDb::refresh`.
    Refresh,
    /// `AsyncDb::backup`.
    Backup,
    /// `AsyncDb::put_blob`.
    PutBlob,
    /// `AsyncDb::get_blob_range`.
    GetBlobRange,
    /// `AsyncCollection::put_with_blob`.
    PutWithBlob,
    /// `AsyncCollection::put_json`, `AsyncCollection::put_struct` (the value's JSON).
    PutJson,
    /// `AsyncCollection::get_json`, `AsyncCollection::get_struct`.
    GetJson,
    /// `AsyncCollection::search_json`.
    SearchJson,
}

impl JobKind {
    /// Whether a started job of this kind is cancelled when its future is dropped (a read) rather than run to its
    /// end (a write).
    const fn is_read(self) -> bool {
        matches!(
            self,
            Self::Get | Self::Search | Self::GetJson | Self::SearchJson | Self::GetBlobRange
        )
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
    /// The number of threads (`OpenOptions::async_threads`); `min(4, cores)` when `None`. 0 is refused at open.
    pub(crate) threads: Option<usize>,
    pub(crate) hook: Option<JobHook>,
    /// Test hook: the OS is taken to refuse the thread of this index (0-based) at the pool's first start.
    #[cfg(feature = "test-hooks")]
    pub(crate) refuse_thread: Option<usize>,
}

impl fmt::Debug for PoolOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolOptions")
            .field("threads", &self.threads)
            .field("hook", &self.hook.is_some())
            .finish_non_exhaustive()
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
        let emit = |event: JobEvent| emit(hook, &event);
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
    /// Which start of the pool its threads belong to. A start the OS refused a thread of moves it on, so the threads
    /// that start made see the change and end (all or nothing: a pool runs every thread it was given, or none).
    generation: u64,
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

/// A start of the pool that failed: its error, and the threads it had started, for the caller to join once it has
/// released the pool's lock (they end when they see the generation moved on).
struct Refused {
    error: Error,
    started: Vec<JoinHandle<()>>,
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
                    generation: 0,
                }),
                work: Condvar::new(),
                options,
            }),
        }
    }

    /// The number of threads the pool starts.
    fn thread_count(&self) -> usize {
        self.shared.options.threads.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(1, NonZeroUsize::get)
                .min(MAX_DEFAULT_THREADS)
        })
    }

    /// The number of threads the pool runs now: 0 before its first job and after it closed.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn running_threads(&self) -> usize {
        self.shared.lock().threads.len()
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
            && let Err(refused) = self.start_threads(&mut state)
        {
            drop(state);
            // The threads that start made end: none was given a job, and each sees the generation moved on.
            self.shared.work.notify_all();
            join(refused.started);
            return JobFuture::ready(Err(refused.error));
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

    /// Starts the pool's threads, all or none: if the OS refuses one, the ones started end, the job that asked is
    /// refused with [`ErrorCode::Io`], and the next job tries again.
    fn start_threads(&self, state: &mut State) -> std::result::Result<(), Refused> {
        let count = self.thread_count();
        if count == 0 {
            // `OpenOptions::open` refuses 0; a pool with no thread would leave every job queued for ever.
            return Err(Refused {
                error: Error::new(
                    ErrorCode::InvalidArgument,
                    "the database's async pool needs at least one thread",
                ),
                started: Vec::new(),
            });
        }
        let generation = state.generation;
        let mut started = Vec::with_capacity(count);
        for n in 0..count {
            match self.spawn(n, generation) {
                Ok(handle) => started.push(handle),
                Err(source) => {
                    state.generation = generation.wrapping_add(1);
                    return Err(Refused {
                        error: Error::with_source(
                            ErrorCode::Io,
                            format_args!(
                                "starting thread {n} of the database's async pool of {count} threads"
                            ),
                            source,
                        ),
                        started,
                    });
                }
            }
        }
        state.threads = started;
        Ok(())
    }

    /// Spawns the pool thread `n` of the start `generation`.
    fn spawn(&self, n: usize, generation: u64) -> std::io::Result<JoinHandle<()>> {
        #[cfg(feature = "test-hooks")]
        if generation == 0 && self.shared.options.refuse_thread == Some(n) {
            return Err(std::io::Error::other(
                "the OS refused the thread (a test hook)",
            ));
        }
        let shared = Arc::clone(&self.shared);
        std::thread::Builder::new()
            .name(format!("persia-async-{n}"))
            .spawn(move || serve(&shared, generation))
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

/// A pool thread of the start `generation`: runs queued jobs in order until the pool closes, or ends at once if that
/// start failed.
fn serve(shared: &Shared, generation: u64) {
    loop {
        let mut state = shared.lock();
        let job = loop {
            if state.generation != generation {
                return;
            }
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

/// Calls the test hook, if there is one, with `event`.
pub(crate) fn emit(hook: Option<&JobHook>, event: &JobEvent) {
    if let Some(hook) = hook {
        hook(event);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)] // Tests: inputs are fixed.

    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use super::*;

    struct Unpark(std::thread::Thread);

    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

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

    #[test]
    fn a_job_that_panics_is_an_io_error_and_its_thread_serves_the_next_job() {
        let pool = Pool::new(PoolOptions {
            threads: Some(1),
            ..PoolOptions::default()
        });
        let panicked = pool.submit(JobKind::Put, |_| -> Result<()> { panic!("a bug in a job") });
        let err = block_on(panicked).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Io);
        // The one thread survived the panic: it runs the next job.
        let next = pool.submit(JobKind::Get, |token| Ok(token.is_some()));
        assert!(block_on(next).unwrap(), "a read runs with a token");
        join(pool.close());
    }

    #[test]
    fn a_closed_pool_refuses_jobs() {
        let pool = Pool::new(PoolOptions::default());
        join(pool.close());
        let refused = pool.submit(JobKind::Commit, |_| Ok(()));
        assert_eq!(block_on(refused).unwrap_err().code(), ErrorCode::Closed);
        assert!(pool.close().is_empty(), "closed once");
    }

    #[test]
    fn a_pool_of_no_thread_refuses_its_jobs_rather_than_queue_them_for_ever() {
        // `OpenOptions::open` refuses `async_threads(0)`; the pool does not rely on it.
        let pool = Pool::new(PoolOptions {
            threads: Some(0),
            ..PoolOptions::default()
        });
        let refused = pool.submit(JobKind::Commit, |_| Ok(()));
        assert_eq!(
            block_on(refused).unwrap_err().code(),
            ErrorCode::InvalidArgument
        );
        assert!(pool.close().is_empty(), "no thread started");
    }
}
