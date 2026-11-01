//! A search request over a collection (SPEC §7.4, §7.5, §7.5.1, ROADMAP 7.5): a sort, a page (`offset`/`limit` or
//! a cursor), facets and a projection, run over [`CollectionViews`].
//!
//! Each view keeps its best `offset + limit` hits by the sort, after the cursor when there is one; they merge into
//! the collection's by the sort, then location (segment order, the memtable last, docid). With facets every match is
//! visited, and each view's matching bitmap is counted. The page's hits then get their documents, projected.

use roaring::RoaringBitmap;

use super::exec::{Execution, Thresholds, ViewWatch};
use super::snapshot::Located;
use super::views::CollectionViews;
use crate::bitmap::Ascending;
use crate::document::Document;
use crate::search::{
    Explanation, FacetCounter, FacetCounts, FacetRequest, MAX_FACETS, MAX_WINDOW, Projection,
    Pruning, RequestError, SearchPlan, Sort, SortValue, Ties, Watch, collect_where, top_sorted,
};
use crate::view::{SearchView, SegmentView};

/// The hits a page returns unless asked otherwise (SPEC §7.4).
pub const DEFAULT_LIMIT: usize = 20;

/// What a search returns beside its query (SPEC §7.4, §7.5, §7.5.1). The default is the first 20 hits by score,
/// no facet, every stored field.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    /// How hits rank.
    pub sort: Sort,
    /// Hits skipped from the top (after the cursor, with one).
    pub offset: usize,
    /// Hits returned at most; `offset + limit` is at most [`MAX_WINDOW`].
    pub limit: usize,
    /// Only hits ranking after this one count: the cursor of the last hit of the page before.
    pub search_after: Option<Cursor>,
    /// Facets to count over every match.
    pub facets: Vec<FacetRequest>,
    /// The fields each hit returns.
    pub projection: Projection,
    /// Whether each hit returns how its score adds up ([`SearchHit::explanation`], SPEC §7.7). It costs about one
    /// unpruned search of the hit's view per hit.
    pub explain: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            sort: Sort::score(),
            offset: 0,
            limit: DEFAULT_LIMIT,
            search_after: None,
            facets: Vec::new(),
            projection: Projection::All,
            explain: false,
        }
    }
}

/// The snapshot and memtable view a [`Cursor`] was taken from (SPEC §7.4). A search on another one returns
/// [`RequestError::CursorView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorOrigin {
    /// [`super::Snapshot::seq`] of that snapshot.
    pub seq: u64,
    /// [`crate::memtable::MemtableView::version`] of that view.
    pub version: u64,
    /// The collection the hit came from.
    pub collection: String,
}

/// Where a page starts (SPEC §7.4): a hit's sort values, location and [`CursorOrigin`], from [`SearchHit::cursor`].
/// Exact only for that snapshot, collection and memtable view. Another one is [`RequestError::CursorView`]: locations
/// are indices into the view that returned the hit, so a cursor from elsewhere would repeat a tied hit or pass one
/// over.
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    /// One value per sort key.
    pub values: Vec<SortValue>,
    /// The hit's document.
    pub location: Located,
    /// The snapshot, collection and memtable view that produced the hit.
    pub origin: CursorOrigin,
}

/// A hit of a search (SPEC §7.5.1).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Where its document is.
    pub location: Located,
    /// Its score (SPEC §7.2), sorted by it or not.
    pub score: f64,
    /// Its value for each sort key.
    pub sort: Vec<SortValue>,
    /// Its id, blob reference and projected stored fields.
    pub document: Document,
    /// How its score adds up, when the request set [`SearchRequest::explain`] (SPEC §7.7).
    pub explanation: Option<Explanation>,
    /// The snapshot, collection and memtable view. [`Self::cursor`] copies it.
    pub origin: CursorOrigin,
}

impl SearchHit {
    /// The cursor of the page after this hit, including the view that produced it.
    pub fn cursor(&self) -> Cursor {
        Cursor {
            values: self.sort.clone(),
            location: self.location,
            origin: self.origin.clone(),
        }
    }
}

/// What a search returns.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResults {
    /// The page, best first.
    pub hits: Vec<SearchHit>,
    /// One per requested facet, in request order.
    pub facets: Vec<FacetCounts>,
}

