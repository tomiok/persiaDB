//! The maintenance thread of a read-write database (SPEC §8.2, §8.5; ROADMAP 10.2.3, 10.2.6; ADR-0065).
//!
//! One thread per read-write [`Db`](crate::Db), named `persia-maintenance`, none per collection and none on a
//! read-only database. A round is one `tick` (the commit the engine's `CommitPolicy` finds due on the database's
//! clock: a full memtable, an idle one, or the commit interval) and one `merge_step` (finished merges committed, new
//! ones started on the engine's merge workers), under the writer mutex. While anything is unflushed or a merge is in
//! flight a round runs every [`PERIOD`]; after a round that leaves neither, the thread parks with no timer until the
//! next write. The close is the thread's last step: the final commit, the merges in flight cancelled, the database
//! dropped (which releases the writer lock), and the first error sent back.

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::db::{Inner, Writer};
use crate::{Error, ErrorCode, Result};

/// How often a round runs while there is work: it bounds how late an idle flush or an interval commit can be.
pub(crate) const PERIOD: Duration = Duration::from_secs(1);

/// What a requested round reports back: its error's code and message (an [`Error`] is not `Clone`; the first error
/// itself goes to the close).
type Outcome = std::result::Result<(), (ErrorCode, String)>;

/// What the application's threads and the maintenance thread share.
#[derive(Debug, Default)]
pub(crate) struct Signals {
    state: Mutex<State>,
    /// Wakes the thread: a write, a requested round, a stop.
    wake: Condvar,
    /// Wakes the callers of [`Signals::round_now`] when their round is done.
    served: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// Writes since the open: the thread compares it with the count its last round saw, so it never parks over a
    /// write that came in while a round ran.
    writes: u64,
    /// Rounds requested by [`Signals::round_now`], and how many of them ran, with the last one's outcome.
    requested: u64,
    done: u64,
    last: Option<Outcome>,
    /// Only requested rounds run (a test hook: [`crate::OpenOptions::hold_maintenance`]).
    held: bool,
    stop: bool,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    // No code panics while holding it, and its counters stay consistent if one did.
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the thread does next.
enum Next {
    Round { requested: bool, writes: u64 },
    Stop,
}

impl Signals {
    pub(crate) fn new(held: bool) -> Self {
        Self {
            state: Mutex::new(State {
                held,
                ..State::default()
            }),
            ..Self::default()
        }
    }

    /// A write was made: a parked thread starts its rounds again.
    pub(crate) fn wrote(&self) {
        let mut state = lock(&self.state);
        state.writes = state.writes.wrapping_add(1);
        drop(state);
        self.wake.notify_one();
    }

