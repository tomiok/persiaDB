//! A search over a whole collection (SPEC §7.3, §8.3, ROADMAP 7.4.6): its segments, each without the documents its
//! committed tombstones delete and the copies the memtable hides, and the memtable as one more segment.
//!
//! Before the commit that flushes it, the memtable's put or delete of an id hides every segment copy of that id,
//! which no tombstone deletes yet. [`CollectionSnapshot::views`] adds those copies to each segment's tombstones, so
//! a search sees each id at most once, as [`CollectionSnapshot::lookup`] does.
//!
//! A snapshot keeps a log of the copies hidden so far, by the version of the operation that first named their id: a
//! view of any version takes a prefix of it, and only a version past the log looks ids up, those first named since.
//! It also keeps the deletions of the newest version searched, which a newer view extends.

use core::ops::Bound;
use std::sync::{Arc, Mutex, PoisonError};

use roaring::RoaringBitmap;

use super::exec::{Execution, Thresholds, ViewWatch};
use super::snapshot::{CollectionSnapshot, Located};
use crate::memtable::MemtableView;
use crate::schema::FieldId;
use crate::search::{
    ExecError, Expansion, Interruptible, Pruning, SearchPlan, Steps, TermSource, collect_where,
};
use crate::segment::{ReadError, Tombstones};
use crate::view::{SearchView, SegmentView};

/// Each segment's deletions as of a memtable version: its committed tombstones plus the docids of the ids
/// operations `1..=version` named (SPEC §8.3).
#[derive(Debug)]
pub(super) struct Hidden {
    version: u64,
    /// By the segment's index in [`CollectionSnapshot::segments`]; `None` for a segment without any.
    tombstones: Vec<Option<Arc<Tombstones>>>,
}

/// The segment copies the memtable hides that no committed tombstone deletes, as `(version, segment index, docid)`,
/// `version` the one of the operation that first named the id; in version order, every id first named through
/// `version` in it.
#[derive(Debug, Default)]
pub(super) struct HiddenLog {
    version: u64,
    copies: Vec<(u64, usize, u32)>,
}

/// What a snapshot keeps of the copies its memtable hides.
#[derive(Debug, Default)]
pub(super) struct HiddenState {
    log: Arc<HiddenLog>,
    /// The deletions of the newest version searched.
    newest: Option<Arc<Hidden>>,
}

/// A snapshot's [`HiddenState`].
pub(super) type HiddenCache = Arc<Mutex<HiddenState>>;

/// A hit of a search over a collection (ROADMAP 7.4.6): where its document is, and its score (SPEC §7.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollectionHit {
    /// The document: a segment's (by its index in [`CollectionSnapshot::segments`]) or the memtable's.
    pub location: Located,
    /// Its score.
    pub score: f64,
}

/// What a search over a collection runs on, as of one view of its memtable ([`CollectionSnapshot::views`]): each
/// segment with every document deleted or hidden left out, then the memtable.
#[derive(Debug, Clone)]
pub struct CollectionViews<'a> {
    snapshot: &'a CollectionSnapshot,
    memtable: &'a MemtableView,
    hidden: Arc<Hidden>,
}

