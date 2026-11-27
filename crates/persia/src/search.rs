//! Search through a [`Collection`] (SPEC §7; ROADMAP 10.1.3; ADR-0062, ADR-0063).
//!
//! The facade's [`SearchRequest`] is a builder of the request SPEC §7.1 describes, distinct from the engine's types:
//! [`Collection::search`] lowers it, and [`Collection::search_json`] parses the request JSON, into the engine's one
//! typed request (`QueryRequest`, ADR-0062), which runs on the collection's latest published snapshot. A cursor
//! crosses the API as an opaque token (SPEC §7.4): [`Hit::cursor`], [`SearchResponse::next_cursor`] and
//! [`SearchRequest::after`].

use std::sync::Arc;

use persia_engine::db::{Relaxation as EngineRelaxation, RelaxedHit, SearchHit};
use persia_engine::document::Document;
use persia_engine::query::{DefaultFields, Filter, QueryLimits};
use persia_engine::relax::{Correction, FallbackOrder, RelaxLevel, RelaxOptions};
use persia_engine::schema::CollectionSchema;
use persia_engine::search::request::{QueryRequest, QueryResponse, QueryResults, RunOptions};
use persia_engine::search::{Explanation, FacetCounts, FacetRequest, Sort};

use crate::collection::Collection;
use crate::{Error, Result};

// The response JSON (ROADMAP 10.1.25): `SearchResponse` is `Serialize`, and `to_json` writes it.
mod json;
// A hit's typed sort values (ROADMAP 10.1.30).
mod sort_value;

pub use sort_value::SortValue;

/// A search request (SPEC §7.1): the user text `q`, and what narrows, orders and pages its hits. Build it with
/// [`SearchRequest::new`] and the setters; [`Collection::search`] runs it. Field ids come from the collection's
/// schema ([`Collection::schema`]), so a request is meant for one collection's schema.
///
/// ```
/// use persia::{FilterValue, Filter, SearchRequest};
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// # let db = persia::Db::open(dir.path().join("app.persia"))?;
/// let products = db.create_collection_json(
///     r#"{"name": "products", "fields": {"title": {"type": "text"}, "brand": {"type": "keyword"}}}"#,
/// )?;
/// products.put_json(r#"{"id": "p1", "title": "red running shoes", "brand": "acme"}"#)?;
/// products.put_json(r#"{"id": "p2", "title": "red rain coat", "brand": "zeta"}"#)?;
/// let brand = products.schema()?.field("brand").map(|f| f.id()).ok_or("no brand")?;
/// let request = SearchRequest::new("red")
///     .filter(Filter::Term { field: brand, value: FilterValue::Keyword("acme".into()) })
///     .limit(10);
/// let response = products.search(&request)?;
/// let ids: Vec<&str> = response.hits.iter().map(|h| h.document.id()).collect();
/// assert_eq!(ids, ["p1"]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct SearchRequest {
    q: String,
    fields: Option<Vec<String>>,
    filter: Option<Filter>,
    soft_filter: Option<Filter>,
    sort: Option<Sort>,
    limit: Option<usize>,
    offset: Option<usize>,
    after: Option<String>,
    facets: Vec<FacetRequest>,
    explain: bool,
    relax: Option<RelaxOptions>,
    proximity: bool,
}

impl SearchRequest {
    /// A request for the user text `q` (SPEC §7.1.1), with every other part at its default: every `text` field
    /// searched, no filter, sorted by score, the first page of the default size, relaxed as the collection says.
    ///
    /// ```
    /// let request = persia::SearchRequest::new("running shoes");
    /// # let _ = request;
    /// ```
    #[must_use]
    pub fn new(q: impl Into<String>) -> Self {
        Self {
            q: q.into(),
            fields: None,
            filter: None,
            soft_filter: None,
            sort: None,
            limit: None,
            offset: None,
            after: None,
            facets: Vec::new(),
            explain: false,
            relax: None,
            proximity: false,
        }
    }

