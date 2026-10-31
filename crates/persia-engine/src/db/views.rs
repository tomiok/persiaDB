//! A search over a whole collection (SPEC §7.3, §8.3, ROADMAP 7.4.6): its segments, each without the documents its
//! committed tombstones delete and the copies the memtable hides, and the memtable as one more segment.
//!
//! Before the commit that flushes it, the memtable's put or delete of an id hides every segment copy of that id,
//! which no tombstone deletes yet. [`CollectionSnapshot::views`] adds those copies to each segment's tombstones, so
//! a search sees each id at most once, as [`CollectionSnapshot::lookup`] does. A snapshot keeps what it computed for
//! the newest memtable version it was asked about, and extends it by the operations after that one.

use std::sync::{Arc, Mutex, PoisonError};

use super::snapshot::{CollectionSnapshot, Located};
use crate::memtable::MemtableView;
use crate::search::{Matcher, Pruning, SearchPlan, collect};
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

/// The [`Hidden`] a snapshot computed last.
pub(super) type HiddenCache = Arc<Mutex<Option<Arc<Hidden>>>>;

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
    /// The first call for a memtable version looks each id named since the last one computed up in every
    /// segment's `IdMap`; the snapshot keeps the newest result.
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
        let cached = lock(&self.hidden).clone();
        let base = match cached {
            Some(h) if h.version <= version => h,
            // None yet, or one for a newer view than this one: start from the commit's.
            _ => Arc::new(Hidden {
                version: 0,
                tombstones: self.tombstones.clone(),
            }),
        };
        let hidden = if base.version == version {
            base
        } else {
            // Computed without the lock: a search on another thread is not held up by these lookups.
            let hidden = Arc::new(self.hide(&base, memtable)?);
            let mut slot = lock(&self.hidden);
            if slot.as_ref().is_none_or(|s| s.version < version) {
                *slot = Some(Arc::clone(&hidden));
            }
            hidden
        };
        Ok(CollectionViews {
            snapshot: self,
            memtable,
            hidden,
        })
    }

    /// `base` plus the segment copies of the ids `memtable` named after `base`'s version.
    fn hide(&self, base: &Hidden, memtable: &MemtableView) -> Result<Hidden, ReadError> {
        let ids = memtable.ids_after(base.version);
        let mut tombstones = base.tombstones.clone();
        for (segment, deleted) in self.segments.iter().zip(&mut tombstones) {
            let mut all = None;
            for id in &ids {
                let Some(docid) = segment.docid_of(id)? else {
                    continue;
                };
                if deleted.as_ref().is_some_and(|t| t.contains(docid)) {
                    continue;
                }
                all.get_or_insert_with(|| {
                    deleted
                        .as_ref()
                        .map(|t| t.docids().clone())
                        .unwrap_or_default()
                })
                .insert(docid);
            }
            if let Some(all) = all {
                *deleted = Some(Arc::new(segment.tombstones(all)?));
            }
        }
        Ok(Hidden {
            version: memtable.version(),
            tombstones,
        })
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

    /// The memtable.
    pub fn memtable(&self) -> &MemtableView {
        self.memtable
    }

    /// The best `k` hits of `plan` in the collection, pruning with `pruning` (ROADMAP 7.4.3, 7.4.4): score
    /// descending, then the segments in the order of [`CollectionSnapshot::segments`] and the memtable last, then
    /// docid. `plan` should be planned with [`CollectionSnapshot::search_statistics`] of the same memtable view.
    ///
    /// The views are searched one after the other (ROADMAP 7.6.1 runs them in parallel). Once `k` hits are kept,
    /// the next view starts from the worst one's score as its threshold: every hit kept wins a tie against a later
    /// view's, so the hits are the same whatever `pruning` is.
    ///
    /// # Errors
    /// As [`SearchPlan::matcher`], for any view.
    pub fn top_k(
        &self,
        plan: &SearchPlan,
        k: usize,
        pruning: Pruning,
    ) -> Result<Vec<CollectionHit>, ReadError> {
        let mut hits = Vec::new();
        if k == 0 {
            return Ok(hits);
        }
        for (index, view) in self.segments().enumerate() {
            search_view(plan, &view, k, pruning, &mut hits, |docid| {
                Located::Segment { index, docid }
            })?;
        }
        search_view(
            plan,
            self.memtable,
            k,
            pruning,
            &mut hits,
            Located::Memtable,
        )?;
        Ok(hits)
    }
}

/// Merges the best `k` hits of `plan` in `view` into `hits`, the best `k` of the views before it, best first.
fn search_view<V: SearchView>(
    plan: &SearchPlan,
    view: &V,
    k: usize,
    pruning: Pruning,
    hits: &mut Vec<CollectionHit>,
    location: impl Fn(u32) -> Located,
) -> Result<(), ReadError> {
    let mut matcher = plan.matcher_with(view, pruning)?;
    if hits.len() >= k
        && let Some(worst) = hits.last()
    {
        matcher.set_min_competitive(worst.score);
    }
    hits.extend(
        collect(&mut matcher, k)?
            .into_iter()
            .map(|h| CollectionHit {
                location: location(h.docid),
                score: h.score,
            }),
    );
    // Stable: on a tie the earlier view's hit stays first, and a view's own hits are already in docid order.
    hits.sort_by(|a, b| (b.score + 0.0).total_cmp(&(a.score + 0.0)));
    hits.truncate(k);
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // The cache is only ever replaced whole, so a panic elsewhere cannot leave it half written.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
