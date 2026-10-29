//! Background merges (SPEC §8.5, ADR-0032, ROADMAP 6.5.3): a pool of worker threads that run merge jobs.
//!
//! The writer plans every merge ([`super::MergePolicy`]) and commits every result; a worker only reads its inputs,
//! which a job holds as `Arc`s (they are immutable, SPEC §5), and writes the merged bytes into memory. So workers
//! need no lock on the database, and a merge never blocks a read or a write. The pool bounds what is in flight to
//! one job per thread: a finished job's bytes wait for the writer to collect them, so a merge in memory is the most
//! there is per thread (backpressure), and plans that do not fit are made again later.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use bytes::Bytes;
use persia_analysis::AnalyzerRegistry;
use persia_format::ByteSource;

use super::merger::{MergeError, MergeSource, merge_until};
use super::policy::{MergePlan, MergePolicy, MergeReason};
use crate::manifest::SegmentId;
use crate::schema::CollectionSchema;
use crate::segment::{Segment, Tombstones, WriteOptions};

/// How a database merges in the background (SPEC §8.5, §14 `merge_threads`, `merge.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeOptions {
    /// What to merge, and when.
    pub policy: MergePolicy,
    /// Worker threads, and so merges in flight. 0 runs one merge at a time on the writer's thread, inside the call
    /// that starts it (deterministic, for tests and single-threaded embeddings).
    pub threads: usize,
}

impl MergeOptions {
    /// One worker thread (SPEC §14).
    pub const DEFAULT_THREADS: usize = 1;

    /// Merges the writer may have in flight: one per thread, one when merges run inline.
    pub(crate) fn capacity(&self) -> usize {
        self.threads.max(1)
    }
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            policy: MergePolicy::default(),
            threads: Self::DEFAULT_THREADS,
        }
    }
}

/// One planned merge with everything a worker needs: the inputs as the plan read them, and the output's identity.
#[derive(Debug)]
pub(crate) struct MergeJob {
    pub collection: String,
    pub plan: MergePlan,
    /// The schema the inputs were opened with and the output is written with.
    pub schema: Arc<CollectionSchema>,
    pub registry: AnalyzerRegistry,
    /// Each input of `plan`, in its order, with its deletions at the version the plan read.
    pub inputs: Vec<(Arc<Segment>, Option<Arc<Tombstones>>)>,
    /// The output's id, reserved against flushes while the job runs.
    pub segment_id: SegmentId,
    pub created_at: i64,
    pub write_options: WriteOptions,
    pub cancel: Arc<AtomicBool>,
}

/// A merged segment, written and verified.
#[derive(Debug)]
pub(crate) struct Merged {
    pub bytes: Bytes,
    pub doc_count: u32,
}

/// A job and what it came to: the merged segment, `None` when nothing is live (a drop, SPEC §8.5 step 1).
#[derive(Debug)]
pub(crate) struct Finished {
    pub job: MergeJob,
    pub result: Result<Option<Merged>, MergeError>,
}

impl MergeJob {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Merges, writes, then opens and verifies the output: a merge copies damage only `verify` finds (SPEC §5.5),
    /// and the commit must not publish it.
    fn run(&self) -> Result<Option<Merged>, MergeError> {
        if self.plan.reason == MergeReason::Drop {
            // Nothing is live: no input needs reading.
            return Ok(None);
        }
        let sources: Vec<MergeSource<'_>> = self
            .inputs
            .iter()
            .map(|(segment, tombstones)| MergeSource {
                segment,
                tombstones: tombstones.as_deref(),
            })
            .collect();
        let cancelled = || self.cancelled();
        let Some(merged) = merge_until(&self.schema, &self.registry, &sources, &cancelled)? else {
            return Ok(None);
        };
        if self.cancelled() {
            return Err(MergeError::Cancelled);
        }
        let bytes = Bytes::from(merged.write(
            self.segment_id.get(),
            self.created_at,
            self.write_options,
        )?);
        if self.cancelled() {
            return Err(MergeError::Cancelled);
        }
        let verify = |source| MergeError::Verify { source };
        let segment =
            Segment::open(ByteSource::new(bytes.clone()), &self.schema).map_err(verify)?;
        segment.verify().map_err(verify)?;
        Ok(Some(Merged {
            bytes,
            doc_count: merged.doc_count(),
        }))
    }

    /// [`Self::run`], with a panic reported as [`MergeError::Internal`] instead of losing the job.
    fn finish(self) -> Finished {
        let result =
            catch_unwind(AssertUnwindSafe(|| self.run())).unwrap_or(Err(MergeError::Internal {
                what: "a merge panicked",
            }));
        Finished { job: self, result }
    }
}

/// The worker threads, started at the first job.
#[derive(Debug)]
pub(crate) struct Workers {
    threads: usize,
    jobs: Option<Sender<MergeJob>>,
    done_tx: Sender<Finished>,
    /// In a `Mutex` only to keep the database `Sync`: the writer reaches it through `&mut self`
    /// ([`Self::receiver`]) and never blocks on the lock. A poisoned mutex is recovered; treating
    /// poison as an empty channel would leave the finished merge uncollected and the inputs busy
    /// forever.
    done: Mutex<Receiver<Finished>>,
    handles: Vec<JoinHandle<()>>,
    /// The cancel flags of the jobs submitted and not yet collected.
    in_flight: Vec<Arc<AtomicBool>>,
}

