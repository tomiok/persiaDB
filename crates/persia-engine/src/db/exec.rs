//! How a search over a collection runs (SPEC §7.3, ROADMAP 7.6): its views as tasks on a thread pool, the pruning
//! thresholds they share, and when it stops.
//!
//! Each view keeps its own best hits; they merge in view order, so the hits do not depend on which task finished
//! first. A view that keeps `k` hits publishes the worst one's score `t`. Another view may skip a document scoring at
//! most `t` when it is a later view (the published hits win its ties, SPEC §7.3), and only one scoring below `t`
//! when it is an earlier one (its tie would win). Once the views finished are a prefix of the view order, the `k`-th
//! best score of all their hits merged is shared too, and every view still running may skip at most that (ROADMAP
//! 7.6.4): on the calling thread each view starts from every earlier hit. Pruning never changes the hits, so neither
//! does the timing.

use core::ops::Bound;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use super::clock::Clock;
use super::views::CollectionViews;
use crate::schema::FieldId;
use crate::search::{
    Expansion, Hit, Interrupt, Interruptible, Interrupts, Pruning, TermSource, Watch,
};
use crate::segment::ReadError;

/// Where the views of a search run.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum Threads {
    /// On the calling thread, one view after another.
    Current,
    /// On rayon's global pool (one thread per CPU unless configured otherwise). Called from a thread of another
    /// rayon pool (a [`SearchPool`] or the caller's own), on that pool instead.
    #[default]
    Global,
    /// On this pool.
    Pool(SearchPool),
}

/// A thread pool searches run on ([`Threads::Pool`]). Clones share the threads.
#[derive(Debug, Clone)]
pub struct SearchPool(Arc<rayon::ThreadPool>);

/// The threads of a [`SearchPool`] could not be started.
#[derive(Debug, thiserror::Error)]
#[error("cannot start the search threads: {0}")]
pub struct PoolError(String);

impl SearchPool {
    /// A pool of `threads` threads.
    ///
    /// ```
    /// use std::num::NonZeroUsize;
    /// use persia_engine::db::{Execution, SearchPool, Threads};
    ///
    /// let pool = SearchPool::new(NonZeroUsize::new(2).ok_or("zero")?)?;
    /// assert_eq!(pool.threads(), 2);
    /// // Clones share the threads: one pool for every search.
    /// let exec = Execution::default().threads(Threads::Pool(pool.clone()));
    /// # let _ = exec;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// The operating system did not start the threads.
    pub fn new(threads: NonZeroUsize) -> Result<Self, PoolError> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads.get())
            .thread_name(|i| format!("persia-search-{i}"))
            .build()
            .map_err(|e| PoolError(e.to_string()))?;
        Ok(Self(Arc::new(pool)))
    }

    /// Its threads.
    pub fn threads(&self) -> usize {
        self.0.current_num_threads()
    }
}