impl CollectionSnapshot {
    /// The views a search over the collection runs on as of `memtable`, a view of [`Self::memtable`] (SPEC §8.3,
    /// ROADMAP 7.4.6): each segment without the documents its committed tombstones delete, nor the copies of ids
    /// `memtable` has a put or a delete of, then `memtable`. Writes after `memtable` was taken hide nothing.
    ///
    /// The first view past every version searched before looks each id first named since up in every segment's
    /// `IdMap`; any other reuses what the snapshot kept (the module docs). A view of another memtable is computed
    /// whole and leaves the snapshot's cache alone.
    ///
    /// ```
    /// use persia_engine::db::{Database, DatabaseOptions, Located};
    /// use persia_engine::document::{Document, Value};
    /// use persia_engine::query::{Query, TermQuery};
    /// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    /// use persia_engine::search::{Pruning, SearchPlan};
    /// use persia_engine::wal::Durability;
    /// use persia_storage::container::sim::SimFile;
    ///
    /// let schema = SchemaBuilder::new("notes").field("body", FieldKind::Text(TextOptions::default())).build()?;
    /// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
    /// let note = |text: &str| Document::new("n1").with(body, Value::String(text.into()));
    /// let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default())?;
    /// db.create_collection(schema)?;
    /// db.put("notes", &note("red shoes"), Durability::Fsync)?;
    /// db.commit()?;
    /// // The upsert hides the committed copy before its tombstone is committed.
    /// db.put("notes", &note("red hat"), Durability::Fsync)?;
    ///
    /// let snapshot = db.snapshot();
    /// let notes = snapshot.collection("notes").ok_or("no collection")?;
    /// let memtable = notes.memtable();
    /// let query = Query::Term(TermQuery { field: body, term: "red".into() });
    /// let plan = SearchPlan::new(&query, notes.schema(), &notes.search_statistics(&memtable))?;
    /// let hits = notes.views(&memtable)?.top_k(&plan, 10, Pruning::default())?;
    /// assert_eq!(hits.iter().map(|h| h.location).collect::<Vec<_>>(), [Located::Memtable(0)]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// A corruption of a segment's `IdMap`.
    pub fn views<'a>(
        &'a self,
        memtable: &'a MemtableView,
    ) -> Result<CollectionViews<'a>, ReadError> {
        let version = memtable.version();
        let views = |hidden| CollectionViews {
            snapshot: self,
            memtable,
            hidden,
        };
        if !memtable.is_of(&self.memtable) {
            // Another memtable's versions mean nothing to this snapshot's cache: computed whole, and not kept.
            let log = self.extend(&HiddenLog::default(), memtable)?;
            return Ok(views(Arc::new(self.materialize(None, &log, version)?)));
        }
        let (log, newest) = {
            let state = lock(&self.hidden);
            (Arc::clone(&state.log), state.newest.clone())
        };
        if let Some(hidden) = newest.as_ref().filter(|h| h.version == version) {
            return Ok(views(Arc::clone(hidden)));
        }
        // The lookups run without the lock: a search on another thread is not held up by them.
        let log = if log.version < version {
            let log = Arc::new(self.extend(&log, memtable)?);
            let mut state = lock(&self.hidden);
            if state.log.version < log.version {
                state.log = Arc::clone(&log);
            }
            log
        } else {
            log
        };
        let base = newest.filter(|h| h.version < version);
        let hidden = Arc::new(self.materialize(base.as_deref(), &log, version)?);
        let mut state = lock(&self.hidden);
        if state.newest.as_ref().is_none_or(|n| n.version < version) {
            state.newest = Some(Arc::clone(&hidden));
        }
        Ok(views(hidden))
    }

    /// `log` extended through `memtable`'s version: the segment copies of the ids first named after `log`'s.
    fn extend(&self, log: &HiddenLog, memtable: &MemtableView) -> Result<HiddenLog, ReadError> {
        // A copy per new version; the log holds one entry per hidden copy, far fewer than the operations.
        let mut copies = log.copies.clone();
        for (version, id) in memtable.ids_first_named_after(log.version) {
            for (index, (segment, deleted)) in
                self.segments.iter().zip(&self.tombstones).enumerate()
            {
                if let Some(docid) = segment.docid_of(&id)?
                    && !deleted.as_ref().is_some_and(|t| t.contains(docid))
                {
                    copies.push((version, index, docid));
                }
            }
        }
        Ok(HiddenLog {
            version: memtable.version(),
            copies,
        })
    }

    /// The deletions as of `version`: `base` (or the commit's tombstones) plus the copies of `log` hidden after
    /// `base`'s version and through `version`, which `log` must cover.
    fn materialize(
        &self,
        base: Option<&Hidden>,
        log: &HiddenLog,
        version: u64,
    ) -> Result<Hidden, ReadError> {
        let from = base.map_or(0, |b| b.version);
        let mut tombstones = base.map_or_else(|| self.tombstones.clone(), |b| b.tombstones.clone());
        let start = log.copies.partition_point(|&(v, ..)| v <= from);
        let end = log.copies.partition_point(|&(v, ..)| v <= version);
        let mut added: Vec<Option<RoaringBitmap>> = vec![None; tombstones.len()];
        for &(_, index, docid) in log.copies.get(start..end).unwrap_or_default() {
            let deleted = tombstones.get(index).and_then(Option::as_deref);
            if let Some(slot) = added.get_mut(index) {
                slot.get_or_insert_with(|| deleted.map(|t| t.docids().clone()).unwrap_or_default())
                    .insert(docid);
            }
        }
        for ((segment, deleted), added) in self.segments.iter().zip(&mut tombstones).zip(added) {
            if let Some(all) = added {
                *deleted = Some(Arc::new(segment.tombstones(all)?));
            }
        }
        Ok(Hidden {
            version,
            tombstones,
        })
    }
}