    /// The `text` fields `q` is searched in, by name, each with an optional `^weight` (`"title^2"`, SPEC §7.1):
    /// checked against the schema when the request runs.
    ///
    /// ```
    /// let request = persia::SearchRequest::new("shoes").fields(["title^2", "body"]);
    /// # let _ = request;
    /// ```
    #[must_use]
    pub fn fields<S: Into<String>>(mut self, fields: impl IntoIterator<Item = S>) -> Self {
        self.fields = Some(fields.into_iter().map(Into::into).collect());
        self
    }

    /// A hard filter (SPEC §7.1.2): no hit of any relaxation level breaks it.
    ///
    /// ```
    /// use persia::{Filter, SearchRequest};
    ///
    /// # let schema = persia::SchemaBuilder::new("c").field("in_stock", persia::FieldKind::Bool { stored: false }).build()?;
    /// # let in_stock = schema.field("in_stock").map(|f| f.id()).ok_or("no field")?;
    /// let request = SearchRequest::new("shoes").filter(Filter::Exists { field: in_stock });
    /// # let _ = request;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn filter(mut self, filter: Filter) -> Self {
        self.filter = Some(filter);
        self
    }

    /// A soft filter (SPEC §7.1.2, §7.6): kept like `filter` at every level but the last resort, which a collection
    /// with `relax.filters = "last_resort"` drops it for.
    ///
    /// ```
    /// use persia::{Filter, SearchRequest};
    ///
    /// # let schema = persia::SchemaBuilder::new("c").field("on_sale", persia::FieldKind::Bool { stored: false }).build()?;
    /// # let on_sale = schema.field("on_sale").map(|f| f.id()).ok_or("no field")?;
    /// let request = SearchRequest::new("shoes").soft_filter(Filter::Exists { field: on_sale });
    /// # let _ = request;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn soft_filter(mut self, filter: Filter) -> Self {
        self.soft_filter = Some(filter);
        self
    }

    /// The order of the hits (SPEC §7.4); by score, best first, by default.
    ///
    /// ```
    /// let request = persia::SearchRequest::new("shoes").sort(persia::Sort::score());
    /// # let _ = request;
    /// ```
    #[must_use]
    pub fn sort(mut self, sort: Sort) -> Self {
        self.sort = Some(sort);
        self
    }

    /// How many hits a page holds (SPEC §7.4).
    ///
    /// ```
    /// let request = persia::SearchRequest::new("shoes").limit(20);
    /// # let _ = request;
    /// ```
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// How many hits to skip before the page (SPEC §7.4); prefer [`Self::after`] for deep pages.
    ///
    /// ```
    /// let request = persia::SearchRequest::new("shoes").offset(20).limit(20);
    /// # let _ = request;
    /// ```
    #[must_use]
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    /// The page after the hit whose cursor token this is ([`Hit::cursor`], [`SearchResponse::next_cursor`]; SPEC
    /// §7.4). A token that was cut, changed, written by another version, or taken from a search with another sort or
    /// another `q`, fields, filters or limits (ROADMAP 10.1.24) is refused when the request runs ([`ErrorCode::InvalidArgument`](crate::ErrorCode::InvalidArgument)).
    ///
    /// ```
    /// let request = persia::SearchRequest::new("shoes").after("token-from-a-previous-page");
    /// # let _ = request;
    /// ```
    #[must_use]
    pub fn after(mut self, cursor: impl Into<String>) -> Self {
        self.after = Some(cursor.into());
        self
    }

    /// Facets to count over every match (SPEC §7.5).
    ///
    /// ```
    /// use persia::{FacetRequest, FieldKind, KeywordOptions, SchemaBuilder, SearchRequest};
    ///
    /// let facet = KeywordOptions::default().facet(true);
    /// let schema = SchemaBuilder::new("c").field("brand", FieldKind::Keyword(facet)).build()?;
    /// let brand = schema.field("brand").map(|f| f.id()).ok_or("no field")?;
    /// let request = SearchRequest::new("shoes").facets([FacetRequest::new(brand, 10, &schema)?]);
    /// # let _ = request;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn facets(mut self, facets: impl IntoIterator<Item = FacetRequest>) -> Self {
        self.facets = facets.into_iter().collect();
        self
    }

    /// Whether each hit carries how its score adds up (SPEC §7.7).
    ///
    /// ```
    /// let request = persia::SearchRequest::new("shoes").explain(true);
    /// # let _ = request;
    /// ```
    #[must_use]
    pub fn explain(mut self, explain: bool) -> Self {
        self.explain = explain;
        self
    }

    /// How this request relaxes, over the collection's settings (SPEC §7.6): `RelaxOptions { enabled: Some(false),
    /// ..}` runs L0 alone.
    ///
    /// ```
    /// use persia::{RelaxOptions, SearchRequest};
    ///
    /// let exact = SearchRequest::new("shoes").relax(RelaxOptions { enabled: Some(false), ..RelaxOptions::default() });
    /// # let _ = exact;
    /// ```
    #[must_use]
    pub fn relax(mut self, relax: RelaxOptions) -> Self {
        self.relax = Some(relax);
        self
    }

    /// Whether the score adds the proximity bonus (SPEC §7.2). Only a search that does not relax takes it: with
    /// relaxation on (the default), `true` is [`ErrorCode::InvalidArgument`](crate::ErrorCode::InvalidArgument);
    /// set [`Self::relax`] to `enabled: Some(false)`.
    ///
    /// ```
    /// use persia::{RelaxOptions, SearchRequest};
    ///
    /// let request = SearchRequest::new("running shoes")
    ///     .relax(RelaxOptions { enabled: Some(false), ..RelaxOptions::default() })
    ///     .proximity(true);
    /// # let _ = request;
    /// ```
    #[must_use]
    pub fn proximity(mut self, proximity: bool) -> Self {
        self.proximity = proximity;
        self
    }

    /// The engine's request for `schema`.
    fn lower(&self, schema: &CollectionSchema) -> Result<QueryRequest> {
        let mut request = QueryRequest::new(self.q.clone())
            .with_explain(self.explain)
            .with_proximity(self.proximity);
        if let Some(fields) = &self.fields {
            let fields = DefaultFields::parse(schema, fields).map_err(Error::wrap)?;
            request = request.with_fields(fields);
        }
        if let Some(filter) = &self.filter {
            request = request.with_filter(filter.clone());
        }
        if let Some(filter) = &self.soft_filter {
            request = request.with_soft_filter(filter.clone());
        }
        if let Some(sort) = &self.sort {
            request = request.with_sort(sort.clone());
        }
        if let Some(limit) = self.limit {
            request = request.with_limit(limit);
        }
        if let Some(offset) = self.offset {
            request = request.with_offset(offset);
        }
        if let Some(token) = &self.after {
            let cursor = persia_engine::db::Cursor::from_token(token).map_err(Error::wrap)?;
            request = request.with_search_after(cursor);
        }
        if !self.facets.is_empty() {
            request = request.with_facets(self.facets.iter().copied());
        }
        if let Some(relax) = &self.relax {
            request = request.with_relax(relax.clone());
        }
        Ok(request)
    }
}

