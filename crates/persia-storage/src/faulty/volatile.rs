//! Volatile-until-crash writes (ROADMAP 2.2.4.2): the bookkeeping that decides what a crash loses.

use std::collections::BTreeMap;

use bytes::Bytes;

use crate::error::Op;
use crate::key::Key;
use crate::retry::Jitter;

/// Which unsynced writes survive a crash of a [`FaultyStorage`](super::FaultyStorage) in volatile mode; the rest are
/// lost, i.e. reverted. Writes made before the last [`sync`](super::FaultyStorage::sync) always survive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrashLoss {
    /// A prefix of the unsynced writes survives, its length drawn from the seed: the store is left as it was at
    /// some moment since the last sync, as with a write-back cache flushed in order. Software that orders its
    /// writes correctly (data before the commit record, SPEC §8) recovers a consistent state from it.
    Suffix,
    /// Each key keeps a prefix of its own unsynced writes, of a length drawn independently per key: writes to
    /// different keys survive out of order, as files without a barrier between their fsyncs. Expect a commit
    /// record whose data is gone; tests use it to check that damage is detected (`Corrupt`), never a panic.
    PerKey,
}

/// A write that a crash undid, as listed by [`FaultyStorage::lost`](super::FaultyStorage::lost).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostWrite {
    /// The write's call number, from 1 (all operations).
    pub call: u64,
    /// `put`, `put_if_absent` or `delete`.
    pub op: Op,
    /// The object written.
    pub key: Key,
}

/// An acknowledged write not yet synced, with what it replaced so a crash can undo it.
#[derive(Debug)]
pub(super) struct Unsynced {
    pub(super) write: LostWrite,
    /// The object before the write (`None`: absent).
    pub(super) before: Option<Bytes>,
}

/// The unsynced writes of a store in volatile mode, oldest first.
#[derive(Debug)]
pub(super) struct Volatile {
    pub(super) loss: CrashLoss,
    pub(super) writes: Vec<Unsynced>,
}

impl Volatile {
    pub(super) fn new(loss: CrashLoss) -> Self {
        Self {
            loss,
            writes: Vec::new(),
        }
    }

    /// Draws which writes a crash loses and returns them newest first: undoing them in that order (restoring each
    /// `before`) leaves every key as it was after the writes that survive, since the lost writes of a key are always
    /// the newest of that key.
    pub(super) fn crash(self, rng: &Jitter) -> Vec<Unsynced> {
        let lost = match self.loss {
            CrashLoss::Suffix => {
                let keep = draw_upto(rng, self.writes.len());
                (0..self.writes.len()).map(|i| i >= keep).collect()
            }
            CrashLoss::PerKey => self.per_key_lost(rng),
        };
        let mut undo: Vec<Unsynced> = self
            .writes
            .into_iter()
            .zip(lost)
            .filter_map(|(write, lost)| lost.then_some(write))
            .collect();
        undo.reverse();
        undo
    }

    fn per_key_lost(&self, rng: &Jitter) -> Vec<bool> {
        let mut per_key: BTreeMap<&Key, usize> = BTreeMap::new();
        for w in &self.writes {
            let n = per_key.entry(&w.write.key).or_insert(0);
            *n = n.saturating_add(1);
        }
        // Keys in byte order, so the draws do not depend on hashing.
        let keep: BTreeMap<&Key, usize> = per_key
            .into_iter()
            .map(|(key, n)| (key, draw_upto(rng, n)))
            .collect();
        let mut seen: BTreeMap<&Key, usize> = BTreeMap::new();
        self.writes
            .iter()
            .map(|w| {
                let i = seen.entry(&w.write.key).or_insert(0);
                let lost = *i >= keep.get(&w.write.key).copied().unwrap_or(0);
                *i = i.saturating_add(1);
                lost
            })
            .collect()
    }
}

/// Uniform in `0..=n`.
fn draw_upto(rng: &Jitter, n: usize) -> usize {
    let bound = u64::try_from(n).unwrap_or(u64::MAX).saturating_add(1);
    let drawn = rng.next_u64().checked_rem(bound).unwrap_or(0);
    usize::try_from(drawn).unwrap_or(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volatile(loss: CrashLoss, keys: &[&str]) -> Volatile {
        let mut v = Volatile::new(loss);
        for (call, k) in (1..).zip(keys) {
            v.writes.push(Unsynced {
                write: LostWrite {
                    call,
                    op: Op::Put,
                    key: Key::new(*k).unwrap(),
                },
                before: None,
            });
        }
        v
    }

    fn lost_calls(undo: &[Unsynced]) -> Vec<u64> {
        undo.iter().map(|u| u.write.call).collect()
    }

    #[test]
    fn suffix_loses_the_newest_writes_newest_first() {
        let mut lengths = std::collections::BTreeSet::new();
        for seed in 0..200 {
            let undo = volatile(CrashLoss::Suffix, &["a", "b", "a", "c"]).crash(&Jitter::new(seed));
            let calls = lost_calls(&undo);
            let n = u64::try_from(calls.len()).unwrap();
            assert_eq!(calls, (5 - n..5).rev().collect::<Vec<_>>(), "seed {seed}");
            lengths.insert(calls.len());
        }
        assert_eq!(
            lengths.len(),
            5,
            "every prefix 0..=4 survives for some seed"
        );
    }

    #[test]
    fn per_key_loses_a_suffix_of_each_key() {
        let mut outcomes = std::collections::BTreeSet::new();
        for seed in 0..500 {
            let undo =
                volatile(CrashLoss::PerKey, &["a", "b", "a", "b", "a"]).crash(&Jitter::new(seed));
            let calls = lost_calls(&undo);
            assert!(
                calls.windows(2).all(|w| w[0] > w[1]),
                "newest first: {calls:?}"
            );
            // Per key, the lost writes are the newest ones: a's calls are 1,3,5 and b's 2,4.
            for key_calls in [[1, 3, 5].as_slice(), &[2, 4]] {
                let lost: Vec<u64> = key_calls
                    .iter()
                    .copied()
                    .filter(|c| calls.contains(c))
                    .collect();
                assert_eq!(
                    lost,
                    key_calls[key_calls.len() - lost.len()..],
                    "seed {seed}"
                );
            }
            outcomes.insert(calls);
        }
        // 4 choices for a times 3 for b; out-of-order survival (b's 4 kept, a's 3 lost) included.
        assert_eq!(outcomes.len(), 12);
        assert!(outcomes.contains(&vec![5, 3]));
    }

    #[test]
    fn nothing_unsynced_loses_nothing() {
        for loss in [CrashLoss::Suffix, CrashLoss::PerKey] {
            assert!(volatile(loss, &[]).crash(&Jitter::new(1)).is_empty());
        }
    }
}