/// A hit before its document is read.
#[derive(Debug, Clone)]
struct Ranked {
    location: Located,
    score: f64,
    values: Vec<SortValue>,
}

impl CollectionViews<'_> {
    /// Runs `plan` with `request` over the collection, pruning with `pruning` where the sort allows it (SPEC §7.3,
    /// §7.4): the page of hits by `request.sort`, each with its projected document, and the facet counts over every
    /// match. `plan` should be planned with [`super::CollectionSnapshot::search_statistics`] of the same memtable
    /// view.
    ///
    /// ```
    /// use persia_engine::db::{Database, DatabaseOptions, SearchRequest};
    /// use persia_engine::document::{Document, Value};
    /// use persia_engine::query::Query;
    /// use persia_engine::schema::{FieldKind, SchemaBuilder};
    /// use persia_engine::search::{Order, Pruning, SearchPlan, Sort, SortKey};
    /// use persia_engine::wal::Durability;
    /// use persia_storage::container::sim::SimFile;
    ///
    /// let schema = SchemaBuilder::new("items").field("price", FieldKind::I64 { stored: true }).build()?;
    /// let price = schema.field("price").map(|f| f.id()).ok_or("no field")?;
    /// let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default())?;
    /// db.create_collection(schema)?;
    /// for (id, p) in [("a", 30), ("b", 10), ("c", 20)] {
    ///     db.put("items", &Document::new(id).with(price, Value::I64(p)), Durability::Fsync)?;
    /// }
    /// let snapshot = db.snapshot();
    /// let items = snapshot.collection("items").ok_or("no collection")?;
    /// let memtable = items.memtable();
    /// let plan = SearchPlan::new(&Query::MatchAll, items.schema(), &items.search_statistics(&memtable))?;
    /// let request = SearchRequest {
    ///     sort: Sort::new([SortKey::field(price, Order::Asc)], items.schema())?,
    ///     limit: 2,
    ///     ..SearchRequest::default()
    /// };
    /// let views = items.views(&memtable)?;
    /// let page = views.search(&plan, &request, Pruning::default())?;
    /// assert_eq!(page.hits.iter().map(|h| h.document.id()).collect::<Vec<_>>(), ["b", "c"]);
    /// // The next page starts after the last hit.
    /// let next = SearchRequest { search_after: page.hits.last().map(|h| h.cursor()), ..request };
    /// let page = views.search(&plan, &next, Pruning::default())?;
    /// assert_eq!(page.hits.iter().map(|h| h.document.id()).collect::<Vec<_>>(), ["a"]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`RequestError::Window`] when `offset + limit` is above [`MAX_WINDOW`]; [`RequestError::Cursor`] for a
    /// cursor that does not fit the sort; [`RequestError::CursorView`] for a cursor from another snapshot,
    /// collection or memtable view; [`RequestError::TooManyFacets`] above [`MAX_FACETS`];
    /// [`RequestError::MismatchedSortValue`] if two sort values are not of the key's type;
    /// [`RequestError::HitSegment`] if a hit names a segment the views do not have (the hit is not skipped);
    /// [`RequestError::Read`] as [`SearchPlan::matcher`], or for a document that cannot be read.
    pub fn search(
        &self,
        plan: &SearchPlan,
        request: &SearchRequest,
        pruning: Pruning,
    ) -> Result<SearchResults, RequestError> {
        self.search_with(plan, request, &Execution::new(pruning))
    }

    /// As [`Self::search`], run as `exec` says (SPEC §7.3, ROADMAP 7.6): each view a task on its threads, keeping
    /// its own best `offset + limit` hits and counting its own facets. They merge in view order, so the page and the
    /// counts are the same whatever the threads or the timing.
    ///
    /// # Errors
    /// As [`Self::search`], for the first view in order that fails; [`RequestError::Interrupted`] once `exec`'s token
    /// is cancelled or its deadline passes, before the page's documents are read.
    pub fn search_with(
        &self,
        plan: &SearchPlan,
        request: &SearchRequest,
        exec: &Execution,
    ) -> Result<SearchResults, RequestError> {
        let window = request
            .offset
            .checked_add(request.limit)
            .filter(|&w| w <= MAX_WINDOW)
            .ok_or(RequestError::Window {
                offset: request.offset,
                limit: request.limit,
                max: MAX_WINDOW,
            })?;
        if let Some(cursor) = &request.search_after {
            request.sort.check(&cursor.values)?;
            let origin = self.cursor_origin();
            if cursor.origin != origin {
                return Err(RequestError::CursorView {
                    cursor_seq: cursor.origin.seq,
                    cursor_version: cursor.origin.version,
                    cursor_collection: cursor.origin.collection.clone(),
                    seq: origin.seq,
                    version: origin.version,
                    collection: origin.collection,
                });
            }
        }
        if request.facets.len() > MAX_FACETS {
            return Err(RequestError::TooManyFacets {
                count: request.facets.len(),
                max: MAX_FACETS,
            });
        }
        let search = Search {
            plan,
            request,
            pruning: exec.pruning,
            window,
        };
        // Read once before any view: a search with no view to run stops too.
        exec.interrupt()?;
        let segments: Vec<_> = self.segments().collect();
        let thresholds = Thresholds::new(segments.len().saturating_add(1));
        let found = exec.run(&self.with_documents(&segments), |index| {
            let mut watch = thresholds.watch(exec, index);
            match segments.get(index) {
                Some(view) => search.view(view, index, &mut watch, |docid| Located::Segment {
                    index,
                    docid,
                }),
                None => search.view(self.memtable(), usize::MAX, &mut watch, Located::Memtable),
            }
        });
        // In view order: every hit of a view is placed after the earlier views' on a full tie.
        let mut ranked = Vec::new();
        let mut facets: Vec<FacetCounter> = request
            .facets
            .iter()
            .copied()
            .map(FacetCounter::new)
            .collect();
        for (hits, counted) in found? {
            ranked = merge(&request.sort, ranked, hits, window)?;
            for (facet, counted) in facets.iter_mut().zip(counted) {
                facet.merge(counted);
            }
        }
        // Reading the page's documents is the last stage a deadline stops.
        exec.interrupt()?;

        let origin = self.cursor_origin();
        let mut hits = Vec::new();
        for hit in ranked.into_iter().skip(request.offset) {
            let mut document = match hit.location {
                Located::Segment { index, docid } => {
                    segment_view(&segments, index)?.document(docid)?
                }
                Located::Memtable(docid) => self.memtable().document(docid)?,
            };
            let explanation = if request.explain {
                // An explanation costs about an unpruned search of its view: each one is stoppable (SPEC §7.3).
                exec.interrupt()?;
                let explained = match hit.location {
                    Located::Segment { index, docid } => {
                        plan.explain(segment_view(&segments, index)?, docid)?
                    }
                    Located::Memtable(docid) => plan.explain(self.memtable(), docid)?,
                };
                // Every hit matched the plan in its view, so it has one.
                debug_assert!(explained.is_some(), "a hit the plan does not explain");
                explained
            } else {
                None
            };
            request.projection.apply(&mut document);
            hits.push(SearchHit {
                location: hit.location,
                score: hit.score,
                sort: hit.values,
                document,
                explanation,
                origin: origin.clone(),
            });
        }
        Ok(SearchResults {
            hits,
            facets: facets.into_iter().map(FacetCounter::finish).collect(),
        })
    }

    fn cursor_origin(&self) -> CursorOrigin {
        CursorOrigin {
            seq: self.snapshot_seq(),
            version: self.memtable().version(),
            collection: self.collection_name().to_owned(),
        }
    }
}

