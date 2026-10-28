//! The tiered merge policy (SPEC §8.5, ADR-0031, ROADMAP 6.5.1).
//!
//! A pure decision, like [`crate::db::CommitPolicy`]: a collection's segments as the manifest lists them and the ones
//! running merges hold, in; disjoint plans out. Nothing is read but the manifest entries.

use std::collections::HashSet;

use crate::manifest::{MAX_SEGMENT_DOCS, SegmentEntry, SegmentId};

/// When and what to merge (SPEC §8.5, §14 `merge.*`).
///
/// Values that would make no sense are raised: a `tier_factor` or `segments_per_merge` below 2 counts as 2, a
/// `tier_floor` of 0 as 1, and a `max_merged_docs` above [`MAX_SEGMENT_DOCS`] as that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergePolicy {
    /// Live size, in bytes, below which (times `tier_factor`) every segment is tier 0.
    pub tier_floor: u64,
    /// Ratio of live sizes between one tier and the next.
    pub tier_factor: u64,
    /// Segments one tier merge takes, and the fewest a tier needs to merge.
    pub segments_per_merge: usize,
    /// A segment with more than this percentage of its documents deleted is rewritten.
    pub deleted_percent: u8,
    /// Most live bytes a tier merge may take; segments this large take part only in deletion rewrites.
    pub max_merged_bytes: u64,
    /// Most live documents a tier merge may take.
    pub max_merged_docs: u32,
}

impl Default for MergePolicy {
    /// The defaults of SPEC §8.5.
    fn default() -> Self {
        Self {
            tier_floor: 2 << 20,
            tier_factor: 10,
            segments_per_merge: 8,
            deleted_percent: 20,
            max_merged_bytes: 5 << 30,
            max_merged_docs: MAX_SEGMENT_DOCS,
        }
    }
}

/// Why a [`MergePlan`] was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeReason {
    /// Every document of its segments is deleted: they are removed and nothing is written.
    Drop,
    /// Enough segments share this size tier.
    Tier(u32),
    /// Too many of its one segment's documents are deleted.
    Deletes,
}

/// A segment a plan merges, as the manifest listed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeInput {
    /// The segment.
    pub id: SegmentId,
    /// The version of its tombstone sidecar the plan read (0 for none): the merge drops the documents deleted at that
    /// version, and its commit removes the segment at that version (SPEC §8.7.2).
    pub sidecar: u64,
}

/// One merge: segments in, one segment out (none for [`MergeReason::Drop`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    /// Why.
    pub reason: MergeReason,
    /// Its segments, in manifest order: the merged segment holds their live documents in this order (SPEC §5.5).
    pub inputs: Vec<MergeInput>,
}

/// What the policy knows of one segment.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    /// Position in the manifest.
    position: usize,
    input: MergeInput,
    doc_count: u32,
    live: u32,
    live_size: u64,
}

impl Candidate {
    fn of(position: usize, entry: &SegmentEntry) -> Self {
        let deleted = entry.tombstones.map_or(0, |t| t.deleted);
        // A manifest that decoded has `deleted <= doc_count` (SPEC §8.7.1); saturate rather than trust it here.
        let live = entry.doc_count.saturating_sub(deleted);
        let live_size = if entry.doc_count == 0 {
            0
        } else {
            // Below `size`, so it fits a u64.
            u64::try_from(u128::from(entry.size) * u128::from(live) / u128::from(entry.doc_count))
                .unwrap_or(u64::MAX)
        };
        Self {
            position,
            input: MergeInput {
                id: entry.id,
                sidecar: entry.tombstones.map_or(0, |t| t.version),
            },
            doc_count: entry.doc_count,
            live,
            live_size,
        }
    }

    fn deleted(&self) -> u32 {
        self.doc_count - self.live
    }
}