/// The page a search returns (SPEC §7.5).
#[derive(Clone)]
#[non_exhaustive]
pub struct SearchResponse {
    /// The page, best first (or in the request's sort).
    pub hits: Vec<Hit>,
    /// One per requested facet, in request order (SPEC §7.5).
    pub facets: Vec<FacetCounts>,
    /// What the relaxation cascade did (SPEC §7.6); level `exact` with nothing applied for a search that ran L0 alone.
    pub relaxation: Relaxation,
    /// Whether part of `q` was left out to stay within the query limits (SPEC §7.1.1).
    pub query_truncated: bool,
    /// The schema of the collection that answered, which names the response's fields in its JSON ([`Self::to_json`]).
    schema: Arc<CollectionSchema>,
}

/// Every public field; the schema it keeps to write its JSON is left out.
impl std::fmt::Debug for SearchResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchResponse")
            .field("hits", &self.hits)
            .field("facets", &self.facets)
            .field("relaxation", &self.relaxation)
            .field("query_truncated", &self.query_truncated)
            .finish_non_exhaustive()
    }
}

impl SearchResponse {
    /// The cursor token of the page after this one: the last hit's ([`Hit::cursor`]), `None` on an empty page.
    ///
    /// ```
    /// use persia::SearchRequest;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// for i in 0..3 {
    ///     notes.put_json(&format!(r#"{{"id": "n{i}", "body": "hello"}}"#))?;
    /// }
    /// let first = notes.search(&SearchRequest::new("hello").limit(2))?;
    /// let next = first.next_cursor().ok_or("no cursor")?;
    /// let second = notes.search(&SearchRequest::new("hello").limit(2).after(next))?;
    /// assert_eq!(first.hits.len() + second.hits.len(), 3);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        self.hits.last().map(|hit| hit.cursor.as_str())
    }
}