/// Stops the searches it is given to once [`Self::cancel`] is called (ROADMAP 7.6.2). Clones share the state, so one
/// thread can cancel a search another runs.
///
/// ```
/// use persia_engine::db::CancelToken;
///
/// let token = CancelToken::new();
/// let other = token.clone();
/// std::thread::spawn(move || other.cancel()).join().map_err(|_| "panicked")?;
/// assert!(token.is_cancelled());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels every search given this token or a clone: each stops with [`Interrupt::Cancelled`] within 256
    /// matches of a view, or 1024 reads of one of its postings, terms or columns (ROADMAP 7.6.3).
    pub fn cancel(&self) {
        // Relaxed: nothing is published through the flag, a search only has to see it soon.
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether [`Self::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// How a search over a collection runs: pruning, threads, a deadline and a cancellation token (SPEC §7.3, ROADMAP
/// 7.6). The default prunes with [`Pruning::default`] on [`Threads::Global`], with neither a deadline nor a token.
///
/// ```
/// use std::sync::Arc;
/// use std::time::Duration;
/// use persia_engine::db::{CancelToken, Clock, Execution, ManualClock, Threads};
/// use persia_engine::search::Pruning;
///
/// let clock = Arc::new(ManualClock::new(0));
/// let at = clock.now() + Duration::from_millis(50);
/// let token = CancelToken::new();
/// let exec = Execution::new(Pruning::default())
///     .threads(Threads::Current)
///     .deadline(clock, at)
///     .cancel(token.clone());
/// assert!(!token.is_cancelled());
/// # let _ = exec;
/// ```
#[derive(Debug, Clone, Default)]
pub struct Execution {
    pub(super) pruning: Pruning,
    pub(super) threads: Threads,
    deadline: Option<(Arc<dyn Clock>, Instant)>,
    cancel: Option<CancelToken>,
    /// Runs the views on the calling thread, the last first, so later views publish before earlier ones search.
    #[cfg(test)]
    pub(super) last_first: bool,
}

impl Execution {
    /// Pruning with `pruning`, on [`Threads::Global`], with neither a deadline nor a token.
    pub fn new(pruning: Pruning) -> Self {
        Self {
            pruning,
            ..Self::default()
        }
    }

    /// Runs the views on `threads`.
    #[must_use]
    pub fn threads(mut self, threads: Threads) -> Self {
        self.threads = threads;
        self
    }

    /// Stops the search with [`Interrupt::DeadlineExceeded`] once `clock` reads `at` or later. The clock is read
    /// before each view, every 256 matches of one, and every 1024 reads of one of its postings, terms or columns
    /// (SPEC §7.3).
    #[must_use]
    pub fn deadline(mut self, clock: Arc<dyn Clock>, at: Instant) -> Self {
        self.deadline = Some((clock, at));
        self
    }

    /// Stops the search with [`Interrupt::Cancelled`] once `token` is cancelled.
    #[must_use]
    pub fn cancel(mut self, token: CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// The interrupt in force, if any: a cancellation first, then the deadline.
    pub(super) fn interrupt(&self) -> Result<(), Interrupt> {
        if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Err(Interrupt::Cancelled);
        }
        if let Some((clock, at)) = &self.deadline
            && clock.now() >= *at
        {
            return Err(Interrupt::DeadlineExceeded);
        }
        Ok(())
    }

    /// The terms of `views` for [`crate::search::SearchPlan::with_terms`], walked under this execution's deadline
    /// and token (ROADMAP 7.6.3): planning a `prefix` or `fuzzy` then stops with
    /// [`crate::search::SearchError::Interrupted`] too.
    ///
    /// ```
    /// use persia_engine::db::{CancelToken, Database, DatabaseOptions, Execution};
    /// use persia_engine::query::{PrefixQuery, Query};
    /// use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder};
    /// use persia_engine::search::{Interrupt, SearchError, SearchPlan};
    /// use persia_storage::container::sim::SimFile;
    ///
    /// let schema = SchemaBuilder::new("c").field("tag", FieldKind::Keyword(KeywordOptions::default())).build()?;
    /// let tag = schema.field("tag").map(|f| f.id()).ok_or("no field")?;
    /// let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default())?;
    /// db.create_collection(schema)?;
    /// let snapshot = db.snapshot();
    /// let c = snapshot.collection("c").ok_or("no collection")?;
    /// let memtable = c.memtable();
    /// let views = c.views(&memtable)?;
    /// let token = CancelToken::new();
    /// token.cancel();
    /// let exec = Execution::default().cancel(token);
    /// let query = Query::Prefix(PrefixQuery { field: tag, prefix: "a".into(), max_expansions: 50 });
    /// let planned = SearchPlan::with_terms(&query, c.schema(), &c.search_statistics(&memtable), &exec.terms(&views));
    /// assert!(matches!(planned, Err(SearchError::Interrupted(Interrupt::Cancelled))));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn terms<'a>(&'a self, views: &'a CollectionViews<'a>) -> ExecTerms<'a> {
        ExecTerms { exec: self, views }
    }

    /// Runs `task` for each of `views` (view indices, ascending), on [`Self::threads`], and returns their results in
    /// that order, or the error of the first view in that order that fails. Whether a view fails can depend on the
    /// timing (a pruned view does not read the blocks a threshold from another view skips, SPEC §7.3); hits returned
    /// without an error do not. On the calling thread (always for a single view) no view starts after one fails.
    pub(super) fn run<T: Send, E: Send>(
        &self,
        views: &[usize],
        task: impl Fn(usize) -> Result<T, E> + Sync + Send,
    ) -> Result<Vec<T>, E> {
        use rayon::prelude::*;
        #[cfg(test)]
        if self.last_first {
            let mut found = views
                .iter()
                .rev()
                .copied()
                .map(task)
                .collect::<Result<Vec<T>, E>>()?;
            found.reverse();
            return Ok(found);
        }
        let all: Vec<Result<T, E>> = match &self.threads {
            _ if views.len() <= 1 => return views.iter().copied().map(task).collect(),
            Threads::Current => return views.iter().copied().map(task).collect(),
            Threads::Global => views.par_iter().copied().map(task).collect(),
            Threads::Pool(pool) => pool
                .0
                .install(|| views.par_iter().copied().map(task).collect()),
        };
        all.into_iter().collect()
    }
}

impl Interrupts for Execution {
    fn check(&self) -> Result<(), Interrupt> {
        self.interrupt()
    }
}

/// The terms of a collection's views, walked under an [`Execution`]'s interrupts ([`Execution::terms`]).
#[derive(Debug, Clone, Copy)]
pub struct ExecTerms<'a> {
    exec: &'a Execution,
    views: &'a CollectionViews<'a>,
}