impl Workers {
    pub fn new(threads: usize) -> Self {
        let (done_tx, done) = mpsc::channel();
        Self {
            threads,
            jobs: None,
            done_tx,
            done: Mutex::new(done),
            handles: Vec::new(),
            in_flight: Vec::new(),
        }
    }

    /// Jobs submitted and not yet collected.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Hands `job` to a worker, or runs it now when there are no threads; its result is collected by
    /// [`Self::try_next`] or [`Self::next`].
    ///
    /// # Errors
    /// A thread that cannot be started; the job is not submitted.
    pub fn submit(&mut self, job: MergeJob) -> std::io::Result<()> {
        let cancel = Arc::clone(&job.cancel);
        if self.threads == 0 {
            self.deliver(job.finish())?;
        } else {
            let jobs = self.start()?;
            if let Err(mpsc::SendError(job)) = jobs.send(job) {
                // Every worker is gone (they exit only when `jobs` closes): finish it here rather than lose it.
                self.deliver(job.finish())?;
            }
        }
        self.in_flight.push(cancel);
        Ok(())
    }

    /// Queues a finished job. Fails only when the receiver is gone, in which case the caller must not count the
    /// job as in flight: nothing will ever collect it, and [`Self::next`] would wait forever.
    fn deliver(&self, finished: Finished) -> std::io::Result<()> {
        self.done_tx
            .send(finished)
            .map_err(|_| std::io::Error::other("the merge result channel is closed"))
    }

    /// The result channel. Recovers a poisoned mutex: the receiver is still usable, and dropping the results on the
    /// floor would stick every in-flight merge.
    fn receiver(&mut self) -> &mut Receiver<Finished> {
        self.done
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The job queue, starting the threads on first use.
    fn start(&mut self) -> std::io::Result<&Sender<MergeJob>> {
        if self.jobs.is_none() {
            let (tx, rx) = mpsc::channel::<MergeJob>();
            let rx = Arc::new(Mutex::new(rx));
            for n in 0..self.threads {
                let rx = Arc::clone(&rx);
                let done = self.done_tx.clone();
                let handle = std::thread::Builder::new()
                    .name(format!("persia-merge-{n}"))
                    .spawn(move || work(&rx, &done))?;
                self.handles.push(handle);
            }
            self.jobs = Some(tx);
        }
        // Set just above.
        self.jobs
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no merge queue"))
    }

    /// A finished job, if one is waiting.
    pub fn try_next(&mut self) -> Option<Finished> {
        match self.receiver().try_recv() {
            Ok(finished) => Some(self.collected(finished)),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// The next finished job, waiting for one; `None` when none is in flight.
    pub fn next(&mut self) -> Option<Finished> {
        if self.in_flight.is_empty() {
            return None;
        }
        // `done_tx` lives in `self`, so the channel never disconnects while a job is in flight.
        self.receiver()
            .recv()
            .ok()
            .map(|finished| self.collected(finished))
    }

    fn collected(&mut self, finished: Finished) -> Finished {
        self.in_flight
            .retain(|c| !Arc::ptr_eq(c, &finished.job.cancel));
        finished
    }

    /// Asks every job in flight to stop.
    pub fn cancel_all(&self) {
        for cancel in &self.in_flight {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for Workers {
    /// Cancels what is in flight, closes the queue and joins the threads, so no worker outlives the database.
    fn drop(&mut self) {
        self.cancel_all();
        self.jobs = None;
        for handle in self.handles.drain(..) {
            // A worker that panicked outside a job has nothing left to report.
            let _joined = handle.join();
        }
    }
}

/// A worker: runs jobs until the queue closes.
fn work(jobs: &Mutex<Receiver<MergeJob>>, done: &Sender<Finished>) {
    loop {
        let job = match jobs.lock() {
            Ok(rx) => rx.recv(),
            // Another worker panicked while holding the lock, which `recv` cannot do: stop.
            Err(_) => return,
        };
        let Ok(job) = job else { return };
        if done.send(job.finish()).is_err() {
            // The database is going away (it dropped the receiver): nothing is left to commit.
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use persia_analysis::AnalyzerRegistry;

    use super::*;
    use crate::manifest::SegmentId;
    use crate::schema::SchemaBuilder;
    use crate::segment::WriteOptions;

    /// A drop plan finishes without reading inputs: enough to put one result on the channel.
    fn drop_job() -> MergeJob {
        let schema = SchemaBuilder::new("a").build().expect("schema");
        MergeJob {
            collection: "a".to_owned(),
            plan: MergePlan {
                reason: MergeReason::Drop,
                inputs: Vec::new(),
            },
            schema: Arc::new(schema),
            registry: AnalyzerRegistry::builtin(),
            inputs: Vec::new(),
            segment_id: SegmentId::new(1),
            created_at: 0,
            write_options: WriteOptions::default(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn a_poisoned_result_channel_still_delivers_the_merge() {
        let mut workers = Workers::new(0);
        let _panicked = catch_unwind(AssertUnwindSafe(|| {
            let _guard = workers.done.lock().expect("lock");
            panic!("poison the result channel");
        }));
        assert!(workers.done.is_poisoned());
        workers.submit(drop_job()).expect("submit");
        let finished = workers
            .try_next()
            .expect("a poisoned channel must not swallow the result");
        assert!(finished.result.is_ok());
        assert_eq!(workers.in_flight(), 0);
    }
}
