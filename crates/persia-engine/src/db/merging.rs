//! Running merges for a [`Database`] (SPEC §8.5, ADR-0032, ROADMAP 6.5.3, 6.5.9).
//!
//! The writer plans merges with the [`MergePolicy`](crate::merge::MergePolicy) over each collection's committed
//! segments, hands them to the worker pool, and commits each result as a merge commit: the inputs removed, the merged
//! segment added after the kept ones (SPEC §8.7.2). Commits may delete from the inputs while a merge runs; the merge
//! commit then removes each input at its current sidecar version and deletes those documents from the merged segment
//! (its first sidecar), mapping their docids with the merge's [`DocidMap`](crate::merge::DocidMap), so no deleted
//! document comes back and steady deletes do not keep merges from committing. A result is discarded, and the segments
//! planned again, only when an input is gone or the schema changed. A merge that fails (its inputs are damaged, or the
//! merged segment fails `verify`) is reported, and its inputs are not merged again until the database is reopened.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use persia_format::ByteSource;
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::ContainerFile;
use roaring::RoaringBitmap;

use super::obsolete::{Obsolete, ObsoleteObject};
use super::{
    CollectionChange, CollectionDiff, Database, DatabaseError, LiveSegment, Location, ManifestDiff,
    SegmentEntry, SegmentId, new_segment_id, open_source, snapshot_of,
};
use crate::manifest::Tombstones;
use crate::merge::scheduler::{Finished, MergeJob, Merged, Workers};
use crate::merge::{MergeError, MergePlan, MergeReason};
use crate::tombstone::TombstoneSidecar;

/// What became of one merge.
#[derive(Debug)]
pub enum MergeOutcome {
    /// Committed: `inputs` are gone, and `output`, if the plan wrote one, holds their live documents.
    Committed {
        /// The collection.
        collection: String,
        /// Why the merge was planned.
        reason: MergeReason,
        /// The segments it removed, in plan order.
        inputs: Vec<SegmentId>,
        /// The merged segment; `None` when no input document was live.
        output: Option<SegmentId>,
        /// The merge commit's `seq`.
        seq: u64,
        /// Documents deleted from the inputs while the merge ran, which the commit deleted from `output` (or,
        /// when they were all of its documents, the reason it wrote none) (ROADMAP 6.5.9).
        reapplied: u64,
    },
    /// Discarded: an input is no longer live (a flush deleted its last documents, or its collection was dropped),
    /// or the collection's schema changed, after the plan read it (SPEC §8.5). The segments are planned again.
    Conflict {
        /// The collection.
        collection: String,
        /// The segments the plan read.
        inputs: Vec<SegmentId>,
    },
    /// Failed: nothing was committed, and `inputs` are not merged again until the database is reopened.
    Failed {
        /// The collection.
        collection: String,
        /// The segments the plan read.
        inputs: Vec<SegmentId>,
        /// Why.
        error: MergeError,
    },
    /// Cancelled ([`Database::cancel_merges`]); nothing was committed.
    Cancelled {
        /// The collection.
        collection: String,
        /// The segments the plan read.
        inputs: Vec<SegmentId>,
    },
}

/// What became of the merges a database ran since it was opened ([`Database::merge_stats`]): one count per
/// [`MergeOutcome`] kind, and how many committed merges caught up with deletions. The outcomes are also returned by
/// each step, but a caller that drops them would not see a discarded merge (ROADMAP 6.5.9, 6.5.11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeStats {
    /// Merges committed.
    pub committed: u64,
    /// Of `committed`, those that deleted from the merged segment documents deleted from the inputs while they ran
    /// (a [`MergeOutcome::Committed`] with `reapplied` above 0, ROADMAP 6.5.9).
    pub caught_up: u64,
    /// Merges discarded because an input is gone or the schema changed ([`MergeOutcome::Conflict`]).
    pub conflicts: u64,
    /// Merges that failed ([`MergeOutcome::Failed`]).
    pub failed: u64,
    /// Merges cancelled ([`MergeOutcome::Cancelled`]).
    pub cancelled: u64,
}

