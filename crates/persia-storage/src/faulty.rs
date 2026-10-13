//! Deterministic failure injection: [`FaultyStorage`] wraps a backend and fails chosen calls (ROADMAP 2.2.2).
//!
//! Faults are scripted ([`Trigger`]: the n-th call, or the n-th call of one operation) or drawn from a seeded
//! generator ([`RandomFaults`]), so a failing test is replayed from its seed. Every injected fault is logged
//! ([`FaultyStorage::injected`]) for the failure message.

use core::ops::Range;
use core::time::Duration;
use std::io;
use std::sync::{Mutex, PoisonError};

use bytes::Bytes;
use persia_format::ByteSource;

use crate::error::{Error, Op, Result};
use crate::key::{Key, KeyPrefix};
use crate::meta::{Capabilities, ObjectMeta, PutOutcome};
use crate::retry::{Clock, Jitter, SystemClock};
use crate::storage::Storage;

/// What goes wrong in a faulted call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fault {
    /// Not applied; the call returns [`Error::Transient`] (e.g. throttling).
    Transient,
    /// Not applied; the call returns [`Error::Io`] (e.g. a full disk).
    Io,
    /// The call sleeps this long on the injected [`Clock`], then proceeds normally.
    Latency(Duration),
    /// `put` / `put_if_absent` only: a strict prefix of the data (length drawn from the seed) is written, then the
    /// call returns [`Error::Io`], as a crash mid-write leaves a non-atomic store. An empty object is written whole.
    TornPut,
    /// `put` / `put_if_absent` only: nothing is written, yet the call succeeds (`Created`), as a write lost before
    /// it was made durable.
    DroppedPut,
    /// Applied; the response is lost, so the call returns [`Error::Transient`] (SPEC §10.1: a failed attempt may
    /// have been applied). An error from the inner backend is returned as is.
    ResponseLost,
}

impl Fault {
    /// Whether the fault can happen to `op`: write faults only to `put` and `put_if_absent`.
    pub fn applies_to(self, op: Op) -> bool {
        match self {
            Self::TornPut | Self::DroppedPut => matches!(op, Op::Put | Op::PutIfAbsent),
            Self::Transient | Self::Io | Self::Latency(_) | Self::ResponseLost => true,
        }
    }
}

/// Which call a scripted fault hits. Calls are counted from 1, in the order they reach the wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// The n-th call of any operation. A fault that does not [apply](Fault::applies_to) to that call's operation is
    /// discarded.
    Call(u64),
    /// The n-th call of one operation.
    Nth(Op, u64),
}

/// Faults injected at random: each call is faulted with probability `1 / one_in`, with a fault picked uniformly
/// among those in `faults` that [apply](Fault::applies_to) to its operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RandomFaults {
    /// The inverse of the fault probability; `0` and `1` fault every call.
    pub one_in: u64,
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

#[derive(Debug, Default)]
struct State {
    calls: u64,
    per_op: [u64; 6],
    scripted: Vec<(Trigger, Fault)>,
    random: Option<RandomFaults>,
    log: Vec<Injection>,
}

/// A [`Storage`] that forwards to `inner` except for the calls chosen to fail.
///
/// The schedule is a function of the seed and the order of calls, so it is reproducible when the calls are
/// (e.g. one thread). Capabilities are the inner backend's.
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
    rng: Jitter,
    clock: Box<dyn Clock>,
    state: Mutex<State>,
}

impl<S: Storage> FaultyStorage<S> {
    /// Wraps `inner` with no faults armed; `seed` drives random faults and torn-write lengths. Latency sleeps on
    /// the [`SystemClock`].
    pub fn new(inner: S, seed: u64) -> Self {
        Self {
            inner,
            seed,
            rng: Jitter::new(seed),
            clock: Box::new(SystemClock),
            state: Mutex::default(),
        }
    }

    /// Sleeps [`Fault::Latency`] on `clock` instead (e.g. one that only records the delays).
    #[must_use]
    pub fn with_clock(mut self, clock: impl Clock + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Arms `fault` for the call `trigger` selects; it fires once. Scripted faults take precedence over random ones.
    pub fn inject(&self, trigger: Trigger, fault: Fault) {
        self.state().scripted.push((trigger, fault));
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

    // A panic while the lock is held (only in this module's bookkeeping) leaves counters that are still usable.
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts the call and decides its fault; `len` is the data length of a write.
    fn decide(&self, op: Op, target: &str, len: usize) -> Option<Injection> {
        let mut state = self.state();
        state.calls = state.calls.saturating_add(1);
        let call = state.calls;
        let slot = op_index(op);
        if let Some(n) = state.per_op.get_mut(slot) {
            *n = n.saturating_add(1);
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
            let (_, fault) = state.scripted.swap_remove(i);
            Some(fault).filter(|f| f.applies_to(op))
        } else {
            state
                .random
                .as_ref()
                .and_then(|random| self.draw(random, op))
        }?;
        let kept = (fault == Fault::TornPut).then(|| self.torn_len(len));
        let injection = Injection {
            call,
            op,
            target: target.to_owned(),
            fault,
            kept,
        };
        state.log.push(injection.clone());
        Some(injection)
    }

    fn draw(&self, random: &RandomFaults, op: Op) -> Option<Fault> {
        if self.rng.next_u64().checked_rem(random.one_in).unwrap_or(0) != 0 {
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
            Fault::TornPut | Fault::DroppedPut | Fault::ResponseLost => Ok(()),
        }
    }

    /// Runs a call without data to tear or drop: `call` is skipped by a failure, delayed by latency, and its answer replaced
    /// by `Transient` if the response is lost.
    fn plain_op<T>(&self, op: Op, target: &str, call: impl FnOnce() -> Result<T>) -> Result<T> {
        let Some(injection) = self.decide(op, target, 0) else {
            return call();
        };
        self.sleep_or_fail(&injection)?;
        let out = call()?;
        if injection.fault == Fault::ResponseLost {
            return Err(transient(&injection));
        }
        Ok(out)
    }

    /// Runs a write of `data`: `write` is called with the bytes to store (a prefix for a torn put).
    fn write_op<T>(
        &self,
        op: Op,
        key: &Key,
        data: Bytes,
        acked: T,
        write: impl FnOnce(Bytes) -> Result<T>,
    ) -> Result<T> {
        let Some(injection) = self.decide(op, key.as_str(), data.len()) else {
            return write(data);
        };
        self.sleep_or_fail(&injection)?;
        match injection.fault {
            Fault::DroppedPut => Ok(acked),
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
            Fault::Transient | Fault::Io | Fault::Latency(_) => write(data),
        }
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
        self.plain_op(Op::GetRange, key.as_str(), || {
            self.inner.get_range(key, range)
        })
    }

    fn get(&self, key: &Key) -> Result<Bytes> {
        self.plain_op(Op::Get, key.as_str(), || self.inner.get(key))
    }

    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        self.write_op(Op::Put, key, data, (), |data| self.inner.put(key, data))
    }

    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        self.write_op(Op::PutIfAbsent, key, data, PutOutcome::Created, |data| {
            self.inner.put_if_absent(key, data)
        })
    }

    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        self.plain_op(Op::List, prefix.as_str(), || self.inner.list(prefix))
    }

    fn delete(&self, key: &Key) -> Result<()> {
        self.plain_op(Op::Delete, key.as_str(), || self.inner.delete(key))
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
