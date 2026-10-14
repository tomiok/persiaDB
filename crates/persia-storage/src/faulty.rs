//! Deterministic failure injection: [`FaultyStorage`] wraps a backend and fails chosen calls (ROADMAP 2.2.2, 2.2.4).
//!
//! Faults are scripted ([`Trigger`]: the n-th call, or the n-th call of one operation) or drawn from a seeded
//! generator ([`RandomFaults`]), so a failing test is replayed from its seed. Every injected fault is logged
//! ([`FaultyStorage::injected`]) for the failure message.
//!
//! A [`Fault::Crash`] models process death: from that call on every call fails. In volatile mode
//! ([`FaultyStorage::set_volatile`]) acknowledged writes stay visible but unsynced until
//! [`sync`](FaultyStorage::sync), and a crash loses some of them ([`CrashLoss`]), as a dropped fsync would.

mod volatile;

use core::num::NonZeroU64;
use core::ops::Range;
use core::time::Duration;
use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use bytes::Bytes;
use persia_format::ByteSource;

use crate::error::{Error, Op, Result};
use crate::key::{Key, KeyPrefix};
use crate::meta::{Capabilities, ObjectMeta, PutOutcome};
use crate::retry::{Clock, Jitter, SystemClock};
use crate::storage::Storage;
pub use volatile::{CrashLoss, LostWrite};
use volatile::{Unsynced, Volatile};

/// What goes wrong in a faulted call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fault {
    /// Not applied; the call returns [`Error::Transient`] (e.g. throttling).
    Transient,
    /// Not applied; the call returns [`Error::Io`] (e.g. a full disk).
    Io,
    /// The call sleeps this long on the injected [`Clock`], then proceeds normally.
    Latency(Duration),
    /// `put` only: a strict prefix of the data (length drawn from the seed) is written, then the call returns
    /// [`Error::Io`]. This deliberately breaks the atomicity SPEC §10.1 promises, as a crash mid-write does to a
    /// store without atomic replace: it tests that readers detect the damage (checksums), not a backend behavior.
    /// An empty object is written whole. `put_if_absent` is atomic on every backend, so it is never torn.
    TornPut,
    /// `put` / `put_if_absent`: nothing is written, yet the call succeeds, as a write lost before it was made
    /// durable. `put_if_absent` still answers `AlreadyExists` for another writer's object, so fencing holds.
    DroppedPut,
    /// Applied; the response is lost, so the call returns [`Error::Transient`] (SPEC §10.1: a failed attempt may
    /// have been applied). An error from the inner backend is returned as is.
    ResponseLost,
    /// The process dies: this call and every later one fail with [`Error::Io`] without reaching the inner backend,
    /// until [`restart`](FaultyStorage::restart); in volatile mode the crash first loses unsynced writes. Logged
    /// once, for the call that crashed.
    Crash,
    /// `put` / `put_if_absent` / `delete`: the call is applied, then the process dies before answering, so the
    /// caller never learns the outcome (SPEC §10.4: a commit whose result is unknown). Fails like [`Crash`](Self::Crash),
    /// which it then is; in volatile mode the applied write may be among those the crash loses.
    CrashAfter,
}

impl Fault {
    /// Whether the fault can happen to `op`: [`TornPut`](Self::TornPut) only to `put`,
    /// [`DroppedPut`](Self::DroppedPut) to `put` and `put_if_absent`, [`CrashAfter`](Self::CrashAfter) to those and
    /// `delete`, the others to every operation.
    pub fn applies_to(self, op: Op) -> bool {
        match self {
            Self::TornPut => op == Op::Put,
            Self::DroppedPut => matches!(op, Op::Put | Op::PutIfAbsent),
            Self::CrashAfter => matches!(op, Op::Put | Op::PutIfAbsent | Op::Delete),
            Self::Transient | Self::Io | Self::Latency(_) | Self::ResponseLost | Self::Crash => {
                true
            }
        }
    }
}