impl MergeStats {
    fn count(&mut self, outcome: &MergeOutcome) {
        if let MergeOutcome::Committed { reapplied: 1.., .. } = outcome {
            self.caught_up = self.caught_up.saturating_add(1);
        }
        let n = match outcome {
            MergeOutcome::Committed { .. } => &mut self.committed,
            MergeOutcome::Conflict { .. } => &mut self.conflicts,
            MergeOutcome::Failed { .. } => &mut self.failed,
            MergeOutcome::Cancelled { .. } => &mut self.cancelled,
        };
        *n = n.saturating_add(1);
    }
}

/// The writer's merge state.
#[derive(Debug)]
pub(super) struct Merges {
    workers: Workers,
    /// The inputs and output id of each merge in flight.
    running: Vec<(HashSet<SegmentId>, SegmentId)>,
    /// Inputs of failed merges, left out of planning.
    failed: HashSet<SegmentId>,
    pub obsolete: Obsolete,
    stats: MergeStats,
    /// Whether finished merges wait for [`Database::release_merge`] (tests, ROADMAP 6.5.14).
    held: bool,
}

impl Merges {
    pub fn new(threads: usize) -> Self {
        Self {
            workers: Workers::new(threads),
            running: Vec::new(),
            failed: HashSet::new(),
            obsolete: Obsolete::default(),
            stats: MergeStats::default(),
            held: false,
        }
    }

    /// Whether a merge in flight writes a segment with this id.
    pub fn reserves(&self, id: SegmentId) -> bool {
        self.running.iter().any(|(_, out)| *out == id)
    }

    /// Segments the policy must leave out: held by a merge in flight, or failed.
    fn busy(&self) -> HashSet<SegmentId> {
        let mut busy = self.failed.clone();
        for (inputs, _) in &self.running {
            busy.extend(inputs);
        }
        busy
    }

    fn done(&mut self, job: &MergeJob) {
        self.running.retain(|(_, out)| *out != job.segment_id);
    }
}

fn input_ids(plan: &MergePlan) -> Vec<SegmentId> {
    plan.inputs.iter().map(|i| i.id).collect()
}