/// One hit of a [`SearchResponse`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Hit {
    /// The document: its id, blob reference and stored fields.
    pub document: Document,
    /// Its score (SPEC §7.2).
    pub score: f64,
    /// The relaxation level whose step matched it first (SPEC §7.6); `exact` for a search that ran L0 alone. The
    /// response JSON states it (`"level"`) for a relaxed search's hit only, beside [`Self::step`] and
    /// [`Self::rank_score`].
    pub level: RelaxLevel,
    /// How its score adds up, when the request asked ([`SearchRequest::explain`]).
    pub explanation: Option<Explanation>,
    /// The cursor token of the page after this hit (SPEC §7.4), for [`SearchRequest::after`].
    pub cursor: String,
    /// Its values for the sort keys, typed by the keys' fields.
    sort: Vec<SortValue>,
    /// Its step and rank score, for a hit of a relaxed search (SPEC §7.6).
    relaxed: Option<RelaxedStep>,
}

impl Hit {
    /// Its values for the keys of the sort that ranked it (SPEC §7.4), one per key in the sort's order, typed by the
    /// key ([`SortValue`]); a request without a sort is sorted by score, so each hit has one value, its
    /// [`SortValue::Score`]. [`SearchResponse::to_json`] writes them as the hit's `"sort"` array, always present, with
    /// one element per value in this order: the score and an `f64` as numbers, an `i64` as an integer, a `datetime` as
    /// SPEC §6.4.1's output text, a `bool`, a keyword as a string (or `{"base64url": ...}` for bytes that are not
    /// UTF-8), and [`SortValue::Missing`] as `null`.
    ///
    /// ```
    /// use persia::{SearchRequest, SortValue};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// notes.put_json(r#"{"id": "n1", "body": "hello world"}"#)?;
    /// let response = notes.search(&SearchRequest::new("hello"))?;
    /// let hit = &response.hits[0];
    /// assert_eq!(hit.sort_values(), [SortValue::Score(hit.score)]);
    /// let json: serde_json::Value = serde_json::from_str(&response.to_json()?)?;
    /// assert_eq!(json["hits"][0]["sort"], serde_json::json!([hit.score]));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn sort_values(&self) -> &[SortValue] {
        &self.sort
    }

    /// The index of the relaxation cascade's step that matched it first (SPEC §7.6), 0 being L0's query, for a hit of
    /// a relaxed search; `None` for a hit of a plain search (one that ran L0 alone: `relax: false`, or a collection
    /// whose cascade is disabled). [`SearchResponse::to_json`] writes it as the hit's `"step"`, an integer, only when
    /// it is `Some`, beside `"level"` ([`Self::level`]) and `"rank_score"` ([`Self::rank_score`]); a plain search's
    /// hits have none of the three keys (never `null`).
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// notes.put_json(r#"{"id": "n1", "body": "hello world"}"#)?;
    /// let relaxed = notes.search_json(r#"{"q": "hello zebra"}"#)?; // L0 finds nothing: a later step does
    /// let json: serde_json::Value = serde_json::from_str(&relaxed.to_json()?)?;
    /// let step = relaxed.hits[0].step().ok_or("a relaxed hit has a step")?;
    /// assert!(step > 0);
    /// assert_eq!(json["hits"][0]["step"], step);
    /// let plain = notes.search_json(r#"{"q": "hello", "relax": false}"#)?;
    /// let json: serde_json::Value = serde_json::from_str(&plain.to_json()?)?;
    /// assert_eq!(plain.hits[0].step(), None);
    /// assert!(json["hits"][0].get("step").is_none());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn step(&self) -> Option<usize> {
        self.relaxed.map(|r| r.step)
    }

    /// Its rank score, for a hit of a relaxed search (SPEC §7.6): its step's rank combined with its score, the order
    /// the cascade ranks hits of different steps in (never below a hit's of a later step); `None` for a hit of a plain
    /// search. [`SearchResponse::to_json`] writes it as the hit's `"rank_score"`, a finite number, only when it is
    /// `Some`, as [`Self::step`] says.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// notes.put_json(r#"{"id": "n1", "body": "hello world"}"#)?;
    /// let response = notes.search_json(r#"{"q": "hello"}"#)?; // relaxed by default: L0 has the hit
    /// let json: serde_json::Value = serde_json::from_str(&response.to_json()?)?;
    /// let rank_score = response.hits[0].rank_score().ok_or("a relaxed hit has a rank score")?;
    /// assert_eq!(json["hits"][0]["rank_score"].as_f64(), Some(rank_score));
    /// let plain = notes.search_json(r#"{"q": "hello", "relax": false}"#)?;
    /// assert_eq!(plain.hits[0].rank_score(), None);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn rank_score(&self) -> Option<f64> {
        self.relaxed.map(|r| r.rank_score)
    }
}