/// A search's request, run over one view at a time.
struct Search<'r> {
    plan: &'r SearchPlan,
    request: &'r SearchRequest,
    pruning: Pruning,
    window: usize,
}

impl Search<'_> {
    /// The best `window` hits of `view`, at `rank` in collection order ([`order`]), and its matches counted for each
    /// facet.
    fn view<V: SearchView>(
        &self,
        view: &V,
        rank: usize,
        watch: &mut ViewWatch<'_>,
        location: impl Fn(u32) -> Located,
    ) -> Result<(Vec<Ranked>, Vec<FacetCounter>), RequestError> {
        let sort = &self.request.sort;
        let counting = !self.request.facets.is_empty();
        // A facet counts every match, and only the score's own order has bounds to prune by (SPEC §7.4).
        let prune = sort.is_score() && !counting;
        let mut matcher = self.plan.matcher_with(
            view,
            if prune {
                self.pruning
            } else {
                Pruning::Exhaustive
            },
        )?;
        let after = self
            .request
            .search_after
            .as_ref()
            .map(|c| (c.values.as_slice(), ties(rank, c.location)));
        let mut docs = Ascending::default();
        let mut seen = |docid| {
            if counting {
                docs.push(docid);
            }
        };
        let hits: Vec<Ranked> = if sort.is_score() {
            let cursor = after.and_then(|(values, ties)| match values.first() {
                Some(SortValue::Score(s)) => Some((s + 0.0, ties)),
                _ => None,
            });
            let keep = |docid, score: f64| {
                seen(docid);
                // A score ranks after the cursor below its score, or tied and placed after it.
                cursor.is_none_or(|(c, ties)| match (score + 0.0).total_cmp(&c) {
                    core::cmp::Ordering::Less => true,
                    core::cmp::Ordering::Equal => ties.admit(docid),
                    core::cmp::Ordering::Greater => false,
                })
            };
            collect_where(&mut matcher, self.window, prune, keep, watch)?
                .into_iter()
                .map(|h| Ranked {
                    location: location(h.docid),
                    score: h.score,
                    values: vec![SortValue::Score(h.score)],
                })
                .collect()
        } else {
            top_sorted(sort, view, &mut matcher, self.window, after, seen, watch)?
                .into_iter()
                .map(|h| Ranked {
                    location: location(h.docid),
                    score: h.score,
                    values: h.values,
                })
                .collect()
        };
        let mut facets = Vec::with_capacity(self.request.facets.len());
        if counting {
            let docs: RoaringBitmap = docs.finish();
            for &request in &self.request.facets {
                watch.check()?;
                let mut facet = FacetCounter::new(request);
                facet.add(view, &docs)?;
                facets.push(facet);
            }
        }
        Ok((hits, facets))
    }
}

