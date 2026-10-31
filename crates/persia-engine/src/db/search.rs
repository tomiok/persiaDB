//! A search request over a collection (SPEC §7.4, §7.5, §7.5.1, ROADMAP 7.5): a sort, a page (`offset`/`limit` or
//! a cursor), facets and a projection, run over [`CollectionViews`].
//!
//! Each view keeps its best `offset + limit` hits by the sort, after the cursor when there is one; they merge into
//! the collection's by the sort, then location (segment order, the memtable last, docid). With facets every match is
//! visited, and each view's matching bitmap is counted. The page's hits then get their documents, projected.

use roaring::RoaringBitmap;

use super::snapshot::Located;
use super::views::CollectionViews;
use crate::bitmap::Ascending;
use crate::document::Document;
use crate::search::{
    FacetCounter, FacetCounts, FacetRequest, MAX_WINDOW, Matcher, Projection, Pruning,
    RequestError, SearchPlan, Sort, SortValue, Ties, collect_where, top_sorted,
};
use crate::segment::ReadError;
use crate::view::SearchView;

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
        }
    }
}

/// Where a page starts (SPEC §7.4): a hit's sort values and location, from [`SearchHit::cursor`]. Exact within the
/// snapshot and memtable view that returned the hit.
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    /// One value per sort key.
    pub values: Vec<SortValue>,
    /// The hit's document.
    pub location: Located,
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
}

impl SearchHit {
    /// The cursor of the page after this hit.
    pub fn cursor(&self) -> Cursor {
        Cursor {
            values: self.sort.clone(),
            location: self.location,
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
    /// cursor that does not fit the sort; [`RequestError::Read`] as [`SearchPlan::matcher`], or for a document that
    /// cannot be read.
    pub fn search(
        &self,
        plan: &SearchPlan,
        request: &SearchRequest,
        pruning: Pruning,
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
        }
        let mut search = Search {
            plan,
            request,
            pruning,
            window,
            hits: Vec::new(),
            facets: request
                .facets
                .iter()
                .copied()
                .map(FacetCounter::new)
                .collect(),
        };
        let segments: Vec<_> = self.segments().collect();
        for (index, view) in segments.iter().enumerate() {
            search.view(view, index, |docid| Located::Segment { index, docid })?;
        }
        search.view(self.memtable(), usize::MAX, Located::Memtable)?;

        let mut hits = Vec::new();
        for hit in search.hits.into_iter().skip(request.offset) {
            let mut document = match hit.location {
                Located::Segment { index, docid } => match segments.get(index) {
                    Some(view) => view.document(docid)?,
                    None => continue,
                },
                Located::Memtable(docid) => self.memtable().document(docid)?,
            };
            request.projection.apply(&mut document);
            hits.push(SearchHit {
                location: hit.location,
                score: hit.score,
                sort: hit.values,
                document,
            });
        }
        Ok(SearchResults {
            hits,
            facets: search
                .facets
                .into_iter()
                .map(FacetCounter::finish)
                .collect(),
        })
    }
}

/// A search in progress: the best `window` hits of the views searched so far, and the facet counts.
struct Search<'r> {
    plan: &'r SearchPlan,
    request: &'r SearchRequest,
    pruning: Pruning,
    window: usize,
    hits: Vec<Ranked>,
    facets: Vec<FacetCounter>,
}

impl Search<'_> {
    /// Merges the hits of `view`, at `rank` in collection order ([`order`]), and counts its matches for the facets.
    fn view<V: SearchView>(
        &mut self,
        view: &V,
        rank: usize,
        location: impl Fn(u32) -> Located,
    ) -> Result<(), ReadError> {
        let sort = &self.request.sort;
        let counting = !self.facets.is_empty();
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
            if prune
                && self.hits.len() >= self.window
                && let Some(worst) = self.hits.last()
            {
                // Every hit kept wins a tie against this later view's (SPEC §7.3).
                matcher.set_min_competitive(worst.score);
            }
            let cursor = after.and_then(|(values, ties)| match values.first() {
                Some(SortValue::Score(s)) => Some((s + 0.0, ties)),
                _ => None,
            });
            collect_where(&mut matcher, self.window, prune, |docid, score| {
                seen(docid);
                // A score ranks after the cursor below its score, or tied and placed after it.
                cursor.is_none_or(|(c, ties)| match (score + 0.0).total_cmp(&c) {
                    core::cmp::Ordering::Less => true,
                    core::cmp::Ordering::Equal => ties.admit(docid),
                    core::cmp::Ordering::Greater => false,
                })
            })?
            .into_iter()
            .map(|h| Ranked {
                location: location(h.docid),
                score: h.score,
                values: vec![SortValue::Score(h.score)],
            })
            .collect()
        } else {
            top_sorted(sort, view, &mut matcher, self.window, after, seen)?
                .into_iter()
                .map(|h| Ranked {
                    location: location(h.docid),
                    score: h.score,
                    values: h.values,
                })
                .collect()
        };
        if counting {
            let docs: RoaringBitmap = docs.finish();
            for facet in &mut self.facets {
                facet.add(view, &docs)?;
            }
        }
        self.hits.extend(hits);
        self.hits.sort_by(|a, b| {
            sort.compare(&a.values, &b.values)
                .then_with(|| order(a.location).cmp(&order(b.location)))
        });
        self.hits.truncate(self.window);
        Ok(())
    }
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