impl<F: ContainerFile + Sync + 'static> Database<F> {
    /// Commits the merges that finished, then plans merges for every collection and starts as many as there is room
    /// for (one per merge thread, [`crate::merge::MergeOptions::threads`]); returns what became of the finished ones.
    /// Never waits for a merge, except with no threads, where it runs one merge to the end and commits it. Call it
    /// periodically, like [`Database::tick`]: the database starts its threads but does not wake on its own.
    ///
    /// # Errors
    /// [`DatabaseError::Poisoned`]; [`DatabaseError::MergeThread`] if no worker can be started; a failed merge
    /// commit, which poisons the database; a compaction it starts ([`crate::db::CompactionOptions::automatic`]) that
    /// poisons the database. A merge that fails is an outcome, not an error; so is a compaction that fails without
    /// poisoning ([`Database::compaction_stats`]).
    pub fn merge_step(&mut self) -> Result<Vec<MergeOutcome>, DatabaseError> {
        self.usable()?;
        let mut outcomes = Vec::new();
        while let Some(finished) = self.next_released() {
            outcomes.push(self.commit_merge(finished)?);
        }
        self.start_merges()?;
        if self.options.merge.threads == 0 && !self.merges.held {
            // The merge just ran inline. A worker's result waits for the next step, so a step never commits a
            // merge it started.
            while let Some(finished) = self.merges.workers.try_next() {
                outcomes.push(self.commit_merge(finished)?);
            }
        }
        if self.options.compaction.automatic {
            // Merges are what leaves most frames dead; the inputs a merge in flight holds are kept (SPEC §4.6). The
            // merges above are committed: a compaction that fails and leaves the database usable (before its rename,
            // e.g. no room for the copy) is counted in `compaction_stats` and tried again later, not reported here.
            if let Err(e) = self.compact_if_due()
                && self.poisoned
            {
                return Err(e);
            }
        }
        Ok(outcomes)
    }

    /// Runs merges until the policy plans none: [`Database::merge_step`], waiting for each merge in flight. Writes
    /// wait meanwhile, since they need the writer this holds.
    ///
    /// # Errors
    /// As [`Database::merge_step`].
    pub fn finish_merges(&mut self) -> Result<Vec<MergeOutcome>, DatabaseError> {
        let mut outcomes = Vec::new();
        loop {
            let step = self.merge_step()?;
            let progressed = !step.is_empty();
            outcomes.extend(step);
            match self.merges.workers.next() {
                Some(finished) => outcomes.push(self.commit_merge(finished)?),
                None if !progressed => return Ok(outcomes),
                None => {}
            }
        }
    }

    /// Cancels every merge in flight and waits for them to stop; nothing they did is committed. Merges start again
    /// at the next [`Database::merge_step`].
    pub fn cancel_merges(&mut self) -> Vec<MergeOutcome> {
        self.merges.workers.cancel_all();
        let mut outcomes = Vec::new();
        while let Some(Finished { job, .. }) = self.merges.workers.next() {
            self.merges.done(&job);
            let outcome = MergeOutcome::Cancelled {
                inputs: input_ids(&job.plan),
                collection: job.collection,
            };
            self.merges.stats.count(&outcome);
            outcomes.push(outcome);
        }
        outcomes
    }

    /// Test hook (ROADMAP 6.5.14): while `hold` is set, [`Database::merge_step`] commits no finished merge, inline
    /// ones included; each waits, in flight, for [`Database::release_merge`], [`Database::finish_merges`] or
    /// [`Database::cancel_merges`]. Whether a merge is in flight across a write then depends on the calls made, not
    /// on how fast a worker runs, so a randomized test explores the same schedules on every machine.
    #[doc(hidden)]
    pub fn hold_merges(&mut self, hold: bool) {
        self.merges.held = hold;
    }

    /// Test hook (ROADMAP 6.5.14): waits for the next merge in flight to finish and commits it, as
    /// [`Database::merge_step`] would have; `None` when none is in flight. Nothing else is planned or started.
    ///
    /// # Errors
    /// [`DatabaseError::Poisoned`]; a failed merge commit, which poisons the database.
    #[doc(hidden)]
    pub fn release_merge(&mut self) -> Result<Option<MergeOutcome>, DatabaseError> {
        self.usable()?;
        match self.merges.workers.next() {
            Some(finished) => self.commit_merge(finished).map(Some),
            None => Ok(None),
        }
    }

    /// A finished merge to commit now: none while merges are held ([`Database::hold_merges`]).
    fn next_released(&mut self) -> Option<Finished> {
        if self.merges.held {
            None
        } else {
            self.merges.workers.try_next()
        }
    }

    /// Merges submitted and not yet committed or discarded: running, or finished and waiting for the next step.
    pub fn merges_running(&self) -> usize {
        self.merges.workers.in_flight()
    }

    /// How many merges committed, conflicted, failed or were cancelled since the database was opened.
    pub fn merge_stats(&self) -> MergeStats {
        self.merges.stats
    }

    /// Every segment and tombstone sidecar that no commit lists any more and that has not been taken by
    /// [`Database::take_unreferenced`], each with whether a reader (a snapshot, or a merge in flight) may still
    /// read it (SPEC §8.5, ROADMAP 6.5.4).
    pub fn obsolete(&self) -> impl Iterator<Item = (&ObsoleteObject, bool)> {
        self.merges.obsolete.all()
    }

    /// Removes and returns the obsolete objects no reader can read any more: they may be deleted. Locally they are
    /// frames, which a container compaction reclaims (SPEC §4.6); it must keep the ones still referenced.
    pub fn take_unreferenced(&mut self) -> Vec<ObsoleteObject> {
        self.merges.obsolete.take_unreferenced()
    }

    /// Plans merges over every collection's committed segments and submits them while there is room.
    fn start_merges(&mut self) -> Result<(), DatabaseError> {
        let capacity = self.options.merge.capacity();
        let names: Vec<String> = self.collections.keys().cloned().collect();
        for name in names {
            if self.merges.workers.in_flight() >= capacity {
                return Ok(());
            }
            let Some(entry) = self.manifest.collection(&name) else {
                continue;
            };
            let plans = self
                .options
                .merge
                .policy
                .plan(entry.segments(), &self.merges.busy());
            for plan in plans {
                if self.merges.workers.in_flight() >= capacity {
                    return Ok(());
                }
                let Some(job) = self.job(&name, plan)? else {
                    continue;
                };
                let inputs = job.plan.inputs.iter().map(|i| i.id).collect();
                self.merges.running.push((inputs, job.segment_id));
                if let Err(e) = self.merges.workers.submit(job) {
                    self.merges.running.pop();
                    return Err(DatabaseError::MergeThread(e));
                }
            }
        }
        Ok(())
    }

    /// The job for `plan` over the open segments of `collection`; `None` if they are not the ones the plan names at
    /// the versions it read (a bug in planning: the manifest and the open segments disagree).
    fn job(
        &mut self,
        collection: &str,
        plan: MergePlan,
    ) -> Result<Option<MergeJob>, DatabaseError> {
        let Some(c) = self.collections.get(collection) else {
            return Ok(None);
        };
        let mut inputs = Vec::with_capacity(plan.inputs.len());
        for input in &plan.inputs {
            let Some(live) = find(&c.segments, input.id) else {
                return Err(internal("a planned merge input is not open"));
            };
            let version = live.deleted.as_ref().map_or(0, |(v, _)| *v);
            if version != input.sidecar {
                return Err(internal(
                    "a planned merge input's sidecar is not the open one",
                ));
            }
            let tombstones = live.deleted.as_ref().map(|(_, t)| Arc::clone(t));
            inputs.push((Arc::clone(&live.segment), tombstones));
        }
        let merges = &self.merges;
        let segment_id = new_segment_id(&mut self.options.segment_ids, &self.manifest, |id| {
            merges.reserves(id)
        });
        Ok(Some(MergeJob {
            collection: collection.to_owned(),
            plan,
            schema: Arc::clone(&c.schema),
            registry: self.options.registry.clone(),
            inputs,
            segment_id,
            created_at: self.options.clock.unix_micros(),
            write_options: self.options.write_options,
            cancel: Arc::new(AtomicBool::new(false)),
        }))
    }

    /// Commits a finished merge if its inputs are still what the plan read; a failed commit poisons the database.
    fn commit_merge(&mut self, finished: Finished) -> Result<MergeOutcome, DatabaseError> {
        let outcome = self.settle_merge(finished)?;
        self.merges.stats.count(&outcome);
        Ok(outcome)
    }

    /// [`Self::commit_merge`] before the outcome is counted.
    fn settle_merge(&mut self, finished: Finished) -> Result<MergeOutcome, DatabaseError> {
        let Finished { job, result } = finished;
        self.merges.done(&job);
        let inputs = input_ids(&job.plan);
        let merged = match result {
            Ok(merged) => merged,
            Err(MergeError::Cancelled) => {
                return Ok(MergeOutcome::Cancelled {
                    collection: job.collection,
                    inputs,
                });
            }
            Err(error) => {
                self.merges.failed.extend(inputs.iter().copied());
                return Ok(MergeOutcome::Failed {
                    collection: job.collection,
                    inputs,
                    error,
                });
            }
        };
        let catch_up = match self.catch_up(&job, merged.as_ref()) {
            Ok(Some(catch_up)) => catch_up,
            Ok(None) => {
                return Ok(MergeOutcome::Conflict {
                    collection: job.collection,
                    inputs,
                });
            }
            // Nothing was written; replanning would meet the same broken invariant at every step.
            Err(DatabaseError::Internal { what }) => {
                self.merges.failed.extend(inputs.iter().copied());
                return Ok(MergeOutcome::Failed {
                    collection: job.collection,
                    inputs,
                    error: MergeError::Internal { what },
                });
            }
            Err(e) => return Err(e),
        };
        let reapplied = catch_up.deleted.len();
        let result = self.commit_merged(&job, merged, catch_up);
        if result.is_err() {
            self.poisoned = true;
        }
        let (output, seq) = result?;
        Ok(MergeOutcome::Committed {
            collection: job.collection,
            reason: job.plan.reason,
            inputs,
            output,
            seq,
            reapplied,
        })
    }

    /// What `job` must catch up with to commit (SPEC §8.5, ROADMAP 6.5.9): each input's current sidecar version, and
    /// the merged docids of the documents deleted from the inputs since the plan read them, which the merged segment
    /// still holds. `None` (a conflict) if the collection's schema changed or an input is gone (a flush deleted its
    /// last documents, or the collection was dropped), or the manifest refuses the diff; checked before anything is
    /// written, so such a merge is discarded instead of poisoning the database after its frame is appended.
    ///
    /// # Errors
    /// [`DatabaseError::Internal`] (the merge then fails, and its inputs are not planned again) if an input's
    /// deletions lost one the plan read (each sidecar version keeps the previous one's, SPEC §8.3), or a document
    /// deleted since is not in the merged segment: nothing is written.
    fn catch_up(
        &self,
        job: &MergeJob,
        merged: Option<&Merged>,
    ) -> Result<Option<CatchUp>, DatabaseError> {
        let Some(c) = self.collections.get(&job.collection) else {
            return Ok(None);
        };
        if !Arc::ptr_eq(&c.schema, &job.schema) {
            return Ok(None);
        }
        let mut versions = Vec::with_capacity(job.plan.inputs.len());
        let mut deleted = RoaringBitmap::new();
        for (i, (input, (segment, read))) in job.plan.inputs.iter().zip(&job.inputs).enumerate() {
            // The same open segment, not only the same id.
            let Some(live) =
                find(&c.segments, input.id).filter(|l| Arc::ptr_eq(&l.segment, segment))
            else {
                return Ok(None);
            };
            let version = live.deleted.as_ref().map_or(0, |(v, _)| *v);
            versions.push(version);
            if version == input.sidecar {
                continue;
            }
            let Some((_, now)) = &live.deleted else {
                return Err(internal("a merge input lost its deletions while it merged"));
            };
            let mut since = now.docids().clone();
            if let Some(read) = read {
                if !read.docids().is_subset(now.docids()) {
                    return Err(internal("a merge input lost a deletion while it merged"));
                }
                since -= read.docids();
            }
            for docid in &since {
                // A plan with no live document (a drop) cannot lose one since.
                let to = merged.and_then(|m| m.docids.get(i, docid)).ok_or(internal(
                    "a document deleted while it merged is not in the merged segment",
                ))?;
                deleted.insert(to);
            }
        }
        if self.manifest.apply(&removal(job, &versions)).is_err() {
            return Ok(None);
        }
        Ok(Some(CatchUp { versions, deleted }))
    }

    /// The merge commit (SPEC §4.5, §8.7.2): the merged segment's frame and, for deletions committed to the inputs
    /// while it ran, its first tombstone sidecar; then the manifest that removes the inputs at their current versions
    /// and adds it (none when those deletions cover every merged document); then the new state is published and the
    /// inputs and their sidecars become obsolete.
    fn commit_merged(
        &mut self,
        job: &MergeJob,
        merged: Option<Merged>,
        catch_up: CatchUp,
    ) -> Result<(Option<SegmentId>, u64), DatabaseError> {
        let CatchUp { versions, deleted } = catch_up;
        let mut diff = removal(job, &versions);
        let mut added = None;
        let merged = merged.filter(|m| deleted.len() < u64::from(m.doc_count));
        if let Some(Merged {
            bytes, doc_count, ..
        }) = merged
        {
            // Checked before the frame is appended. Swallowing a missing change would commit the
            // removal and leave the merged bytes unreferenced: the inputs' live documents would be
            // gone.
            let Some(CollectionDiff::Change(change)) = diff.collections.get_mut(&job.collection)
            else {
                return Err(internal("a merge diff has no collection change"));
            };
            change
                .added
                .push(self.append_merged(job, &bytes, doc_count, &deleted)?);
            added = Some(bytes);
        }
        let next = self.commit_diff(diff)?;

        // Durable: step 4.
        let seq = next.seq();
        let c = self
            .collections
            .get_mut(&job.collection)
            .ok_or(internal("a merged collection is not open"))?;
        let base = self
            .manifest
            .collection(&job.collection)
            .ok_or(internal("a merged collection is not in the manifest"))?;
        let removed: HashSet<SegmentId> = job.plan.inputs.iter().map(|i| i.id).collect();
        for live in c
            .segments
            .iter()
            .filter(|l| removed.contains(&SegmentId::new(l.segment.segment_id())))
        {
            let id = SegmentId::new(live.segment.segment_id());
            let entry = base
                .segment(id)
                .ok_or(internal("a merged segment is not in the manifest"))?;
            self.merges
                .obsolete
                .retire_segment(&job.collection, live, entry, seq);
        }
        c.segments
            .retain(|l| !removed.contains(&SegmentId::new(l.segment.segment_id())));
        let output = match added {
            Some(bytes) => {
                let entry = next
                    .collection(&job.collection)
                    .and_then(|e| e.segment(job.segment_id))
                    .ok_or(internal("a committed segment is not in its manifest"))?;
                let segment =
                    open_source(ByteSource::new(bytes), &job.collection, entry, &c.schema)?;
                let tombstones = if deleted.is_empty() {
                    None
                } else {
                    // `TombstoneSidecar::new` checked every docid against the segment's count.
                    let t = segment
                        .tombstones(deleted)
                        .map_err(|_| internal("a merged segment's sidecar does not fit it"))?;
                    Some((1, Arc::new(t)))
                };
                c.segments.push(LiveSegment::new(segment, tombstones));
                Some(job.segment_id)
            }
            None => None,
        };
        self.manifest = Arc::new(next);
        self.published
            .store(Arc::new(snapshot_of(&self.manifest, &self.collections)));
        Ok((output, seq))
    }

    /// Appends the merged segment's frame and, when deletions were committed to the inputs while it ran, its first
    /// sidecar after it (SPEC §8.3, §8.5); returns the manifest entry that adds them.
    fn append_merged(
        &mut self,
        job: &MergeJob,
        bytes: &[u8],
        doc_count: u32,
        deleted: &RoaringBitmap,
    ) -> Result<SegmentEntry, DatabaseError> {
        let sidecar = if deleted.is_empty() {
            None
        } else {
            let invalid = |source| DatabaseError::Sidecar {
                collection: job.collection.clone(),
                id: job.segment_id,
                source,
            };
            let sidecar = TombstoneSidecar::new(job.segment_id, 1, doc_count, deleted.clone())
                .map_err(invalid)?;
            let encoded = sidecar.encode().map_err(invalid)?;
            Some((sidecar.deleted_count(), encoded))
        };
        let container = self.wal.log_mut();
        let frame = container.append(FrameKind::Segment, bytes, Compression::None)?;
        let tombstones = match sidecar {
            Some((count, encoded)) => {
                // After the segment it deletes from (SPEC §8.3).
                let frame =
                    container.append(FrameKind::TombstoneSidecar, &encoded, Compression::None)?;
                Some(Tombstones {
                    version: 1,
                    deleted: count,
                    location: Location::Frame {
                        offset: frame.offset,
                        len: frame.len,
                    },
                })
            }
            None => None,
        };
        Ok(SegmentEntry {
            id: job.segment_id,
            doc_count,
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            location: Location::Frame {
                offset: frame.offset,
                len: frame.len,
            },
            tombstones,
        })
    }
}

/// What a finished merge catches up with before it commits ([`Database::catch_up`]).
#[derive(Debug)]
struct CatchUp {
    /// Each input's current sidecar version (0 for none), in plan order: the merge commit removes it at that one.
    versions: Vec<u64>,
    /// The merged docids of the documents deleted from the inputs since the plan read them.
    deleted: RoaringBitmap,
}

fn find(segments: &[LiveSegment], id: SegmentId) -> Option<&LiveSegment> {
    segments
        .iter()
        .find(|l| SegmentId::new(l.segment.segment_id()) == id)
}

fn internal(what: &'static str) -> DatabaseError {
    DatabaseError::Internal { what }
}

/// The diff that removes `job`'s inputs, each at its sidecar version in `versions` (plan order); the merge commit
/// adds its segment.
fn removal(job: &MergeJob, versions: &[u64]) -> ManifestDiff {
    let mut change = CollectionChange::default();
    for (input, version) in job.plan.inputs.iter().zip(versions) {
        change.removed.insert(input.id, *version);
    }
    let mut diff = ManifestDiff::default();
    diff.collections
        .insert(job.collection.clone(), CollectionDiff::Change(change));
    diff
}