impl TermSource for ExecTerms<'_> {
    fn each_term(
        &self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        expansion: &Expansion,
        each: &mut dyn FnMut(&[u8]) -> Result<(), ReadError>,
    ) -> Result<(), ReadError> {
        // Read once first: a walk shorter than one look stops too.
        self.exec.interrupt().map_err(ReadError::Interrupted)?;
        for segment in self.views.segments() {
            Interruptible::new(&segment, self.exec)
                .each_term(field, lower, upper, expansion, each)?;
        }
        Interruptible::new(self.views.memtable(), self.exec)
            .each_term(field, lower, upper, expansion, each)
    }
}

/// The thresholds the views of a search share. Each view's own: the worst score it keeps once it keeps `k`. And
/// one merged over the views finished so far when they are a prefix of the view order (ROADMAP 7.6.4): the `k`-th
/// best score of all their hits together, which a later view may skip at most (those hits win its ties). On the
/// calling thread the views finish in order, so each starts from every earlier hit, as one sequential search would.
///
/// A score is stored as `bits + 1` of its (non-negative) `f64`, 0 before one is: the bits of non-negative floats
/// order as the floats do, so `fetch_max` keeps the highest.
#[derive(Debug)]
pub(super) struct Thresholds {
    views: Vec<AtomicU64>,
    merged: AtomicU64,
    finished: Mutex<Finished>,
}

/// The views finished so far and the best scores of the prefix of them.
#[derive(Debug)]
struct Finished {
    k: usize,
    /// Per view, its kept scores once it finished (none for a view that does not run).
    scores: Vec<Option<Vec<f64>>>,
    /// Views `0..prefix` have finished.
    prefix: usize,
    /// The best `k` scores of views `0..prefix`, descending.
    best: Vec<f64>,
}

impl Thresholds {
    /// The thresholds of `views` views, of which `run` (ascending) run, each keeping at most `k` hits.
    pub(super) fn new(views: usize, run: &[usize], k: usize) -> Self {
        let mut scores = vec![Some(Vec::new()); views];
        for &view in run {
            if let Some(slot) = scores.get_mut(view) {
                *slot = None;
            }
        }
        let mut finished = Finished {
            k,
            scores,
            prefix: 0,
            best: Vec::new(),
        };
        let merged = AtomicU64::new(0);
        finished.extend(&merged);
        Self {
            views: (0..views).map(|_| AtomicU64::new(0)).collect(),
            merged,
            finished: Mutex::new(finished),
        }
    }

    /// The watch of view `view`.
    pub(super) fn watch<'a>(&'a self, exec: &'a Execution, view: usize) -> ViewWatch<'a> {
        ViewWatch {
            exec,
            thresholds: self,
            view,
        }
    }

