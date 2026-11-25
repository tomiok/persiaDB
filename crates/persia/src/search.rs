//! Search through a [`Collection`] (SPEC §7; ROADMAP 10.1.3; ADR-0062, ADR-0063).
//!
//! The facade's [`SearchRequest`] is a builder of the request SPEC §7.1 describes, distinct from the engine's types:
//! [`Collection::search`] lowers it, and [`Collection::search_json`] parses the request JSON, into the engine's one
//! typed request (`QueryRequest`, ADR-0062), which runs on the collection's latest published snapshot. A cursor
//! crosses the API as an opaque token (SPEC §7.4): [`Hit::cursor`], [`SearchResponse::next_cursor`] and
//! [`SearchRequest::after`].

use persia_engine::db::{RelaxedHit, SearchHit};
use persia_engine::document::Document;
use persia_engine::query::{DefaultFields, Filter, QueryLimits};
use persia_engine::relax::{RelaxLevel, RelaxOptions};
use persia_engine::schema::CollectionSchema;
use persia_engine::search::request::{QueryRequest, QueryResponse, QueryResults, RunOptions};
use persia_engine::search::{Explanation, FacetCounts, FacetRequest, Sort};

use crate::collection::Collection;
use crate::{Error, Result};

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
    /// §7.4). A token that was cut, changed, written by another version, or taken from a search with another sort is
    /// refused when the request runs ([`ErrorCode::InvalidArgument`](crate::ErrorCode::InvalidArgument)).
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
    /// let facet = KeywordOptions { facet: true, ..KeywordOptions::default() };
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
#[derive(Debug, Clone)]
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
    /// The relaxation level whose step matched it first (SPEC §7.6); `exact` for a search that ran L0 alone.
    pub level: RelaxLevel,
    /// How its score adds up, when the request asked ([`SearchRequest::explain`]).
    pub explanation: Option<Explanation>,
    /// The cursor token of the page after this hit (SPEC §7.4), for [`SearchRequest::after`].
    pub cursor: String,
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
}

impl Relaxation {
    fn exact(original: &str) -> Self {
        Self {
            level: RelaxLevel::Exact,
            applied: Vec::new(),
            original: Some(original.to_owned()),
            effective: None,
            truncated: false,
        }
    }
}

fn plain_hit(hit: SearchHit) -> Hit {
    Hit {
        cursor: hit.cursor().to_token(),
        document: hit.document,
        score: hit.score,
        level: RelaxLevel::Exact,
        explanation: hit.explanation,
    }
}

fn relaxed_hit(hit: RelaxedHit) -> Hit {
    let cursor = hit.cursor().to_token();
    Hit {
        cursor,
        level: hit.level,
        score: hit.hit.score,
        document: hit.hit.document,
        explanation: hit.hit.explanation,
    }
}

fn response(q: &str, response: QueryResponse) -> Result<SearchResponse> {
    let query_truncated = response.query_truncated;
    match response.results {
        QueryResults::Plain(results) => Ok(SearchResponse {
            hits: results.hits.into_iter().map(plain_hit).collect(),
            facets: results.facets,
            relaxation: Relaxation::exact(q),
            query_truncated,
        }),
        QueryResults::Relaxed(results) => {
            let r = results.relaxation;
            Ok(SearchResponse {
                hits: results.hits.into_iter().map(relaxed_hit).collect(),
                facets: results.facets,
                relaxation: Relaxation {
                    level: r.level,
                    applied: r.applied,
                    original: r.original,
                    effective: r.effective,
                    truncated: r.truncated,
                },
                query_truncated,
            })
        }
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
            response(&request.q, ran)
        })
    }

    /// [`search`](Self::search) of the request JSON `json` (SPEC §7.1, ADR-0062): `q`, `fields`, `filter`,
    /// `soft_filter`, `sort`, `limit`, `offset`, `search_after` (a cursor token), `facets`, `relax`, `explain` and
    /// `proximity`, read strictly.
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
            let q = request.q().to_owned();
            let ran = request.run(c, &RunOptions::new()).map_err(Error::wrap)?;
            response(&q, ran)
        })
    }
}