/// The terms of every segment, then of the memtable: what a `prefix` or `fuzzy` planned for these views expands
/// over ([`SearchPlan::with_terms`], SPEC §7.1).
impl TermSource for CollectionViews<'_> {
    fn each_term(
        &self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        expansion: &Expansion,
        each: &mut dyn FnMut(&[u8]) -> Result<(), ReadError>,
    ) -> Result<(), ReadError> {
        for segment in self.segments() {
            segment.each_term(field, lower, upper, expansion, each)?;
        }
        self.memtable()
            .each_term(field, lower, upper, expansion, each)
    }
}

impl CollectionViews<'_> {
    /// Each segment with its deletions and the copies the memtable hides, in the order of
    /// [`CollectionSnapshot::segments`].
    pub fn segments(&self) -> impl ExactSizeIterator<Item = SegmentView<'_>> {
        self.snapshot
            .segments
            .iter()
            .zip(&self.hidden.tombstones)
            .map(|(s, t)| SegmentView::new(s, t.as_deref()))
    }

    /// The views of `segments` (these views' [`Self::segments`]) and the memtable, `segments.len()`, that have a
    /// document: an empty one matches nothing, and is not worth a task. `doc_count` counts deleted documents, so only a
    /// view with no document at all is left out (and its dictionary is not read).
    pub(super) fn with_documents(&self, segments: &[SegmentView<'_>]) -> Vec<usize> {
        let memtable = (self.memtable.doc_count() > 0).then_some(segments.len());
        (0..segments.len())
            .filter(|&i| segments.get(i).is_some_and(|s| s.doc_count() > 0))
            .chain(memtable)
            .collect()
    }

    /// The memtable.
    pub fn memtable(&self) -> &MemtableView {
        self.memtable
    }

    /// The commit that published the snapshot these views belong to ([`super::Snapshot::seq`]).
    pub(crate) fn snapshot_seq(&self) -> u64 {
        self.snapshot.seq
    }

    /// The collection these views search.
    pub(crate) fn collection_name(&self) -> &str {
        self.snapshot.schema().name()
    }

    /// The best `k` hits of `plan` in the collection, pruning with `pruning` (ROADMAP 7.4.3, 7.4.4): score
    /// descending, then the segments in the order of [`CollectionSnapshot::segments`] and the memtable last, then
    /// docid. `plan` should be planned with [`CollectionSnapshot::search_statistics`] of the same memtable view.
    /// As [`Self::top_k_with`] with [`Execution::new`]`(pruning)`: the views run on rayon's global pool.
    ///
    /// # Errors
    /// As [`Self::top_k_with`]; never [`ExecError::Interrupted`].
    pub fn top_k(
        &self,
        plan: &SearchPlan,
        k: usize,
        pruning: Pruning,
    ) -> Result<Vec<CollectionHit>, ExecError> {
        self.top_k_with(plan, k, &Execution::new(pruning))
    }

    /// As [`Self::top_k`], run as `exec` says (SPEC §7.3, ROADMAP 7.6): each view a task on its threads, keeping
    /// its own best `k` and sharing its threshold with the others (the [`super::exec`] docs). The hits merge in view
    /// order, so they are the same whatever the threads, the timing or the pruning.
    ///
    /// # Errors
    /// [`ExecError::Read`] as [`SearchPlan::matcher`], for the first view in order that fails;
    /// [`ExecError::Interrupted`] once `exec`'s token is cancelled or its deadline passes.
    pub fn top_k_with(
        &self,
        plan: &SearchPlan,
        k: usize,
        exec: &Execution,
    ) -> Result<Vec<CollectionHit>, ExecError> {
        // Read once before any view: a search with no view to run, or no hit to keep, stops too.
        exec.interrupt()?;
        if k == 0 {
            return Ok(Vec::new());
        }
        let segments: Vec<_> = self.segments().collect();
        let run = self.with_documents(&segments);
        let thresholds = Thresholds::new(segments.len().saturating_add(1), &run, k);
        let found = exec.run(&run, |index| {
            let mut watch = thresholds.watch(exec, index);
            match segments.get(index) {
                Some(view) => view_top_k(plan, view, k, exec, &mut watch, |docid| {
                    Located::Segment { index, docid }
                }),
                None => view_top_k(plan, self.memtable, k, exec, &mut watch, Located::Memtable),
            }
        });
        let mut hits: Vec<_> = found?.into_iter().flatten().collect();
        // Stable: on a tie the earlier view's hit stays first, and a view's own hits are already in docid order.
        hits.sort_by(|a, b| (b.score + 0.0).total_cmp(&(a.score + 0.0)));
        hits.truncate(k);
        Ok(hits)
    }
}

