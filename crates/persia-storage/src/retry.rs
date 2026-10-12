//! Retrying transient failures: exponential backoff with full jitter (SPEC §10.1).
//!
//! Sleeping goes through a [`Clock`] and the jitter through a seeded [`Jitter`] generator, so tests replay the exact
//! schedule without waiting.

use core::fmt;
use core::time::Duration;
use std::hash::{BuildHasher, RandomState};

use crate::error::{Error, ErrorKind, Result};

impl Error {
    /// Whether repeating the operation may succeed: only [`ErrorKind::Transient`] errors (SPEC §10.1).
    pub fn is_retryable(&self) -> bool {
        self.kind() == ErrorKind::Transient
    }
}

/// Where a retry loop sleeps. Tests inject one that records the delays instead.
pub trait Clock: Send + Sync + fmt::Debug {
    /// Blocks the calling thread for `duration`.
    fn sleep(&self, duration: Duration);
}

/// The real clock: [`std::thread::sleep`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The random source for backoff jitter: `SplitMix64`, seeded, so a schedule is reproducible from its seed.
///
/// Not for anything security-relevant.
#[derive(Debug, Clone)]
pub struct Jitter {
    state: u64,
}

impl Jitter {
    /// A generator that always yields the same sequence for `seed`.
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// A generator seeded from the process's hash randomness, for production use (so that many clients do not
    /// retry in lockstep).
    pub fn from_entropy() -> Self {
        Self::new(RandomState::new().hash_one(0u8))
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        // SplitMix64 (Steele, Lea, Flood 2014); wrapping arithmetic is the algorithm.
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// How often and how long to retry [`ErrorKind::Transient`] errors (SPEC §10.1).
///
/// Before retry `n` (counted from 0) the caller waits a uniformly random duration in
/// `0..=min(max_delay, base_delay · 2ⁿ)` ("full jitter"), which spreads out clients that failed together.
///
/// ```
/// use std::time::Duration;
/// use persia_storage::retry::{Jitter, RetryPolicy, SystemClock};
/// use persia_storage::{Error, Op};
///
/// let policy = RetryPolicy { base_delay: Duration::ZERO, ..RetryPolicy::default() };
/// let mut calls = 0;
/// let out = policy.run(&SystemClock, &mut Jitter::new(1), || {
///     calls += 1;
///     if calls < 3 {
///         return Err(Error::Transient { op: Op::Get, target: "k".into(), source: "503".into() });
///     }
///     Ok(calls)
/// });
/// assert_eq!(out?, 3);
/// # Ok::<(), Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts in total, the first included; `0` is treated as `1` (no retries).
    pub max_attempts: u32,
    /// Upper bound of the first delay; doubles with every retry.
    pub base_delay: Duration,
    /// Upper bound of any delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    /// 5 attempts, 50 ms base, 5 s cap (SPEC §10.1).
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// Never retry.
    pub const NONE: Self = Self {
        max_attempts: 1,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    };

    /// The longest wait before retry `retry` (from 0): `min(max_delay, base_delay · 2^retry)`, at most
    /// `u64::MAX` nanoseconds.
    pub fn delay_cap(&self, retry: u32) -> Duration {
        let max = nanos(self.max_delay);
        let exp = 1u64.checked_shl(retry).unwrap_or(u64::MAX);
        Duration::from_nanos(nanos(self.base_delay).saturating_mul(exp).min(max))
    }

    /// The wait before retry `retry` (from 0) given 64 random bits: uniform in `0..=delay_cap(retry)`.
    pub fn delay(&self, retry: u32, random: u64) -> Duration {
        let cap = u128::from(nanos(self.delay_cap(retry)));
        // Multiply-shift maps `random` onto 0..=cap without modulo bias; the product fits: < 2^64 · 2^64.
        let scaled = u128::from(random)
            .saturating_mul(cap.saturating_add(1))
            .checked_shr(64)
            .unwrap_or(0);
        Duration::from_nanos(u64::try_from(scaled).unwrap_or(u64::MAX))
    }

    /// Runs `op` until it succeeds, fails with a non-retryable error, or `max_attempts` are used up; sleeps
    /// [`delay`](Self::delay) on `clock` between attempts.
    ///
    /// # Errors
    /// The first non-retryable error, or the last retryable one.
    pub fn run<T>(
        &self,
        clock: &dyn Clock,
        jitter: &mut Jitter,
        mut op: impl FnMut() -> Result<T>,
    ) -> Result<T> {
        let mut retry = 0u32;
        loop {
            match op() {
                Err(err) if err.is_retryable() && retry.saturating_add(1) < self.max_attempts => {
                    clock.sleep(self.delay(retry, jitter.next_u64()));
                    retry = retry.saturating_add(1);
                }
                out => return out,
            }
        }
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::error::Op;
    use crate::key::Key;

    #[derive(Debug, Default)]
    struct RecordingClock(Mutex<Vec<Duration>>);

    impl Clock for RecordingClock {
        fn sleep(&self, duration: Duration) {
            self.0.lock().unwrap().push(duration);
        }
    }

    fn transient() -> Error {
        Error::Transient {
            op: Op::Get,
            target: "k".into(),
            source: "503".into(),
        }
    }

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(250),
        }
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        assert!(transient().is_retryable());
        let not_found = Error::NotFound {
            key: Key::new("k").unwrap(),
        };
        assert!(!not_found.is_retryable());
        let io = Error::Io {
            op: Op::Put,
            target: "k".into(),
            source: std::io::Error::from(std::io::ErrorKind::TimedOut),
        };
        assert!(!io.is_retryable(), "backends classify; Io is never retried");
    }