    fn get(slot: &AtomicU64) -> Option<f64> {
        slot.load(Ordering::Relaxed)
            .checked_sub(1)
            .map(f64::from_bits)
    }
}

impl Finished {
    /// Takes in the views finished right after the prefix, and publishes the prefix's `k`-th best score to `merged`.
    fn extend(&mut self, merged: &AtomicU64) {
        let start = self.prefix;
        while let Some(Some(scores)) = self.scores.get_mut(self.prefix) {
            let scores = core::mem::take(scores);
            self.best.extend(scores);
            self.prefix += 1;
        }
        if self.prefix == start {
            return;
        }
        self.best.sort_by(|a, b| b.total_cmp(a));
        self.best.truncate(self.k);
        if self.k > 0
            && self.best.len() == self.k
            && let Some(&t) = self.best.last()
        {
            store_max(merged, t);
        }
    }
}

/// Raises `slot` to `score`. Scores are at least 0; a NaN or a negative one is not stored (it would order wrongly as
/// bits).
fn store_max(slot: &AtomicU64, score: f64) {
    let score = score + 0.0;
    if score >= 0.0 {
        slot.fetch_max(score.to_bits().saturating_add(1), Ordering::Relaxed);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A view's scores are moved in whole under the lock, so a panic elsewhere cannot leave them half written.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A view's [`Watch`]: the interrupts of its [`Execution`] and the thresholds of the other views.
#[derive(Debug)]
pub(super) struct ViewWatch<'a> {
    exec: &'a Execution,
    thresholds: &'a Thresholds,
    view: usize,
}

impl Watch for ViewWatch<'_> {
    fn check(&mut self) -> Result<Option<f64>, Interrupt> {
        self.exec.interrupt()?;
        // A running view is not in the finished prefix, so it comes after it: skip at most the merged threshold.
        let mut best = Thresholds::get(&self.thresholds.merged);
        for (i, slot) in self.thresholds.views.iter().enumerate() {
            let Some(t) = Thresholds::get(slot) else {
                continue;
            };
            // An earlier view's hits win a tie with this view's: skip only below them.
            let t = match i.cmp(&self.view) {
                core::cmp::Ordering::Less => t,
                core::cmp::Ordering::Greater => t.next_down(),
                core::cmp::Ordering::Equal => continue,
            };
            best = Some(best.map_or(t, |b| b.max(t)));
        }
        Ok(best)
    }

    fn publish(&mut self, score: f64) {
        if let Some(slot) = self.thresholds.views.get(self.view) {
            store_max(slot, score);
        }
    }

    fn finish(&mut self, hits: &[Hit]) {
        let mut finished = lock(&self.thresholds.finished);
        if let Some(slot @ None) = finished.scores.get_mut(self.view) {
            *slot = Some(hits.iter().map(|h| h.score).collect());
            finished.extend(&self.thresholds.merged);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(scores: &[f64]) -> Vec<Hit> {
        (0..)
            .zip(scores)
            .map(|(docid, &score)| Hit { docid, score })
            .collect()
    }

    #[test]
    fn the_finished_prefix_merges_its_hits() {
        let exec = Execution::default();
        // Views 0 to 3, k = 2; view 2 has no document and does not run.
        let thresholds = Thresholds::new(4, &[0, 1, 3], 2);
        // View 1 finishes first: not a prefix yet, so nothing is merged.
        thresholds.watch(&exec, 1).finish(&hits(&[4.0, 3.0]));
        assert_eq!(thresholds.watch(&exec, 3).check(), Ok(None));
        // View 0 finishes: views 0 to 2 are the prefix, and their 2nd best score of all is 4.0, above either view's own.
        thresholds.watch(&exec, 0).finish(&hits(&[5.0, 1.0]));
        assert_eq!(thresholds.watch(&exec, 3).check(), Ok(Some(4.0)));
        // A view's own published threshold still counts where it is higher.
        thresholds.watch(&exec, 0).publish(4.5);
        assert_eq!(thresholds.watch(&exec, 3).check(), Ok(Some(4.5)));
    }

    #[test]
    fn a_prefix_with_fewer_than_k_hits_merges_nothing() {
        let exec = Execution::default();
        let thresholds = Thresholds::new(3, &[0, 1, 2], 3);
        thresholds.watch(&exec, 0).finish(&hits(&[5.0]));
        thresholds.watch(&exec, 1).finish(&hits(&[4.0]));
        assert_eq!(thresholds.watch(&exec, 2).check(), Ok(None));
        // A second finish of a view is ignored.
        thresholds.watch(&exec, 1).finish(&hits(&[9.0, 9.0, 9.0]));
        assert_eq!(thresholds.watch(&exec, 2).check(), Ok(None));
    }

    #[test]
    fn views_that_do_not_run_are_finished_from_the_start() {
        let exec = Execution::default();
        let thresholds = Thresholds::new(3, &[1], 1);
        assert_eq!(thresholds.watch(&exec, 1).check(), Ok(None));
        thresholds.watch(&exec, 1).finish(&hits(&[2.0]));
        // Views 0 to 2 are all finished.
        assert_eq!(lock(&thresholds.finished).prefix, 3);
    }

    #[test]
    fn a_view_skips_at_most_an_earlier_views_threshold_and_below_a_later_ones() {
        let exec = Execution::default();
        let thresholds = Thresholds::new(3, &[0, 1, 2], 1);
        thresholds.watch(&exec, 0).publish(2.0);
        thresholds.watch(&exec, 2).publish(5.0);
        // View 1: at most 2.0 from view 0, below 5.0 from view 2.
        assert_eq!(
            thresholds.watch(&exec, 1).check(),
            Ok(Some(5.0_f64.next_down()))
        );
        // A lower publish does not lower the threshold.
        thresholds.watch(&exec, 2).publish(1.0);
        assert_eq!(
            thresholds.watch(&exec, 1).check(),
            Ok(Some(5.0_f64.next_down()))
        );
        // View 2 skips at most view 0's; its own is not counted.
        assert_eq!(thresholds.watch(&exec, 2).check(), Ok(Some(2.0)));
        // View 0 skips only below the others.
        assert_eq!(
            thresholds.watch(&exec, 0).check(),
            Ok(Some(5.0_f64.next_down()))
        );
    }

    #[test]
    fn zero_is_a_threshold_and_nan_is_not_published() {
        let exec = Execution::default();
        let thresholds = Thresholds::new(2, &[0, 1], 1);
        assert_eq!(thresholds.watch(&exec, 1).check(), Ok(None));
        thresholds.watch(&exec, 0).publish(f64::NAN);
        thresholds.watch(&exec, 0).publish(-1.0);
        assert_eq!(thresholds.watch(&exec, 1).check(), Ok(None));
        thresholds.watch(&exec, 0).publish(-0.0);
        assert_eq!(thresholds.watch(&exec, 1).check(), Ok(Some(0.0)));
        // An earlier view skips only below 0: nothing a score can be.
        thresholds.watch(&exec, 1).publish(0.0);
        let t = thresholds.watch(&exec, 0).check().unwrap().unwrap();
        assert!(t < 0.0);
    }

    #[test]
    fn a_cancellation_wins_over_a_passed_deadline() {
        let clock = Arc::new(super::super::ManualClock::new(0));
        let token = CancelToken::new();
        let exec = Execution::default()
            .deadline(clock.clone(), clock.now())
            .cancel(token.clone());
        assert_eq!(exec.interrupt(), Err(Interrupt::DeadlineExceeded));
        token.cancel();
        assert_eq!(exec.interrupt(), Err(Interrupt::Cancelled));
    }

    #[test]
    fn the_deadline_is_read_from_the_clock() {
        let clock = Arc::new(super::super::ManualClock::new(0));
        let at = clock.now() + std::time::Duration::from_millis(5);
        let exec = Execution::default().deadline(clock.clone(), at);
        assert_eq!(exec.interrupt(), Ok(()));
        clock.advance(std::time::Duration::from_millis(4));
        assert_eq!(exec.interrupt(), Ok(()));
        clock.advance(std::time::Duration::from_millis(1));
        assert_eq!(exec.interrupt(), Err(Interrupt::DeadlineExceeded));
    }
}