/// Which call a scripted fault hits. Calls are counted from 1, in the order they reach the wrapper, including the
/// calls a crashed store fails; those fire no trigger, so a trigger that falls on one stays pending.
///
/// A fault whose trigger fires on a call it does not [apply](Fault::applies_to) to is discarded; one whose
/// trigger never fires stays [pending](FaultyStorage::pending). Tests assert `pending()` is empty to be sure every
/// scripted fault was injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// The n-th call of any operation.
    Call(u64),
    /// The n-th call of one operation.
    Nth(Op, u64),
}

/// Faults injected at random: each call is faulted with probability `1 / one_in`, with a fault picked uniformly
/// among those in `faults` that [apply](Fault::applies_to) to its operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RandomFaults {
    /// The inverse of the fault probability; `1` faults every call.
    pub one_in: NonZeroU64,
    /// The faults to choose from.
    pub faults: Vec<Fault>,
}

/// One injected fault, as logged by [`FaultyStorage::injected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    /// The call's number, from 1 (all operations).
    pub call: u64,
    /// The call's operation.
    pub op: Op,
    /// The key, or the prefix for [`Op::List`].
    pub target: String,
    /// The fault.
    pub fault: Fault,
    /// For [`Fault::TornPut`], how many bytes were written.
    pub kept: Option<usize>,
}

/// Everything a call's fault depends on, behind one lock: the generator is only advanced while the call is
/// counted, so the schedule is a function of the seed and the call order.
#[derive(Debug)]
struct State {
    calls: u64,
    per_op: [u64; 6],
    scripted: Vec<(Trigger, Fault)>,
    random: Option<RandomFaults>,
    rng: Jitter,
    log: Vec<Injection>,
    crashed: bool,
    volatile: Option<Volatile>,
    lost: Vec<LostWrite>,
}

impl State {
    fn draw(&self, op: Op) -> Option<Fault> {
        let random = self.random.as_ref()?;
        if self.rng.next_u64().checked_rem(random.one_in.get()) != Some(0) {
            return None;
        }
        let applicable: Vec<Fault> = random
            .faults
            .iter()
            .copied()
            .filter(|f| f.applies_to(op))
            .collect();
        let n = u64::try_from(applicable.len()).ok()?;
        let i = usize::try_from(self.rng.next_u64().checked_rem(n)?).ok()?;
        applicable.get(i).copied()
    }

    /// A strict prefix length of a `len`-byte write (0 for an empty one).
    fn torn_len(&self, len: usize) -> usize {
        let len = u64::try_from(len).unwrap_or(u64::MAX);
        let kept = self.rng.next_u64().checked_rem(len).unwrap_or(0);
        usize::try_from(kept).unwrap_or(0)
    }
}

/// A [`Storage`] that forwards to `inner` except for the calls chosen to fail.
///
/// The schedule is a function of the seed and the order of calls, so it is reproducible when the calls are
/// (e.g. one thread). Capabilities are the inner backend's. To reopen on what survived a faulted run, wrap an
/// `Arc` of the backend and keep a clone.
///
/// ```
/// use bytes::Bytes;
/// use persia_storage::faulty::{Fault, FaultyStorage, Trigger};
/// use persia_storage::{ErrorKind, Key, MemoryStorage, Op, Storage};
///
/// let storage = FaultyStorage::new(MemoryStorage::new(), 7);
/// storage.inject(Trigger::Nth(Op::Put, 2), Fault::ResponseLost);
/// let key = Key::new("k")?;
/// storage.put(&key, Bytes::from_static(b"one"))?;
/// let err = storage.put(&key, Bytes::from_static(b"two")).unwrap_err();
/// assert_eq!(err.kind(), ErrorKind::Transient);
/// assert_eq!(&storage.get(&key)?[..], b"two", "applied all the same");
/// assert_eq!(storage.injected().len(), 1);
/// # Ok::<(), persia_storage::Error>(())
/// ```
pub struct FaultyStorage<S> {
    inner: S,
    seed: u64,
    clock: Box<dyn Clock>,
    state: Mutex<State>,
    // Every call reaches `inner` under this lock and re-checks `crashed` there, so nothing lands after a crash.
    // Shared by reads and durable writes, so they still race in the inner backend; exclusive for a volatile write
    // (reading what it replaces, writing, reading what it left), a crash's undo, `sync`, `restart` and mode changes.
    // Taken before `state`.
    gate: RwLock<()>,
}