/// The step that matched a relaxed search's hit, and its rank score (SPEC §7.6): what the response JSON states beside
/// the hit's level.
#[derive(Debug, Clone, Copy)]
struct RelaxedStep {
    step: usize,
    rank_score: f64,
}

/// What the relaxation cascade did for a response (SPEC §7.6).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Relaxation {
    /// The level of the last step built (`"level"`).
    pub level: RelaxLevel,
    /// What each step after L0 applied, in order (`"applied"`): `"drop_terms:75%"`, `"fuzzy"`.
    pub applied: Vec<String>,
    /// The user text, as read (`"original"`).
    pub original: Option<String>,
    /// The text of the query of the last step that found a hit (`"effective"`).
    pub effective: Option<String>,
    /// Whether the cascade's time budget cut it short (`"truncated"`).
    pub truncated: bool,
    /// The words the fuzzy level replaced (`"corrected"`), in query order.
    corrected: Vec<Correction>,
    /// The order L5 found the hits in (`"fallback"`), when it did.
    fallback: Option<FallbackOrder>,
    /// Whether the last resort dropped the filters (`"filters_dropped"`).
    filters_dropped: bool,
}

impl Relaxation {
    /// The words the fuzzy level replaced, in query order (`"corrected"`, SPEC §7.6): each required word no document
    /// holds and the term searched instead ("did you mean"). Empty unless L4 found the hits.
    ///
    /// ```
    /// use persia::SearchRequest;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let shoes = db.create_collection_json(r#"{"name": "shoes", "fields": {"title": {"type": "text"}}}"#)?;
    /// shoes.put_json(r#"{"id": "a", "title": "red sandals"}"#)?;
    /// let response = shoes.search_json(r#"{"q": "red snadals", "relax": {"levels": ["fuzzy"]}}"#)?;
    /// let corrected = response.relaxation.corrected();
    /// assert_eq!((corrected[0].original.as_str(), corrected[0].corrected.as_str()), ("snadals", "sandals"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn corrected(&self) -> &[Correction] {
        &self.corrected
    }

    /// The order L5 `fallback` ranked the hits in (`"fallback"`, SPEC §7.6), when L5 found them.
    ///
    /// ```
    /// use persia::FallbackOrder;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// notes.put_json(r#"{"id": "n1", "body": "hello"}"#)?;
    /// let response = notes.search_json(r#"{"q": "purple"}"#)?;
    /// assert_eq!(response.relaxation.fallback(), Some(FallbackOrder::Recency)); // no static rank
    /// assert_eq!(notes.search_json(r#"{"q": "hello"}"#)?.relaxation.fallback(), None);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn fallback(&self) -> Option<FallbackOrder> {
        self.fallback
    }

    /// Whether the last resort dropped the filters to find the hits (`"filters_dropped"`, SPEC §7.6): only in a
    /// collection whose relaxation allows it (`filters: "last_resort"`).
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// notes.put_json(r#"{"id": "n1", "body": "hello"}"#)?;
    /// assert!(!notes.search_json(r#"{"q": "hello"}"#)?.relaxation.filters_dropped());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn filters_dropped(&self) -> bool {
        self.filters_dropped
    }

    /// The facade's value of what the engine's cascade did, or of the `exact` a plain search states.
    fn of(r: &EngineRelaxation) -> Self {
        Self {
            level: r.level,
            applied: r.applied.clone(),
            original: r.original.clone(),
            effective: r.effective.clone(),
            truncated: r.truncated,
            corrected: r.corrected.clone(),
            fallback: r.fallback,
            filters_dropped: r.filters_dropped,
        }
    }
}