/// The segment view at `index`. A hit that names another index is an error, not a skipped hit: skipping it would
/// return a short page.
fn segment_view<'a>(
    segments: &'a [SegmentView<'a>],
    index: usize,
) -> Result<&'a SegmentView<'a>, RequestError> {
    segments.get(index).ok_or(RequestError::HitSegment {
        index,
        segments: segments.len(),
    })
}

/// Collection order of a location (SPEC §7.3): segment order with the memtable last, then docid.
fn order(location: Located) -> (usize, u32) {
    match location {
        Located::Segment { index, docid } => (index, docid),
        Located::Memtable(docid) => (usize::MAX, docid),
    }
}

/// Which matches of the view at `rank` (its [`order`]: a segment's index, the memtable `usize::MAX`) tied with a
/// cursor at `cursor` rank after it.
fn ties(rank: usize, cursor: Located) -> Ties {
    let (at, docid) = order(cursor);
    match rank.cmp(&at) {
        core::cmp::Ordering::Greater => Ties::All,
        core::cmp::Ordering::Less => Ties::None,
        core::cmp::Ordering::Equal => Ties::Above(docid),
    }
}

/// The first `window` of `a` and `b`, both sorted, by `sort` then location ([`order`]).
fn merge(
    sort: &Sort,
    a: Vec<Ranked>,
    b: Vec<Ranked>,
    window: usize,
) -> Result<Vec<Ranked>, RequestError> {
    let mut out = Vec::with_capacity(a.len().saturating_add(b.len()).min(window));
    let (mut a, mut b) = (a.into_iter().peekable(), b.into_iter().peekable());
    while out.len() < window {
        let take_a = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => {
                sort.compare(&x.values, &y.values)?
                    .then_with(|| order(x.location).cmp(&order(y.location)))
                    != core::cmp::Ordering::Greater
            }
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        out.extend(if take_a { a.next() } else { b.next() });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::segment_view;
    use crate::search::RequestError;
    use crate::view::SegmentView;

    #[test]
    fn a_hit_whose_segment_index_has_no_view_is_an_error() {
        let segments: &[SegmentView<'_>] = &[];
        let err = segment_view(segments, 4).unwrap_err();
        assert!(matches!(
            err,
            RequestError::HitSegment {
                index: 4,
                segments: 0
            }
        ));
    }
}