impl MergePolicy {
    /// The merges to run for one collection whose live segments are `segments`, in manifest order, while running
    /// merges hold the segments in `busy` (SPEC §8.5). Plans are disjoint and leave `busy` out: drops first, then
    /// tier merges by tier and size, then deletion rewrites in manifest order.
    ///
    /// ```
    /// use persia_engine::manifest::{Location, SegmentEntry, SegmentId};
    /// use persia_engine::merge::{MergePolicy, MergeReason};
    ///
    /// let flush = |id| SegmentEntry {
    ///     id: SegmentId::new(id),
    ///     doc_count: 1_000,
    ///     size: 1 << 20,
    ///     location: Location::Object,
    ///     tombstones: None,
    /// };
    /// let segments: Vec<_> = (1..=9).map(flush).collect();
    /// let plans = MergePolicy::default().plan(&segments, &Default::default());
    /// // Nine small flushes share tier 0: the first eight (all the same size) merge, the ninth waits.
    /// assert_eq!(plans.len(), 1);
    /// assert_eq!(plans[0].reason, MergeReason::Tier(0));
    /// assert_eq!(plans[0].inputs.len(), 8);
    /// ```
    #[must_use]
    pub fn plan(&self, segments: &[SegmentEntry], busy: &HashSet<SegmentId>) -> Vec<MergePlan> {
        let mut free: Vec<Candidate> = segments
            .iter()
            .enumerate()
            .filter(|(_, entry)| !busy.contains(&entry.id))
            .map(|(position, entry)| Candidate::of(position, entry))
            .collect();
        let mut plans = Vec::new();

        let (dead, rest): (Vec<_>, Vec<_>) = free.into_iter().partition(|c| c.live == 0);
        free = rest;
        if !dead.is_empty() {
            plans.push(plan(MergeReason::Drop, dead));
        }

        let mut taken = HashSet::new();
        for (tier, group) in self.tiers(&free) {
            for chosen in self.tier_merges(&group) {
                taken.extend(chosen.iter().map(|c| c.position));
                plans.push(plan(MergeReason::Tier(tier), chosen));
            }
        }

        let percent = u64::from(self.deleted_percent);
        for candidate in free {
            let over =
                u64::from(candidate.deleted()) * 100 > u64::from(candidate.doc_count) * percent;
            if over && !taken.contains(&candidate.position) {
                plans.push(plan(MergeReason::Deletes, vec![candidate]));
            }
        }
        plans
    }

    /// The segments that may take part in tier merges, grouped by tier, lowest first; each group by live size, then
    /// manifest position.
    fn tiers(&self, free: &[Candidate]) -> Vec<(u32, Vec<Candidate>)> {
        let mut tiered: Vec<(u32, Candidate)> = free
            .iter()
            .filter(|c| c.live_size < self.max_merged_bytes)
            .map(|c| (self.tier_of(c.live_size), *c))
            .collect();
        tiered.sort_by_key(|(tier, c)| (*tier, c.live_size, c.position));
        let mut tiers: Vec<(u32, Vec<Candidate>)> = Vec::new();
        for (tier, candidate) in tiered {
            match tiers.last_mut() {
                Some((last, group)) if *last == tier => group.push(candidate),
                _ => tiers.push((tier, vec![candidate])),
            }
        }
        tiers
    }

    /// 0 below `tier_floor × tier_factor`; otherwise the largest `t` with `live_size ≥ tier_floor × tier_factor^t`.
    fn tier_of(&self, live_size: u64) -> u32 {
        let factor = self.tier_factor.max(2);
        let mut tier = 0;
        let mut bound = self.tier_floor.max(1).saturating_mul(factor);
        while live_size >= bound && bound < u64::MAX {
            tier += 1;
            bound = bound.saturating_mul(factor);
        }
        tier
    }

    /// The merges of one tier's segments, sorted smallest first: `segments_per_merge` at a time while that many are
    /// left and the caps hold.
    fn tier_merges(&self, group: &[Candidate]) -> Vec<Vec<Candidate>> {
        let width = self.segments_per_merge.max(2);
        let max_docs = u64::from(self.max_merged_docs.min(MAX_SEGMENT_DOCS));
        let mut merges = Vec::new();
        for chosen in group.chunks_exact(width) {
            let bytes = chosen
                .iter()
                .fold(0u64, |sum, c| sum.saturating_add(c.live_size));
            let docs: u64 = chosen.iter().map(|c| u64::from(c.live)).sum();
            if bytes > self.max_merged_bytes || docs > max_docs {
                break;
            }
            merges.push(chosen.to_vec());
        }
        merges
    }
}

/// A plan of `candidates`, in manifest order.
fn plan(reason: MergeReason, mut candidates: Vec<Candidate>) -> MergePlan {
    candidates.sort_by_key(|c| c.position);
    MergePlan {
        reason,
        inputs: candidates.into_iter().map(|c| c.input).collect(),
    }
}

#[cfg(test)]
mod tests;