/// The facade's hit of `hit`, a hit of a search of the collection of `schema`: its sort values typed by their keys.
fn plain_hit(mut hit: SearchHit, schema: &CollectionSchema) -> Result<Hit> {
    // The cursor copies the engine's values: take it before they move into the typed ones.
    let cursor = hit.cursor().to_token();
    let sort = SortValue::take_from(&mut hit, schema)?;
    Ok(Hit {
        cursor,
        document: hit.document,
        score: hit.score,
        level: RelaxLevel::Exact,
        explanation: hit.explanation,
        sort,
        relaxed: None,
    })
}

fn relaxed_hit(hit: RelaxedHit, schema: &CollectionSchema) -> Result<Hit> {
    let relaxed = Some(RelaxedStep {
        step: hit.step,
        rank_score: hit.rank_score,
    });
    Ok(Hit {
        level: hit.level,
        relaxed,
        ..plain_hit(hit.hit, schema)?
    })
}

/// The facade's response for `response`, a search of the collection of `schema`.
fn response(response: QueryResponse, schema: Arc<CollectionSchema>) -> Result<SearchResponse> {
    let query_truncated = response.query_truncated;
    // A plain search states level `exact` with L0's text (SPEC §7.6), as the engine's response JSON does.
    let relaxation = Relaxation::of(response.relaxation());
    match response.results {
        QueryResults::Plain(results) => Ok(SearchResponse {
            hits: results
                .hits
                .into_iter()
                .map(|hit| plain_hit(hit, &schema))
                .collect::<Result<_>>()?,
            facets: results.facets,
            relaxation,
            query_truncated,
            schema,
        }),
        QueryResults::Relaxed(results) => Ok(SearchResponse {
            hits: results
                .hits
                .into_iter()
                .map(|hit| relaxed_hit(hit, &schema))
                .collect::<Result<_>>()?,
            facets: results.facets,
            relaxation,
            query_truncated,
            schema,
        }),
        // `#[non_exhaustive]`: a kind of results this build does not know.
        _ => Err(Error::new(
            crate::ErrorCode::Unsupported,
            "the engine returned results of a kind this build of persia does not read",
        )),
    }
}