    /// Asks for one round now and waits for it; its outcome.
    pub(crate) fn round_now(&self) -> Result<()> {
        let mut state = lock(&self.state);
        if state.stop {
            return Err(Error::closed());
        }
        state.requested = state.requested.wrapping_add(1);
        let ticket = state.requested;
        self.wake.notify_one();
        while state.done < ticket && !state.stop {
            state = self
                .served
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.done < ticket {
            return Err(Error::closed());
        }
        match state.last.clone() {
            None | Some(Ok(())) => Ok(()),
            Some(Err((code, message))) => Err(Error::new(code, message)),
        }
    }

    /// Tells the thread to make its last step, and the callers waiting for a round that it will not run.
    pub(crate) fn stop(&self) {
        lock(&self.state).stop = true;
        self.wake.notify_all();
        self.served.notify_all();
    }

    /// Waits until the next round is due (or a stop): at once for a requested round, [`PERIOD`] after the last
    /// round while there is work (`busy`, or a write since `writes_seen`), never while there is none.
    fn next(&self, busy: bool, writes_seen: u64) -> Next {
        let mut state = lock(&self.state);
        let mut due: Option<Instant> = None;
        loop {
            if state.stop {
                return Next::Stop;
            }
            if state.requested > state.done {
                return Next::Round {
                    requested: true,
                    writes: state.writes,
                };
            }
            let active = !state.held && (busy || state.writes != writes_seen);
            if !active {
                due = None;
                state = self
                    .wake
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            let at = *due.get_or_insert_with(|| Instant::now() + PERIOD);
            let now = Instant::now();
            if now >= at {
                return Next::Round {
                    requested: false,
                    writes: state.writes,
                };
            }
            state = self
                .wake
                .wait_timeout(state, at - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn served(&self, outcome: Outcome) {
        let mut state = lock(&self.state);
        state.done = state.done.wrapping_add(1);
        state.last = Some(outcome);
        drop(state);
        self.served.notify_all();
    }
}

/// The thread's handle: how the close waits for it.
#[derive(Debug)]
pub(crate) struct Thread {
    handle: JoinHandle<()>,
    result: Receiver<Result<()>>,
}

impl Thread {
    /// Starts the thread of `inner`, a read-write database.
    ///
    /// # Errors
    /// [`ErrorCode::Io`] if the OS refuses a thread.
    pub(crate) fn start(inner: &Arc<Inner>) -> Result<Self> {
        let (send, result) = std::sync::mpsc::sync_channel(1);
        let inner = Arc::clone(inner);
        let handle = std::thread::Builder::new()
            .name("persia-maintenance".to_owned())
            .spawn(move || run(&inner, &send))
            .map_err(|source| {
                Error::with_source(ErrorCode::Io, "starting the maintenance thread", source)
            })?;
        Ok(Self { handle, result })
    }

    /// Waits for the thread's last step without limit; its result. `None` if it panicked before it sent one.
    pub(crate) fn join(self) -> Option<Result<()>> {
        let result = self.result.recv().ok();
        let joined = self.handle.join();
        result.filter(|_| joined.is_ok())
    }

    /// [`Thread::join`] for up to `bound`. `Err(self)` past the bound: the thread still runs.
    pub(crate) fn join_within(
        self,
        bound: Duration,
    ) -> std::result::Result<Option<Result<()>>, Self> {
        match self.result.recv_timeout(bound) {
            Ok(result) => Ok(self.handle.join().ok().map(|()| result)),
            Err(RecvTimeoutError::Disconnected) => {
                let _panicked = self.handle.join();
                Ok(None)
            }
            Err(RecvTimeoutError::Timeout) => Err(self),
        }
    }

    /// The thread's handle, given up on: it ends on its own.
    pub(crate) fn detach(self) -> JoinHandle<()> {
        self.handle
    }
}

/// The thread's body: rounds until a stop, then the last step ([`Inner::finish`]).
fn run(inner: &Inner, send: &SyncSender<Result<()>>) {
    let Some(signals) = inner.signals() else {
        return;
    };
    let mut first_error: Option<Error> = None;
    // An open may have replayed writes: start active.
    let mut busy = true;
    let mut writes_seen = 0;
    // The writer stopped for good (a failed write poisoned it, or a thread panicked holding it): no more rounds.
    let mut stopped = false;
    while let Next::Round { requested, writes } = signals.next(busy && !stopped, writes_seen) {
        writes_seen = writes;
        let outcome = if stopped {
            Err(Error::new(
                ErrorCode::Poisoned,
                "the database's writer stopped after a failure: close it and open it again",
            ))
        } else {
            inner.with_writer(round)
        };
        let shared = match outcome {
            Ok(more) => {
                busy = more;
                Ok(())
            }
            Err(error) => {
                stopped |= error.code() == ErrorCode::Poisoned;
                let shared = Err((error.code(), error.message().to_owned()));
                if first_error.is_none() {
                    tracing::error!(path = %inner.path().display(), %error, "a background commit or merge failed");
                    first_error = Some(error);
                }
                shared
            }
        };
        if requested {
            signals.served(shared);
        }
    }
    let last = inner.finish();
    let result = match (first_error, last) {
        (Some(first), Err(last)) => {
            tracing::error!(path = %inner.path().display(), error = %last, "the final commit failed too");
            Err(first)
        }
        (Some(first), Ok(())) => Err(first),
        (None, last) => last,
    };
    // Nobody waits any more after a `Drop` that gave up: then only the log reports a failure.
    if let Err(TrySendError::Disconnected(Err(error)) | TrySendError::Full(Err(error))) =
        send.try_send(result)
    {
        tracing::error!(path = %inner.path().display(), %error, "closing a dropped database failed");
    }
}

/// One round on the writer: whether a later round has work (something unflushed, a merge in flight).
fn round(db: &mut Writer) -> Result<bool> {
    let ticked = db.log_mut().tick().map(drop);
    // A commit made the documents before it durable: their blobs need no pin any more (SPEC §9.5).
    db.release_durable();
    ticked.map_err(Error::wrap)?;
    db.log_mut().merge_step().map(drop).map_err(Error::wrap)?;
    Ok(has_work(db))
}

/// Whether a later round has something to do: an unflushed write, or a merge in flight.
fn has_work(db: &Writer) -> bool {
    let log = db.log();
    log.merges_running() > 0
        || log
            .snapshot()
            .collections()
            .any(|(_, c)| c.memtable().version() > 0)
}