    #[test]
    fn caps_double_up_to_the_maximum() {
        let p = policy();
        let caps: Vec<u64> = (0..4)
            .map(|n| u64::try_from(p.delay_cap(n).as_millis()).unwrap())
            .collect();
        assert_eq!(caps, [100, 200, 250, 250]);
        assert_eq!(p.delay_cap(u32::MAX), p.max_delay);
        let huge = RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::MAX,
            max_delay: Duration::MAX,
        };
        assert_eq!(huge.delay_cap(63), Duration::from_nanos(u64::MAX));
    }

    #[test]
    fn delay_spans_zero_to_the_cap() {
        let p = policy();
        assert_eq!(p.delay(1, 0), Duration::ZERO);
        assert_eq!(p.delay(1, u64::MAX), p.delay_cap(1));
        assert_eq!(p.delay(1, 1 << 63), p.delay_cap(1) / 2);
        assert_eq!(RetryPolicy::NONE.delay(3, u64::MAX), Duration::ZERO);
    }

    #[test]
    fn retries_transient_errors_with_the_seeded_schedule() {
        let clock = RecordingClock::default();
        let mut calls = 0;
        let out = policy().run(&clock, &mut Jitter::new(42), || {
            calls += 1;
            if calls < 3 {
                Err(transient())
            } else {
                Ok(calls)
            }
        });
        assert_eq!(out.unwrap(), 3);
        let mut jitter = Jitter::new(42);
        let expected: Vec<Duration> = (0..2)
            .map(|n| policy().delay(n, jitter.next_u64()))
            .collect();
        assert_eq!(*clock.0.lock().unwrap(), expected);
    }

    #[test]
    fn gives_up_after_max_attempts_with_the_last_error() {
        let clock = RecordingClock::default();
        let mut calls = 0;
        let out: Result<()> = policy().run(&clock, &mut Jitter::new(7), || {
            calls += 1;
            Err(transient())
        });
        assert!(out.unwrap_err().is_retryable());
        assert_eq!(calls, 4);
        let sleeps = clock.0.lock().unwrap().clone();
        assert_eq!(sleeps.len(), 3);
        for (n, d) in (0..).zip(sleeps) {
            assert!(d <= policy().delay_cap(n));
        }
    }

    #[test]
    fn non_retryable_errors_and_zero_attempts_stop_at_once() {
        for max_attempts in [0, 1, 5] {
            let clock = RecordingClock::default();
            let p = RetryPolicy {
                max_attempts,
                ..policy()
            };
            let mut calls = 0;
            let err = p
                .run(&clock, &mut Jitter::new(1), || -> Result<()> {
                    calls += 1;
                    if max_attempts == 5 {
                        Err(Error::Unsupported {
                            op: Op::PutIfAbsent,
                        })
                    } else {
                        Err(transient())
                    }
                })
                .unwrap_err();
            assert_eq!(calls, 1, "{err}");
            assert!(clock.0.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn jitter_is_reproducible_from_its_seed() {
        let a: Vec<u64> = {
            let mut j = Jitter::new(3);
            (0..4).map(|_| j.next_u64()).collect()
        };
        let mut j = Jitter::new(3);
        assert_eq!(a, (0..4).map(|_| j.next_u64()).collect::<Vec<_>>());
        // SplitMix64 reference value for seed 0.
        assert_eq!(Jitter::new(0).next_u64(), 0xe220_a839_7b1d_cdaf);
    }
}
