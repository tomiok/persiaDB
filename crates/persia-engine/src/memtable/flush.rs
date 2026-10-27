//! When a memtable is flushed into a segment (SPEC §8.2 step 3, ROADMAP 6.1.4).
//!
//! A pure decision over the memtable's size and the time of its last write; the caller passes the clock's `now`, so
//! tests need no sleeps and the engine one clock.

use std::time::{Duration, Instant};

/// The thresholds of SPEC §8.2: flush at `memtable_bytes`, or once writes have stopped for `idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushPolicy {
    /// Flush when [`super::Memtable::size_in_bytes`] reaches this (SPEC §14 `memtable_bytes`).
    pub memtable_bytes: usize,
    /// Flush a memtable that is not empty once no write came for this long.
    pub idle: Duration,
}

impl FlushPolicy {
    /// 64 MiB (SPEC §8.2).
    pub const DEFAULT_MEMTABLE_BYTES: usize = 64 << 20;
    /// 10 s (SPEC §8.2).
    pub const DEFAULT_IDLE: Duration = Duration::from_secs(10);

    /// Why a memtable of `size_in_bytes`, last written at `last_write` (`None` when it is empty), is due for a
    /// flush at `now`; `None` if it is not. Size is checked first. A `now` before `last_write` (a clock that went
    /// back) counts as no time passed.
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use persia_engine::memtable::{FlushPolicy, FlushReason};
    ///
    /// let policy = FlushPolicy::default();
    /// let t = Instant::now();
    /// assert_eq!(policy.due(1 << 10, Some(t), t + Duration::from_secs(1)), None);
    /// assert_eq!(policy.due(1 << 10, Some(t), t + Duration::from_secs(10)), Some(FlushReason::Idle));
    /// assert_eq!(policy.due(64 << 20, Some(t), t), Some(FlushReason::Size));
    /// assert_eq!(policy.due(0, None, t + Duration::from_secs(60)), None);
    /// ```
    pub fn due(
        &self,
        size_in_bytes: usize,
        last_write: Option<Instant>,
        now: Instant,
    ) -> Option<FlushReason> {
        let last = last_write?;
        if size_in_bytes >= self.memtable_bytes {
            return Some(FlushReason::Size);
        }
        (now.saturating_duration_since(last) >= self.idle).then_some(FlushReason::Idle)
    }
}

impl Default for FlushPolicy {
    fn default() -> Self {
        Self {
            memtable_bytes: Self::DEFAULT_MEMTABLE_BYTES,
            idle: Self::DEFAULT_IDLE,
        }
    }
}

/// Why [`FlushPolicy::due`] asks for a flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushReason {
    /// The memtable reached [`FlushPolicy::memtable_bytes`].
    Size,
    /// No write came for [`FlushPolicy::idle`].
    Idle,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds() {
        let policy = FlushPolicy {
            memtable_bytes: 100,
            idle: Duration::from_secs(10),
        };
        let t = Instant::now();
        let s = Duration::from_secs;
        let cases = [
            // (size, last write, now, due)
            (0, None, t, None),
            (1000, None, t + s(100), None),
            (99, Some(t), t, None),
            (100, Some(t), t, Some(FlushReason::Size)),
            (99, Some(t), t + s(9), None),
            (99, Some(t), t + s(10), Some(FlushReason::Idle)),
            (100, Some(t), t + s(10), Some(FlushReason::Size)),
            (99, Some(t + s(5)), t, None),
        ];
        for (size, last, now, due) in cases {
            assert_eq!(policy.due(size, last, now), due, "{size} {last:?} {now:?}");
        }
        assert_eq!(
            FlushPolicy::default(),
            FlushPolicy {
                memtable_bytes: 64 * 1024 * 1024,
                idle: s(10)
            }
        );
    }
}
