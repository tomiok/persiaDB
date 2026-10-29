//! Running merges for a [`Database`] (SPEC §8.5, ADR-0032, ROADMAP 6.5.3).
//!
//! The writer plans merges with the [`MergePolicy`](crate::merge::MergePolicy) over each collection's committed
//! segments, hands them to the worker pool, and commits each result as a merge commit: the inputs removed at the
//! sidecar version the plan read, the merged segment added after the kept ones (SPEC §8.7.2). A result whose inputs
//! changed meanwhile (a commit deleted from one: its sidecar version moved) would bring deleted documents back, so it
//! is discarded and the segments are planned again. A merge that fails (its inputs are damaged, or the merged
//! segment fails `verify`) is reported, and its inputs are not merged again until the database is reopened.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use persia_format::ByteSource;
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::ContainerFile;

use super::obsolete::{Obsolete, ObsoleteKind, ObsoleteObject, Pin};
use super::{
    CollectionChange, CollectionDiff, Database, DatabaseError, LiveSegment, Location, ManifestDiff,
    SegmentEntry, SegmentId, new_segment_id, open_source, snapshot_of,
};
use crate::merge::scheduler::{Finished, MergeJob, Merged, Workers};
use crate::merge::{MergeError, MergePlan, MergeReason};

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
    },
    /// Discarded: an input was removed, or a commit deleted from it, after the plan read it (SPEC §8.7.2). The
    /// segments are planned again.
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

/// The writer's merge state.
#[derive(Debug)]
pub(super) struct Merges {
    workers: Workers,
    /// The inputs and output id of each merge in flight.
    running: Vec<(HashSet<SegmentId>, SegmentId)>,
    /// Inputs of failed merges, left out of planning.
    failed: HashSet<SegmentId>,
    pub obsolete: Obsolete,
}

impl Merges {
    pub fn new(threads: usize) -> Self {
        Self {
            workers: Workers::new(threads),
            running: Vec::new(),
            failed: HashSet::new(),
            obsolete: Obsolete::default(),
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

impl<F: ContainerFile + Sync> Database<F> {
    /// Commits the merges that finished, then plans merges for every collection and starts as many as there is room
    /// for (one per merge thread, [`crate::merge::MergeOptions::threads`]); returns what became of the finished ones.
    /// Never waits for a merge, except with no threads, where it runs one merge to the end and commits it. Call it
    /// periodically, like [`Database::tick`]: the database starts its threads but does not wake on its own.
    ///
    /// # Errors
    /// [`DatabaseError::Poisoned`]; [`DatabaseError::MergeThread`] if no worker can be started; a failed merge
    /// commit, which poisons the database. A merge that fails is an outcome, not an error.
    pub fn merge_step(&mut self) -> Result<Vec<MergeOutcome>, DatabaseError> {
        self.usable()?;
        let mut outcomes = Vec::new();
        while let Some(finished) = self.merges.workers.try_next() {
            outcomes.push(self.commit_merge(finished)?);
        }
        self.start_merges()?;
        if self.options.merge.threads == 0 {
            // The merge just ran inline. A worker's result waits for the next step, so a step never commits a
            // merge it started.
            while let Some(finished) = self.merges.workers.try_next() {
                outcomes.push(self.commit_merge(finished)?);
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
            outcomes.push(MergeOutcome::Cancelled {
                inputs: input_ids(&job.plan),
                collection: job.collection,
            });
        }
        outcomes
    }

    /// Merges submitted and not yet committed or discarded: running, or finished and waiting for the next step.
    pub fn merges_running(&self) -> usize {
        self.merges.workers.in_flight()
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
        if !self.still_applies(&job) {
            return Ok(MergeOutcome::Conflict {
                collection: job.collection,
                inputs,
            });
        }
        let result = self.commit_merged(&job, merged);
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
        })
    }

    /// Whether `job`'s collection still has its schema and every input, each at the sidecar version the plan read,
    /// and the manifest accepts the diff that removes them (SPEC §8.7.2). Checked before anything is written, so a
    /// merge diff that would not apply is discarded instead of poisoning the database after its frame is appended.
    fn still_applies(&self, job: &MergeJob) -> bool {
        let Some(c) = self.collections.get(&job.collection) else {
            return false;
        };
        let open = Arc::ptr_eq(&c.schema, &job.schema)
            && job.plan.inputs.iter().all(|input| {
                find(&c.segments, input.id).is_some_and(|live| {
                    live.deleted.as_ref().map_or(0, |(v, _)| *v) == input.sidecar
                })
            });
        open && self.manifest.apply(&removal(job)).is_ok()
    }

    /// The merge commit (SPEC §4.5, §8.7.2): the merged segment's frame, then the manifest that removes the inputs
    /// and adds it; then the new state is published and the inputs and their sidecars become obsolete.
    fn commit_merged(
        &mut self,
        job: &MergeJob,
        merged: Option<Merged>,
    ) -> Result<(Option<SegmentId>, u64), DatabaseError> {
        let mut diff = removal(job);
        let mut added = None;
        if let Some(Merged { bytes, doc_count }) = merged {
            // Checked before the frame is appended. Swallowing a missing change would commit the
            // removal and leave the merged bytes unreferenced: the inputs' live documents would be
            // gone.
            let Some(CollectionDiff::Change(change)) = diff.collections.get_mut(&job.collection)
            else {
                return Err(internal("a merge diff has no collection change"));
            };
            let frame = self
                .wal
                .log_mut()
                .append(FrameKind::Segment, &bytes, Compression::None)?;
            let entry = SegmentEntry {
                id: job.segment_id,
                doc_count,
                size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                location: Location::Frame {
                    offset: frame.offset,
                    len: frame.len,
                },
                tombstones: None,
            };
            change.added.push(entry);
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
            let obsolete = &mut self.merges.obsolete;
            obsolete.retire(
                &job.collection,
                ObsoleteKind::Segment(id),
                entry.location,
                seq,
                Pin::segment(&live.segment),
            );
            if let (Some((version, tombstones)), Some(t)) = (&live.deleted, entry.tombstones) {
                let kind = ObsoleteKind::Sidecar {
                    segment: id,
                    version: *version,
                };
                obsolete.retire(
                    &job.collection,
                    kind,
                    t.location,
                    seq,
                    Pin::sidecar(tombstones),
                );
            }
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
                c.segments.push(LiveSegment {
                    segment,
                    deleted: None,
                });
                Some(job.segment_id)
            }
            None => None,
        };
        self.manifest = Arc::new(next);
        self.published
            .store(Arc::new(snapshot_of(&self.manifest, &self.collections)));
        Ok((output, seq))
    }
}

fn find(segments: &[LiveSegment], id: SegmentId) -> Option<&LiveSegment> {
    segments
        .iter()
        .find(|l| SegmentId::new(l.segment.segment_id()) == id)
}

fn internal(what: &'static str) -> DatabaseError {
    DatabaseError::Internal { what }
}

/// The diff that removes `job`'s inputs at the sidecar versions its plan read; the merge commit adds its segment.
fn removal(job: &MergeJob) -> ManifestDiff {
    let mut change = CollectionChange::default();
    for input in &job.plan.inputs {
        change.removed.insert(input.id, input.sidecar);
    }
    let mut diff = ManifestDiff::default();
    diff.collections
        .insert(job.collection.clone(), CollectionDiff::Change(change));
    diff
}
