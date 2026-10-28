//! Time and segment ids, injected so tests and golden files are deterministic.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Where the database reads the time: [`SystemClock`], or [`ManualClock`] in tests.
pub trait Clock: Send + Sync + fmt::Debug {
    /// A monotonic instant, for the commit policy (ROADMAP 6.3.4).
    fn now(&self) -> Instant;

    /// Microseconds since the Unix epoch, UTC: a new segment's `created_at` (SPEC §5.1).
    fn unix_micros(&self) -> i64;
}

/// The system's clocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn unix_micros(&self) -> i64 {
        // Before 1970 or after year 294 247: neither happens; 0 is as good as any value then.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_micros()).ok())
            .unwrap_or(0)
    }
}

/// A clock that moves only when told: [`ManualClock::advance`].
///
/// ```
/// use std::time::Duration;
/// use persia_engine::db::{Clock, ManualClock};
///
/// let clock = ManualClock::new(1_000);
/// let t = clock.now();
/// clock.advance(Duration::from_millis(3));
/// assert_eq!(clock.now() - t, Duration::from_millis(3));
/// assert_eq!(clock.unix_micros(), 4_000);
/// ```
#[derive(Debug)]
pub struct ManualClock {
    start: Instant,
    unix_start: i64,
    elapsed: Mutex<Duration>,
}

impl ManualClock {
    /// A clock that reads `unix_micros` as its wall time until it is advanced.
    pub fn new(unix_micros: i64) -> Self {
        Self {
            start: Instant::now(),
            unix_start: unix_micros,
            elapsed: Mutex::new(Duration::ZERO),
        }
    }

    /// Moves both clocks forward by `by`.
    pub fn advance(&self, by: Duration) {
        let mut elapsed = self.elapsed.lock().unwrap_or_else(PoisonError::into_inner);
        *elapsed = elapsed.saturating_add(by);
    }

    fn elapsed(&self) -> Duration {
        *self.elapsed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        // An `Instant` this far ahead does not exist only after centuries of `advance`.
        self.start.checked_add(self.elapsed()).unwrap_or(self.start)
    }

    fn unix_micros(&self) -> i64 {
        let micros = i64::try_from(self.elapsed().as_micros()).unwrap_or(i64::MAX);
        self.unix_start.saturating_add(micros)
    }
}

/// Where new segment ids come from (SPEC §8.7.1: drawn at random, never reused).
#[derive(Debug)]
pub struct SegmentIds(Source);

#[derive(Debug)]
enum Source {
    /// The standard library's keyed hasher (`RandomState`, seeded from the operating system's randomness; `SipHash`
    /// today, unspecified by std) of a counter: a keyed PRF, so ids are unique with overwhelming probability
    /// (SPEC §8.7.1). Unpredictability is not required.
    Random { keys: RandomState, drawn: u64 },
    /// `SplitMix64`: reproducible, for tests and golden files.
    Seeded(u64),
}

impl SegmentIds {
    /// Unpredictable 128-bit ids.
    pub fn random() -> Self {
        Self(Source::Random {
            keys: RandomState::new(),
            drawn: 0,
        })
    }

    /// The same sequence of ids for the same `seed`. Ids of two databases seeded alike collide: for tests only.
    pub fn seeded(seed: u64) -> Self {
        Self(Source::Seeded(seed))
    }

    /// The next id.
    pub fn next_id(&mut self) -> u128 {
        let mut half = || match &mut self.0 {
            Source::Random { keys, drawn } => {
                *drawn = drawn.wrapping_add(1);
                let mut h = keys.build_hasher();
                h.write_u64(*drawn);
                h.finish()
            }
            Source::Seeded(state) => {
                *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = *state;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                z ^ (z >> 31)
            }
        };
        let lo = half();
        let hi = half();
        (u128::from(hi) << 64) | u128::from(lo)
    }
}

impl Default for SegmentIds {
    fn default() -> Self {
        Self::random()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let draw = |mut ids: SegmentIds| (0..100).map(|_| ids.next_id()).collect::<Vec<_>>();
        assert_eq!(draw(SegmentIds::seeded(5)), draw(SegmentIds::seeded(5)));
        assert_ne!(draw(SegmentIds::seeded(5)), draw(SegmentIds::seeded(6)));
        let random = draw(SegmentIds::random());
        let mut unique = random.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), random.len());
        assert_ne!(random, draw(SegmentIds::random()));
    }
}