impl Collection {
    /// Runs `request` on the collection as the latest published state of the database shows it: every write that
    /// returned, committed or not, on a read-write `Db`; the state of the open or the last
    /// [`refresh`](crate::Db::refresh) on a read-only one (SPEC §7, §8.4).
    ///
    /// ```
    /// use persia::SearchRequest;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// notes.put_json(r#"{"id": "n1", "body": "the quick brown fox"}"#)?;
    /// let response = notes.search(&SearchRequest::new("quick fox"))?;
    /// assert_eq!(response.hits[0].document.id(), "n1");
    /// // Never zero results by default: a word that matches nothing is relaxed away.
    /// let relaxed = notes.search(&SearchRequest::new("quick zebra"))?;
    /// assert!(!relaxed.hits.is_empty());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`](crate::ErrorCode::InvalidArgument) for a part that does not fit the schema, a
    /// cursor token that is damaged or from another sort or search, or `proximity` on a search that relaxes;
    /// [`ErrorCode::Limit`](crate::ErrorCode::Limit) past a limit of SPEC §7.1.3 or §17;
    /// [`ErrorCode::NotFound`](crate::ErrorCode::NotFound) if the collection was dropped;
    /// [`ErrorCode::Corrupt`](crate::ErrorCode::Corrupt) if a segment is damaged;
    /// [`ErrorCode::Closed`](crate::ErrorCode::Closed).
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResponse> {
        self.search_with(request, &RunOptions::new())
    }

    /// [`search`](Self::search) that stops with [`ErrorCode::Cancelled`](crate::ErrorCode::Cancelled) once `token`
    /// is cancelled (the async API's search, ADR-0064 item 13).
    #[cfg(feature = "async")]
    pub(crate) fn search_cancellable(
        &self,
        request: &SearchRequest,
        token: Option<persia_engine::db::CancelToken>,
    ) -> Result<SearchResponse> {
        let run = match token {
            Some(token) => RunOptions::new().cancel(token),
            None => RunOptions::new(),
        };
        self.search_with(request, &run)
    }

    /// [`search`](Self::search) run as `run` says.
    fn search_with(&self, request: &SearchRequest, run: &RunOptions) -> Result<SearchResponse> {
        self.read(|c| {
            let lowered = request.lower(c.schema())?;
            let ran = lowered.run(c, run).map_err(Error::wrap)?;
            response(ran, c.shared_schema())
        })
    }

    /// [`search`](Self::search) of the request JSON `json` (SPEC §7.1, ADR-0062): `q`, `fields`, `filter`,
    /// `soft_filter`, `sort`, `limit`, `offset`, `search_after` (a cursor token), `facets`, `relax`, `explain`,
    /// `proximity` and `projection`, read strictly.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// notes.put_json(r#"{"id": "n1", "body": "hello world"}"#)?;
    /// let response = notes.search_json(r#"{"q": "hello", "limit": 5, "relax": false}"#)?;
    /// assert_eq!(response.hits.len(), 1);
    /// let err = notes.search_json(r#"{"query": "hello"}"#).unwrap_err();
    /// assert_eq!(err.code(), persia::ErrorCode::InvalidArgument);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`](crate::ErrorCode::InvalidArgument) for text that is not a request (malformed,
    /// an unknown or repeated key, a value of another shape); then as [`search`](Self::search).
    pub fn search_json(&self, json: &str) -> Result<SearchResponse> {
        self.read(|c| {
            let limits = QueryLimits::default();
            let request =
                QueryRequest::from_json(json, c.schema(), &limits).map_err(Error::wrap)?;
            let ran = request.run(c, &RunOptions::new()).map_err(Error::wrap)?;
            response(ran, c.shared_schema())
        })
    }
}
