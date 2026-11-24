//! The maintenance thread of a read-write database (SPEC §8.2, §8.5; ROADMAP 10.2.3, 10.2.6; ADR-0064).
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
    /// The thread ended, normally or by a panic: no round will be served any more.
    dead: bool,
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

    /// Asks for one round now and waits for it; its outcome. Reached by `Db::maintain_now` (feature `test-hooks`).
    #[cfg_attr(not(any(test, feature = "test-hooks")), allow(dead_code))]
    pub(crate) fn round_now(&self) -> Result<()> {
        let mut state = lock(&self.state);
        if state.dead {
            return Err(Error::writer_panicked());
        }
        if state.stop {
            return Err(Error::closed());
        }
        state.requested = state.requested.wrapping_add(1);
        let ticket = state.requested;
        self.wake.notify_one();
        while state.done < ticket && !state.stop && !state.dead {
            state = self
                .served
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.done < ticket {
            return Err(if state.stop {
                Error::closed()
            } else {
                Error::writer_panicked()
            });
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

    /// Waits until the next round is due (or a stop), as [`decide`] says.
    fn next(&self, busy: bool, writes_seen: u64) -> Next {
        let mut state = lock(&self.state);
        let mut due: Option<Instant> = None;
        loop {
            let now = Instant::now();
            match decide(&state, busy, writes_seen, due, now) {
                Decision::Stop => return Next::Stop,
                Decision::Round { requested } => {
                    return Next::Round {
                        requested,
                        writes: state.writes,
                    };
                }
                Decision::Park => {
                    due = None;
                    state = self
                        .wake
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                Decision::WaitUntil(at) => {
                    due = Some(at);
                    state = self
                        .wake
                        .wait_timeout(state, at.saturating_duration_since(now))
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
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

/// What the thread does from `state` at `now`, given whether its last round left work (`busy`), the write count
/// that round saw, and when the timed round it waits for is `due`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Stop,
    /// A round now: one a caller asked for, or the timed one.
    Round {
        requested: bool,
    },
    /// No work: wait, with no timer, for a write, a request or a stop.
    Park,
    /// Work: the timed round is due at this instant ([`PERIOD`] after the wait began).
    WaitUntil(Instant),
}

/// The thread's schedule (module docs): a stop first, then a requested round at once, then, while there is work
/// (`busy`, or a write since `writes_seen`) and the thread is not held, a round [`PERIOD`] after the wait began.
fn decide(
    state: &State,
    busy: bool,
    writes_seen: u64,
    due: Option<Instant>,
    now: Instant,
) -> Decision {
    if state.stop {
        return Decision::Stop;
    }
    if state.requested > state.done {
        return Decision::Round { requested: true };
    }
    let work = busy || state.writes != writes_seen;
    if state.held || !work {
        return Decision::Park;
    }
    let at = due.unwrap_or_else(|| now.checked_add(PERIOD).unwrap_or(now));
    if now >= at {
        Decision::Round { requested: false }
    } else {
        Decision::WaitUntil(at)
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

/// Sets [`State::dead`] when dropped, and wakes the callers waiting for a round.
struct Dead<'a>(&'a Signals);

impl Drop for Dead<'_> {
    fn drop(&mut self) {
        lock(&self.0.state).dead = true;
        self.0.served.notify_all();
    }
}

/// The thread's body: rounds until a stop, then the last step ([`Inner::finish`]).
fn run(inner: &Inner, send: &SyncSender<Result<()>>) {
    let Some(signals) = inner.signals() else {
        return;
    };
    // Marks the thread dead when this function ends, also by a panic, so no caller of `round_now` waits forever.
    let _dead = Dead(signals);
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

#[cfg(test)]
mod tests {
    use persia_engine::db::DatabaseOptions;
    use persia_engine::document::Document;
    use persia_engine::schema::SchemaBuilder;
    use persia_engine::wal::Durability;

    use super::*;

    fn state(writes: u64, requested: u64, done: u64, held: bool, stop: bool) -> State {
        State {
            writes,
            requested,
            done,
            last: None,
            held,
            stop,
            dead: false,
        }
    }

    #[test]
    fn a_dead_thread_never_leaves_a_round_waiting() {
        let signals = Signals::new(true);
        std::thread::scope(|s| {
            let waiting = s.spawn(|| signals.round_now());
            // Once the request is in, the thread dies (a panic in a round) without serving it.
            while lock(&signals.state).requested == 0 {
                std::thread::yield_now();
            }
            drop(Dead(&signals));
            let err = waiting.join().unwrap().unwrap_err();
            assert_eq!(err.code(), ErrorCode::Poisoned, "{err}");
        });
        assert_eq!(signals.round_now().unwrap_err().code(), ErrorCode::Poisoned);
    }

    #[test]
    fn the_schedule() {
        let now = Instant::now();
        let later = now + PERIOD;
        let idle = state(3, 0, 0, false, false);
        // No work: park, with no timer.
        assert_eq!(decide(&idle, false, 3, None, now), Decision::Park);
        // A write since the last round, or a round that left work: a round one period from now.
        assert_eq!(
            decide(&idle, false, 2, None, now),
            Decision::WaitUntil(later)
        );
        assert_eq!(
            decide(&idle, true, 3, None, now),
            Decision::WaitUntil(later)
        );
        // The wait keeps its deadline, and the round runs once it is reached.
        assert_eq!(
            decide(&idle, true, 3, Some(later), now),
            Decision::WaitUntil(later)
        );
        let timed = Decision::Round { requested: false };
        assert_eq!(decide(&idle, true, 3, Some(later), later), timed);
        assert_eq!(decide(&idle, true, 3, Some(now), later), timed);
        // Held: only a requested round runs, at once.
        let held = state(9, 0, 0, true, false);
        assert_eq!(decide(&held, true, 3, Some(now), later), Decision::Park);
        let asked = Decision::Round { requested: true };
        assert_eq!(
            decide(&state(9, 2, 1, true, false), false, 9, None, now),
            asked
        );
        assert_eq!(
            decide(&state(9, 2, 1, false, false), false, 9, None, now),
            asked
        );
        assert_eq!(
            decide(&state(9, 2, 2, false, false), false, 9, None, now),
            Decision::Park
        );
        // A stop wins over everything.
        assert_eq!(
            decide(&state(9, 2, 1, false, true), true, 3, None, now),
            Decision::Stop
        );
    }

    #[test]
    fn a_write_is_counted_and_a_stopped_thread_serves_no_round() {
        let signals = Signals::new(false);
        signals.wrote();
        signals.wrote();
        assert_eq!(lock(&signals.state).writes, 2);
        assert!(!lock(&signals.state).held);
        assert!(lock(&Signals::new(true).state).held);
        signals.stop();
        assert_eq!(signals.round_now().unwrap_err().code(), ErrorCode::Closed);
        assert_eq!(lock(&signals.state).requested, 0, "not even asked");
    }

    #[test]
    fn work_is_an_unflushed_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut db =
            Writer::create(dir.path().join("app.persia"), DatabaseOptions::default()).unwrap();
        assert!(!has_work(&db), "no collection");
        let schema = SchemaBuilder::new("c").build().unwrap();
        db.log_mut().create_collection(schema).unwrap();
        assert!(!has_work(&db), "an empty collection");
        db.put_document("c", &Document::new("a"), Durability::Buffered)
            .unwrap();
        assert!(has_work(&db));
        assert!(round(&mut db).unwrap(), "not due yet: still unflushed");
        db.commit().unwrap();
        assert!(!has_work(&db), "flushed");
        assert!(!round(&mut db).unwrap());
        // A merge in flight is work too: eight flushed segments, and the merge held in flight (ROADMAP 6.5.14).
        db.log_mut().hold_merges(true);
        for i in 0..7 {
            db.put_document("c", &Document::new(format!("d{i}")), Durability::Buffered)
                .unwrap();
            db.commit().unwrap();
        }
        db.log_mut().merge_step().unwrap();
        assert!(db.log().merges_running() > 0);
        assert!(has_work(&db), "a merge in flight");
        db.log_mut().cancel_merges();
        assert!(!has_work(&db));
    }
}
