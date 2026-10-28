//! When the database commits on its own (SPEC §8.2 step 3, §14 `commit_interval`, ROADMAP 6.3.4).
//!
//! A pure decision, like [`FlushPolicy`]: the caller passes each memtable's size and the times of its first and last
//! unflushed writes, and the clock's `now`.

use std::time::{Duration, Instant};

use crate::memtable::{FlushPolicy, FlushReason};

/// When collections are flushed and committed without an explicit `commit()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitPolicy {
    /// Flush one memtable at a size, or once its writes stop for a while (SPEC §8.2).
    pub flush: FlushPolicy,
    /// Flush a memtable once this long has passed since its first unflushed write (SPEC §14 `commit_interval`):
    /// writes that never pause for [`FlushPolicy::idle`] and stay under its size are flushed too, which bounds the WAL
    /// an open replays.
    pub interval: Duration,
}

/// What [`CommitPolicy::due`] knows of one memtable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemtableState {
    /// [`crate::memtable::Memtable::size_in_bytes`].
    pub size_in_bytes: usize,
    /// When its oldest unflushed operation was applied; `None` when it is empty.
    pub first_write: Option<Instant>,
    /// When its newest operation was applied; `None` when it is empty.
    pub last_write: Option<Instant>,
}

impl CommitPolicy {
    /// 60 s (SPEC §14).
    pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

    /// Why each memtable is due for a flush at `now`, if it is. The memtable's own triggers ([`FlushPolicy::due`])
    /// come first. A clock that went back counts as no time passed.
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use persia_engine::db::{CommitPolicy, CommitReason, MemtableState};
    /// use persia_engine::memtable::FlushReason;
    ///
    /// let policy = CommitPolicy::default();
    /// let t = Instant::now();
    /// let later = t + Duration::from_secs(60);
    /// // First written at t and every 5 s since, so never idle; the interval still flushes it.
    /// let trickle = MemtableState {
    ///     size_in_bytes: 1 << 10,
    ///     first_write: Some(t),
    ///     last_write: Some(later - Duration::from_secs(5)),
    /// };
    /// let empty = MemtableState { size_in_bytes: 0, first_write: None, last_write: None };
    /// assert_eq!(policy.due([trickle, empty], later), [Some(CommitReason::Interval), None]);
    /// let full = MemtableState { size_in_bytes: 64 << 20, first_write: Some(t), last_write: Some(t) };
    /// assert_eq!(policy.due([full], t), [Some(CommitReason::Flush(FlushReason::Size))]);
    /// ```
    pub fn due(
        &self,
        memtables: impl IntoIterator<Item = MemtableState>,
        now: Instant,
    ) -> Vec<Option<CommitReason>> {
        memtables
            .into_iter()
            .map(|m| {
                let own = self
                    .flush
                    .due(m.size_in_bytes, m.last_write, now)
                    .map(CommitReason::Flush);
                own.or_else(|| {
                    let first = m.first_write?;
                    (now.saturating_duration_since(first) >= self.interval)
                        .then_some(CommitReason::Interval)
                })
            })
            .collect()
    }
}

impl Default for CommitPolicy {
    fn default() -> Self {
        Self {
            flush: FlushPolicy::default(),
            interval: Self::DEFAULT_INTERVAL,
        }
    }
}

/// Why [`CommitPolicy::due`] flushes a memtable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitReason {
    /// The memtable's own trigger: its size or its idle time.
    Flush(FlushReason),
    /// [`CommitPolicy::interval`] passed since the memtable's first unflushed write.
    Interval,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table() {
        let policy = CommitPolicy {
            flush: FlushPolicy {
                memtable_bytes: 100,
                idle: Duration::from_secs(10),
            },
            interval: Duration::from_secs(30),
        };
        let t = Instant::now();
        let s = Duration::from_secs;
        let size = Some(CommitReason::Flush(FlushReason::Size));
        let idle = Some(CommitReason::Flush(FlushReason::Idle));
        let interval = Some(CommitReason::Interval);
        let cases = [
            // (size, first write, last write, now, due)
            (0, None, None, t + s(100), None),
            (10, Some(t), Some(t), t + s(9), None),
            (10, Some(t), Some(t), t + s(10), idle),
            (100, Some(t), Some(t), t, size),
            (100, Some(t), Some(t + s(29)), t + s(30), size),
            (10, Some(t), Some(t + s(25)), t + s(29), None),
            (10, Some(t), Some(t + s(25)), t + s(30), interval),
            (10, Some(t + s(5)), Some(t + s(25)), t + s(30), None),
            // A clock that went back.
            (10, Some(t + s(40)), Some(t + s(40)), t + s(30), None),
        ];
        for (bytes, first_write, last_write, now, due) in cases {
            let m = MemtableState {
                size_in_bytes: bytes,
                first_write,
                last_write,
            };
            assert_eq!(policy.due([m], now), [due], "{m:?} {now:?}");
        }
        assert_eq!(CommitPolicy::default().interval, s(60));
    }
}
