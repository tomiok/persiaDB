//! Time for `put_blob`'s bind retries (SPEC §9.4, ADR-0051 item 3): an unknown binding outcome is retried with
//! jittered exponential backoff until a time budget runs out. Time is read and waited through a [`RetryClock`], so
//! tests run the whole budget without waiting.

use std::fmt::Debug;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The first wait between two bind attempts, before jitter.
pub const FIRST_BACKOFF: Duration = Duration::from_millis(50);

/// The longest wait between two bind attempts, before jitter.
pub const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Where bind retries read the time and wait.
pub trait RetryClock: Send + Sync + Debug {
    /// Time elapsed since an origin of the clock's choosing; never goes back.
    fn now(&self) -> Duration;

    /// Waits `duration`.
    fn sleep(&self, duration: Duration);
}

/// The monotonic system clock and `thread::sleep`.
#[derive(Debug, Clone, Copy)]
pub struct SystemRetryClock {
    origin: Instant,
}

impl SystemRetryClock {
    /// A clock whose origin is now.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemRetryClock {
    fn default() -> Self {
        Self::new()
    }
}

impl RetryClock for SystemRetryClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// A clock that only moves when it is slept on, recording each wait: for deterministic tests.
///
/// ```
/// use std::time::Duration;
/// use persia::internal::retry::{ManualRetryClock, RetryClock};
///
/// let clock = ManualRetryClock::new();
/// clock.sleep(Duration::from_millis(5));
/// assert_eq!(clock.now(), Duration::from_millis(5));
/// assert_eq!(clock.sleeps(), [Duration::from_millis(5)]);
/// ```
#[derive(Debug, Default)]
pub struct ManualRetryClock {
    sleeps: Mutex<Vec<Duration>>,
}

impl ManualRetryClock {
    /// A clock at its origin.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every wait so far, in order.
    pub fn sleeps(&self) -> Vec<Duration> {
        self.sleeps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl RetryClock for ManualRetryClock {
    fn now(&self) -> Duration {
        self.sleeps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .fold(Duration::ZERO, |sum, d| sum.saturating_add(*d))
    }

    fn sleep(&self, duration: Duration) {
        self.sleeps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(duration);
    }
}

/// The wait before retry `attempt` (from 0): `FIRST_BACKOFF · 2^attempt` capped at [`MAX_BACKOFF`], of which a random
/// half is kept ("equal jitter"), so writers that failed together do not retry together. `seed` picks the jitter
/// (the upload id): the same upload and attempt always wait the same.
pub fn backoff(seed: u128, attempt: u32) -> Duration {
    let cap = u64::try_from(MAX_BACKOFF.as_micros()).unwrap_or(u64::MAX);
    let first = u64::try_from(FIRST_BACKOFF.as_micros()).unwrap_or(cap);
    let full = first.checked_shl(attempt.min(32)).unwrap_or(cap).min(cap);
    let half = full / 2;
    let jitter = splitmix(seed, attempt) % half.saturating_add(1);
    Duration::from_micros(half.saturating_add(jitter))
}

/// `SplitMix64` of the seed's halves and the attempt: well spread, not secret.
fn splitmix(seed: u128, attempt: u32) -> u64 {
    let low = u64::try_from(seed & u128::from(u64::MAX)).unwrap_or(0);
    let high = u64::try_from(seed >> 64).unwrap_or(0);
    let mut z = low ^ high.rotate_left(17) ^ u64::from(attempt).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_its_cap_with_at_most_half_of_jitter() {
        for seed in [0, 1, u128::MAX, 0x1234_5678_9abc_def0] {
            for attempt in 0..40 {
                let full = FIRST_BACKOFF
                    .saturating_mul(1 << attempt.min(20))
                    .min(MAX_BACKOFF);
                let wait = backoff(seed, attempt);
                assert!(
                    wait >= full / 2 && wait <= full,
                    "{seed} {attempt}: {wait:?}"
                );
                assert_eq!(wait, backoff(seed, attempt));
            }
        }
        assert_ne!(backoff(1, 3), backoff(2, 3));
    }
}