/// The best `k` hits of `plan` in `view`, best first, its reads stopped by `exec`'s interrupts.
fn view_top_k<V: SearchView>(
    plan: &SearchPlan,
    view: &V,
    k: usize,
    exec: &Execution,
    watch: &mut ViewWatch<'_>,
    location: impl Fn(u32) -> Located,
) -> Result<Vec<CollectionHit>, ExecError> {
    let steps = Steps::new(exec);
    let view = Interruptible::new(view, &steps);
    let mut matcher = plan.matcher_with(&view, exec.pruning)?;
    Ok(collect_where(&mut matcher, k, true, |_, _| true, watch)?
        .into_iter()
        .map(|h| CollectionHit {
            location: location(h.docid),
            score: h.score,
        })
        .collect())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // The cache is only ever replaced whole, so a panic elsewhere cannot leave it half written.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DatabaseOptions, SearchRequest};
    use crate::document::{Document, Value};
    use crate::query::{Query, TermQuery};
    use crate::schema::{FieldKind, KeywordOptions, SchemaBuilder};
    use crate::wal::Durability;
    use persia_storage::container::sim::SimFile;

    /// Two segments and the memtable, each holding one document of the same score.
    fn three_tied_views() -> (Database<SimFile>, Query) {
        let schema = SchemaBuilder::new("c")
            .field("tag", FieldKind::Keyword(KeywordOptions::default()))
            .build()
            .unwrap();
        let tag = schema.field("tag").unwrap().id();
        let mut db =
            Database::create_on(SimFile::new(), [3; 16], DatabaseOptions::default()).unwrap();
        db.create_collection(schema).unwrap();
        for (id, commit) in [("s0", true), ("s1", true), ("m", false)] {
            let doc = Document::new(id).with(tag, Value::String("a".into()));
            db.put("c", &doc, Durability::Fsync).unwrap();
            if commit {
                db.commit().unwrap();
            }
        }
        let query = Query::Term(TermQuery {
            field: tag,
            term: "a".into(),
        });
        (db, query)
    }

    #[test]
    fn an_earlier_view_keeps_a_tie_with_a_later_views_threshold() {
        let (db, query) = three_tied_views();
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        assert_eq!(c.segments.len(), 2);
        let memtable = c.memtable();
        let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
        let views = c.views(&memtable).unwrap();
        for pruning in [Pruning::MaxScore, Pruning::BlockMaxWand] {
            // The memtable publishes its hit's score first; each segment may skip only below it.
            let mut exec = Execution::new(pruning);
            exec.last_first = true;
            let first = Located::Segment { index: 0, docid: 0 };
            let hits = views.top_k_with(&plan, 1, &exec).unwrap();
            assert_eq!(hits.iter().map(|h| h.location).collect::<Vec<_>>(), [first]);
            let request = SearchRequest {
                limit: 1,
                ..SearchRequest::default()
            };
            let page = views.search_with(&plan, &request, &exec).unwrap();
            assert_eq!(
                page.hits.iter().map(|h| h.location).collect::<Vec<_>>(),
                [first]
            );
            // After the first hit, the tie in segment 1 ranks next: the memtable's published score must not skip it.
            let next = SearchRequest {
                limit: 1,
                search_after: page.hits.first().map(crate::db::SearchHit::cursor),
                ..SearchRequest::default()
            };
            let page = views.search_with(&plan, &next, &exec).unwrap();
            assert_eq!(
                page.hits.iter().map(|h| h.location).collect::<Vec<_>>(),
                [Located::Segment { index: 1, docid: 0 }]
            );
        }
    }
}
