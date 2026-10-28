//! When the database commits on its own (SPEC §8.2 step 3, §14 `commit_interval`, ROADMAP 6.3.4).
//!
//! A pure decision, like [`FlushPolicy`]: the caller passes each memtable's size and last write, the time of the last
//! commit, and the clock's `now`.

use std::time::{Duration, Instant};

use crate::memtable::{FlushPolicy, FlushReason};

/// When collections are flushed and committed without an explicit `commit()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitPolicy {
    /// Flush one memtable at a size, or once its writes stop for a while (SPEC §8.2).
    pub flush: FlushPolicy,
    /// Flush every non-empty memtable once this long has passed since the last commit (SPEC §14 `commit_interval`):
    /// writes that never pause for [`FlushPolicy::idle`] and stay under its size are flushed too, which bounds the WAL
    /// an open replays.
    pub interval: Duration,
}

impl CommitPolicy {
    /// 60 s (SPEC §14).
    pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

    /// Why each memtable, given as its size in bytes and the time of its last write (`None` when it is empty), is
    /// due for a flush at `now`, if it is; `last_commit` is when the database last committed. The memtable's own
    /// triggers ([`FlushPolicy::due`]) come first. A clock that went back counts as no time passed.
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use persia_engine::db::{CommitPolicy, CommitReason};
    /// use persia_engine::memtable::FlushReason;
    ///
    /// let policy = CommitPolicy::default();
    /// let t = Instant::now();
    /// let later = t + Duration::from_secs(61);
    /// // Written to every 5 s, so never idle; the interval still flushes it, and leaves the empty one alone.
    /// let memtables = [(1 << 10, Some(later - Duration::from_secs(5))), (0, None)];
    /// assert_eq!(policy.due(memtables, t, later), [Some(CommitReason::Interval), None]);
    /// assert_eq!(
    ///     policy.due([(64 << 20, Some(t))], t, t),
    ///     [Some(CommitReason::Flush(FlushReason::Size))],
    /// );
    /// ```
    pub fn due(
        &self,
        memtables: impl IntoIterator<Item = (usize, Option<Instant>)>,
        last_commit: Instant,
        now: Instant,
    ) -> Vec<Option<CommitReason>> {
        let interval = now.saturating_duration_since(last_commit) >= self.interval;
        memtables
            .into_iter()
            .map(|(size, last_write)| {
                let own = self
                    .flush
                    .due(size, last_write, now)
                    .map(CommitReason::Flush);
                own.or_else(|| (interval && last_write.is_some()).then_some(CommitReason::Interval))
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
    /// [`CommitPolicy::interval`] passed since the last commit.
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
            // (size, last write, last commit, now, due)
            (0, None, t, t + s(100), None),
            (10, Some(t), t, t + s(9), None),
            (10, Some(t), t, t + s(10), idle),
            (100, Some(t), t, t, size),
            (100, Some(t + s(29)), t, t + s(30), size),
            (10, Some(t + s(25)), t, t + s(29), None),
            (10, Some(t + s(25)), t, t + s(30), interval),
            (10, Some(t + s(25)), t + s(5), t + s(30), None),
            // A clock that went back.
            (10, Some(t + s(25)), t + s(40), t + s(30), None),
        ];
        for (bytes, last_write, last_commit, now, due) in cases {
            assert_eq!(
                policy.due([(bytes, last_write)], last_commit, now),
                [due],
                "{bytes} {last_write:?} {last_commit:?} {now:?}"
            );
        }
        assert_eq!(CommitPolicy::default().interval, s(60));
    }
}