impl<S: Storage> FaultyStorage<S> {
    /// Wraps `inner` with no faults armed; `seed` drives random faults and torn-write lengths. Latency sleeps on
    /// the [`SystemClock`].
    pub fn new(inner: S, seed: u64) -> Self {
        Self {
            inner,
            seed,
            clock: Box::new(SystemClock),
            gate: RwLock::new(()),
            state: Mutex::new(State {
                calls: 0,
                per_op: [0; 6],
                scripted: Vec::new(),
                random: None,
                rng: Jitter::new(seed),
                log: Vec::new(),
                crashed: false,
                volatile: None,
                lost: Vec::new(),
            }),
        }
    }

    /// Sleeps [`Fault::Latency`] on `clock` instead (e.g. one that only records the delays).
    #[must_use]
    pub fn with_clock(mut self, clock: impl Clock + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Arms `fault` for the call `trigger` selects; it fires once. Scripted faults take precedence over random ones,
    /// and the first armed of two on the same call wins.
    pub fn inject(&self, trigger: Trigger, fault: Fault) {
        self.state().scripted.push((trigger, fault));
    }

    /// Scripted faults whose trigger has not fired yet.
    pub fn pending(&self) -> Vec<(Trigger, Fault)> {
        self.state().scripted.clone()
    }

    /// Faults calls at random from now on (`None` stops).
    pub fn set_random(&self, random: Option<RandomFaults>) {
        self.state().random = random;
    }

    /// Every fault injected so far, in call order.
    pub fn injected(&self) -> Vec<Injection> {
        self.state().log.clone()
    }

    /// Calls seen so far (all operations).
    pub fn calls(&self) -> u64 {
        self.state().calls
    }

    /// The seed, for failure messages.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The wrapped backend, to inspect or set up its state without faults.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// The wrapped backend, without the wrapper.
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// Crashes now, as if [`Fault::Crash`] hit the next call: in volatile mode unsynced writes are lost, then every
    /// call fails until [`restart`](Self::restart). Crashing a crashed store does nothing.
    ///
    /// A lost write is undone only if its object still holds what the write left: an object another writer has
    /// since replaced (a second wrapper or process on the same inner store) keeps that writer's bytes, since one
    /// process dying cannot undo another's write.
    ///
    /// # Errors
    /// The inner backend's error while undoing a lost write. The store is crashed all the same; the writes not
    /// undone stay unsynced (a later crash may lose them) and are not listed by [`lost`](Self::lost).
    pub fn crash(&self) -> Result<()> {
        let _gate = self.exclusive();
        let mut undo = {
            let mut state = self.state();
            if state.crashed {
                return Ok(());
            }
            state.crashed = true;
            let Some(volatile) = state.volatile.take() else {
                return Ok(());
            };
            let loss = volatile.loss;
            let undo = volatile.crash(&state.rng);
            state.volatile = Some(Volatile::new(loss));
            undo
        }
        .into_iter();
        while let Some(unsynced) = undo.next() {
            if let Err(err) = self.undo(&unsynced) {
                if let Some(volatile) = self.state().volatile.as_mut() {
                    // `undo` is newest first; the log is oldest first.
                    let mut left: Vec<Unsynced> = core::iter::once(unsynced).chain(undo).collect();
                    left.reverse();
                    volatile.writes.splice(0..0, left);
                }
                return Err(err);
            }
        }
        Ok(())
    }

    /// Restores what `unsynced` replaced, if its object still holds what it left; lists it as lost if so.
    fn undo(&self, unsynced: &Unsynced) -> Result<()> {
        let key = &unsynced.write.key;
        if self.read_back(key)? != unsynced.after {
            return Ok(());
        }
        match &unsynced.before {
            Some(data) => self.inner.put(key, data.clone())?,
            None => self.inner.delete(key)?,
        }
        self.state().lost.push(unsynced.write.clone());
        Ok(())
    }

    /// Whether the store has crashed (and not restarted).
    pub fn is_crashed(&self) -> bool {
        self.state().crashed
    }

    /// Comes back after a crash, on what survived: calls reach the inner backend again. Armed faults, call
    /// counters and the volatile mode are kept.
    pub fn restart(&self) {
        let _gate = self.exclusive();
        self.state().crashed = false;
    }

    /// Makes acknowledged writes volatile from now on (`Some`), so a crash loses some of them as `loss` says; or
    /// durable (`None`), which also makes every write so far durable. Changing the mode keeps the unsynced writes.
    ///
    /// SPEC §10.1 promises that an acknowledged write is durable, so volatile mode breaks the contract on purpose:
    /// a test under it must not expect an acknowledged commit to survive a crash unless it called
    /// [`sync`](Self::sync) after it.
    pub fn set_volatile(&self, loss: Option<CrashLoss>) {
        let _gate = self.exclusive();
        let mut state = self.state();
        state.volatile = match (loss, state.volatile.take()) {
            (None, _) => None,
            (Some(loss), Some(mut volatile)) => {
                volatile.loss = loss;
                Some(volatile)
            }
            (Some(loss), None) => Some(Volatile::new(loss)),
        };
    }

    /// Makes every write so far durable: a later crash cannot lose it (an fsync of everything).
    pub fn sync(&self) {
        let _gate = self.exclusive();
        if let Some(volatile) = self.state().volatile.as_mut() {
            volatile.writes.clear();
        }
    }

    /// Every write that crashes have undone, in the order undone (newest first per crash).
    pub fn lost(&self) -> Vec<LostWrite> {
        self.state().lost.clone()
    }

    // A panic while a lock is held (only in this module's bookkeeping) leaves state that is still usable.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn shared(&self) -> RwLockReadGuard<'_, ()> {
        self.gate.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn exclusive(&self) -> RwLockWriteGuard<'_, ()> {
        self.gate.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The object under `key`, or `None`, read past the wrapper.
    fn read_back(&self, key: &Key) -> Result<Option<Bytes>> {
        match self.inner.get(key) {
            Ok(data) => Ok(Some(data)),
            Err(Error::NotFound { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Counts the call and decides its fault; `len` is the data length of a write. Returns the call number.
    ///
    /// # Errors
    /// The crashed error if the store has crashed.
    fn decide(&self, op: Op, target: &str, len: usize) -> Result<(u64, Option<Injection>)> {
        let mut state = self.state();
        state.calls = state.calls.saturating_add(1);
        let call = state.calls;
        let slot = op_index(op);
        if let Some(n) = state.per_op.get_mut(slot) {
            *n = n.saturating_add(1);
        }
        if state.crashed {
            return Err(crashed(op, target, None));
        }
        let nth = state.per_op.get(slot).copied().unwrap_or(0);
        let hit = state
            .scripted
            .iter()
            .position(|(trigger, _)| match trigger {
                Trigger::Call(n) => *n == call,
                Trigger::Nth(o, n) => *o == op && *n == nth,
            });
        let fault = if let Some(i) = hit {
            let (_, fault) = state.scripted.remove(i);
            Some(fault).filter(|f| f.applies_to(op))
        } else {
            state.draw(op)
        };
        let Some(fault) = fault else {
            return Ok((call, None));
        };
        let kept = (fault == Fault::TornPut).then(|| state.torn_len(len));
        let injection = Injection {
            call,
            op,
            target: target.to_owned(),
            fault,
            kept,
        };
        state.log.push(injection.clone());
        Ok((call, Some(injection)))
    }

    fn sleep_or_fail(&self, injection: &Injection) -> Result<()> {
        match injection.fault {
            Fault::Latency(d) => {
                self.clock.sleep(d);
                Ok(())
            }
            Fault::Transient => Err(transient(injection)),
            Fault::Io => Err(Error::Io {
                op: injection.op,
                target: injection.target.clone(),
                source: io::Error::other("injected I/O fault"),
            }),
            Fault::Crash => Err(self.die(injection)),
            Fault::TornPut | Fault::DroppedPut | Fault::ResponseLost | Fault::CrashAfter => Ok(()),
        }
    }

    /// Crashes on behalf of `injection`'s call and returns its error: always the crashed one, never retryable,
    /// with a failed undo kept in the message.
    fn die(&self, injection: &Injection) -> Error {
        crashed(injection.op, &injection.target, self.crash().err())
    }

    /// Runs a read (`call`) unless the store crashed since the call was decided (e.g. while it was delayed).
    fn reach<T>(&self, op: Op, target: &str, call: impl FnOnce() -> Result<T>) -> Result<T> {
        let _gate = self.shared();
        if self.is_crashed() {
            return Err(crashed(op, target, None));
        }
        call()
    }

    /// Runs `write` of `key` unless the store crashed since the call was decided. In volatile mode it also records
    /// what the write replaced and what it left, so a crash can undo it; whatever its outcome, since a failed write
    /// (torn, or with a lost response) may have changed the object too. A write that changed nothing is not
    /// recorded.
    fn record<T>(
        &self,
        call: u64,
        op: Op,
        key: &Key,
        write: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if self.state().volatile.is_none() {
            let _gate = self.shared();
            // The mode cannot change while the gate is held; re-check it under the gate.
            if self.state().volatile.is_none() {
                if self.is_crashed() {
                    return Err(crashed(op, key.as_str(), None));
                }
                return write();
            }
        }
        let _gate = self.exclusive();
        if self.is_crashed() {
            return Err(crashed(op, key.as_str(), None));
        }
        let before = self
            .read_back(key)
            .map_err(|err| crashed_read(op, key, &err))?;
        let out = write();
        let after = self
            .read_back(key)
            .map_err(|err| crashed_read(op, key, &err))?;
        let mut state = self.state();
        match state.volatile.as_mut() {
            Some(volatile) if before != after => volatile.writes.push(Unsynced {
                write: LostWrite {
                    call,
                    op,
                    key: key.clone(),
                },
                before,
                after,
            }),
            // Turned durable meanwhile is impossible under the gate; an unchanged object has nothing to undo.
            _ => {}
        }
        out
    }

    /// Runs a call without data to tear or drop: `call` is skipped by a failure, delayed by latency, and its answer
    /// replaced by `Transient` if the response is lost. `written` names the object a `delete` changes.
    fn plain_op<T>(
        &self,
        op: Op,
        target: &str,
        written: Option<&Key>,
        call: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let (n, injection) = self.decide(op, target, 0)?;
        let call = || match written {
            Some(key) => self.record(n, op, key, call),
            None => self.reach(op, target, call),
        };
        let Some(injection) = injection else {
            return call();
        };
        self.sleep_or_fail(&injection)?;
        let out = call();
        if injection.fault == Fault::CrashAfter {
            return Err(self.die(&injection));
        }
        let out = out?;
        if injection.fault == Fault::ResponseLost {
            return Err(transient(&injection));
        }
        Ok(out)
    }

    /// Runs a write of `data`: `write` is called with the bytes to store (a prefix for a torn put); `dropped`
    /// answers a dropped write without writing.
    fn write_op<T>(
        &self,
        op: Op,
        key: &Key,
        data: Bytes,
        dropped: impl FnOnce(&Bytes) -> Result<T>,
        write: impl FnOnce(Bytes) -> Result<T>,
    ) -> Result<T> {
        let (n, injection) = self.decide(op, key.as_str(), data.len())?;
        let write = |data| self.record(n, op, key, || write(data));
        let Some(injection) = injection else {
            return write(data);
        };
        self.sleep_or_fail(&injection)?;
        match injection.fault {
            Fault::DroppedPut => self.reach(op, key.as_str(), || dropped(&data)),
            Fault::TornPut => {
                let kept = injection.kept.unwrap_or(0).min(data.len());
                write(data.slice(..kept))?;
                Err(Error::Io {
                    op,
                    target: injection.target,
                    source: io::Error::other("injected torn write"),
                })
            }
            Fault::ResponseLost => {
                write(data)?;
                Err(transient(&injection))
            }
            Fault::CrashAfter => {
                // Applied or not, the caller never learns: the process is gone.
                let _outcome = write(data);
                Err(self.die(&injection))
            }
            Fault::Transient | Fault::Io | Fault::Latency(_) | Fault::Crash => write(data),
        }
    }
}

/// What every call of a crashed store returns: an I/O error, never retried. `cause` is a failure while crashing.
fn crashed(op: Op, target: &str, cause: Option<Error>) -> Error {
    let message = match cause {
        None => "storage crashed (injected)".to_owned(),
        Some(err) => format!("storage crashed (injected); undoing lost writes failed: {err}"),
    };
    Error::Io {
        op,
        target: target.to_owned(),
        source: io::Error::other(message),
    }
}

/// A volatile write could not read its object back, so it cannot be recorded: reported as an I/O error of the
/// write itself, never retried.
fn crashed_read(op: Op, key: &Key, err: &Error) -> Error {
    Error::Io {
        op,
        target: key.as_str().to_owned(),
        source: io::Error::other(format!(
            "volatile write could not read its object back: {err}"
        )),
    }
}

fn transient(injection: &Injection) -> Error {
    Error::Transient {
        op: injection.op,
        target: injection.target.clone(),
        source: format!("injected {:?} at call {}", injection.fault, injection.call).into(),
    }
}

fn op_index(op: Op) -> usize {
    match op {
        Op::Get => 0,
        Op::GetRange => 1,
        Op::Put => 2,
        Op::PutIfAbsent => 3,
        Op::List => 4,
        Op::Delete => 5,
    }
}

impl<S: Storage> Storage for FaultyStorage<S> {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        self.plain_op(Op::GetRange, key.as_str(), None, || {
            self.inner.get_range(key, range)
        })
    }

    fn get(&self, key: &Key) -> Result<Bytes> {
        self.plain_op(Op::Get, key.as_str(), None, || self.inner.get(key))
    }

    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.write_op(
            Op::Put,
            key,
            data,
            |_| Ok(()),
            |data| self.inner.put(key, data),
        )
    }

    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        // A dropped write is only acknowledged as `Created` where the real call could have created the object;
        // another writer's object is still reported, so a dropped write never breaks fencing (SPEC §11.3).
        let dropped = |data: &Bytes| match self.inner.get(key) {
            Ok(existing) if existing == *data => Ok(PutOutcome::Created),
            Ok(_) => Ok(PutOutcome::AlreadyExists),
            Err(Error::NotFound { .. }) => Ok(PutOutcome::Created),
            Err(err) => Err(err),
        };
        self.write_op(Op::PutIfAbsent, key, data, dropped, |data| {
            self.inner.put_if_absent(key, data)
        })
    }

    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        self.plain_op(Op::List, prefix.as_str(), None, || self.inner.list(prefix))
    }

    fn delete(&self, key: &Key) -> Result<()> {
        self.plain_op(Op::Delete, key.as_str(), Some(key), || {
            self.inner.delete(key)
        })
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

impl<S: core::fmt::Debug> core::fmt::Debug for FaultyStorage<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FaultyStorage")
            .field("inner", &self.inner)
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}
