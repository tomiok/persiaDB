//! A whole search request (SPEC §7.1, ADR-0062): the user text, the fields it searches, the filters, the page, the
//! relaxation settings and the proximity option, as one typed value, and the one function that runs it on a
//! collection's snapshot.
//!
//! [`QueryRequest`] is what a caller builds in code and what the request JSON parses to ([`QueryRequest::from_json`]),
//! so both run the same way: `q` compiled with the collection's analyzers, the filters joined to it (`filter` hard,
//! `soft_filter` soft), then the relaxed search with the collection's settings as the request overrides them, or the
//! plain search when those settings run L0 alone.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use super::cursor::CursorTokenError;
use super::fingerprint::fingerprint;
use crate::db::{
    CancelToken, Clock, CollectionSnapshot, Cursor, Execution, Relaxation, RelaxedResults,
    RelaxedSearchError, SearchRequest, SearchResults, Threads,
};
use crate::query::{DefaultFields, Filter, QueryError, QueryLimits};
use crate::relax::{RelaxError, RelaxOptions, RelaxPlan};
use crate::schema::CollectionSchema;
use crate::search::{
    FacetRequest, Interrupt, Projection, RequestError, SearchError, SearchPlan, Sort,
};

/// A search request (SPEC §7.1): what the request JSON says, typed. Built in code with [`Self::new`] and the `with_`
/// setters, or parsed ([`Self::from_json`]); run with [`Self::run`].
///
/// The default is an empty `q` (which matches every document, SPEC §7.1.1) over every `text` field, no filter, the
/// first 20 hits by score, no facet, every stored field, the collection's relaxation settings, no explanation and no
/// proximity bonus.
///
/// A request is built for one schema: its filters, sort, facets, default fields and projection name fields by id
/// (SPEC §6.2.1). Run it on a snapshot of that schema.
///
/// ```
/// use persia_engine::db::{Database, DatabaseOptions};
/// use persia_engine::document::{Document, Value};
/// use persia_engine::query::{Filter, FilterValue, QueryLimits};
/// use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
/// use persia_engine::search::request::{QueryRequest, QueryResults, RunOptions};
/// use persia_engine::wal::Durability;
/// use persia_storage::container::sim::SimFile;
///
/// let schema = SchemaBuilder::new("shoes")
///     .field("title", FieldKind::Text(TextOptions::default()))
///     .field("brand", FieldKind::Keyword(KeywordOptions::default()))
///     .build()?;
/// let (title, brand) = (schema.field("title").ok_or("no field")?.id(), schema.field("brand").ok_or("no field")?.id());
/// let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default())?;
/// db.create_collection(schema)?;
/// for (id, text, by) in [("a", "red running shoes", "nike"), ("b", "red hat", "acme")] {
///     let doc = Document::new(id).with(title, Value::String(text.into())).with(brand, Value::String(by.into()));
///     db.put("shoes", &doc, Durability::Fsync)?;
/// }
/// let snapshot = db.snapshot();
/// let shoes = snapshot.collection("shoes").ok_or("no collection")?;
///
/// // Built in code.
/// let nike = Filter::Term { field: brand, value: FilterValue::Keyword("nike".into()) };
/// let request = QueryRequest::new("red").with_filter(nike).with_limit(10);
/// // Or parsed from the request JSON: the same value.
/// let json = r#"{"q": "red", "filter": {"term": ["brand", "nike"]}, "limit": 10}"#;
/// assert_eq!(QueryRequest::from_json(json, shoes.schema(), &QueryLimits::default())?, request);
///
/// let response = request.run(shoes, &RunOptions::default())?;
/// // The collection relaxes by default, so the hits are a relaxed search's.
/// let QueryResults::Relaxed(found) = &response.results else { return Err("not relaxed".into()) };
/// assert_eq!(found.hits.len(), 1);
/// assert_eq!(found.hits[0].hit.document.id(), "a");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryRequest {
    pub(super) q: String,
    pub(super) fields: Option<DefaultFields>,
    pub(super) filter: Option<Filter>,
    pub(super) soft_filter: Option<Filter>,
    pub(super) page: SearchRequest,
    pub(super) relax: RelaxOptions,
    pub(super) proximity: bool,
}

impl QueryRequest {
    /// A request for the user text `q` (SPEC §7.1.1), everything else at its default.
    pub fn new(q: impl Into<String>) -> Self {
        Self {
            q: q.into(),
            ..Self::default()
        }
    }

    /// Searches plain words and phrases in `fields` with their weights (`fields`), in place of every `text` field
    /// with its schema `boost`.
    #[must_use]
    pub fn with_fields(mut self, fields: DefaultFields) -> Self {
        self.fields = Some(fields);
        self
    }

    /// Selects only the documents `filter` selects (`filter`, SPEC §7.1.2): a hard filter, which no relaxation level
    /// drops. It replaces an earlier one; several are one [`Filter::And`].
    #[must_use]
    pub fn with_filter(mut self, filter: Filter) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Selects only the documents `filter` selects (`soft_filter`, SPEC §7.1.2), until the filter step of
    /// `relax.filters = "last_resort"` drops it (SPEC §7.6). A filter equal to the hard one is hard.
    #[must_use]
    pub fn with_soft_filter(mut self, filter: Filter) -> Self {
        self.soft_filter = Some(filter);
        self
    }

    /// Ranks hits by `sort` (`sort`, SPEC §7.4).
    #[must_use]
    pub fn with_sort(mut self, sort: Sort) -> Self {
        self.page.sort = sort;
        self
    }

    /// Returns at most `limit` hits (`limit`); `offset + limit` is at most [`super::MAX_WINDOW`].
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.page.limit = limit;
        self
    }

    /// Skips `offset` hits from the top, or from the cursor (`offset`).
    #[must_use]
    pub fn with_offset(mut self, offset: usize) -> Self {
        self.page.offset = offset;
        self
    }

    /// Starts the page after `cursor`, a hit's cursor of the page before (`search_after`, SPEC §7.4).
    #[must_use]
    pub fn with_search_after(mut self, cursor: Cursor) -> Self {
        self.page.search_after = Some(cursor);
        self
    }

    /// Counts `facets` over every match (`facets`, SPEC §7.5).
    #[must_use]
    pub fn with_facets(mut self, facets: impl IntoIterator<Item = FacetRequest>) -> Self {
        self.page.facets = facets.into_iter().collect();
        self
    }

    /// Returns each hit's stored fields as `projection` says (`projection`, SPEC §7.5.1).
    #[must_use]
    pub fn with_projection(mut self, projection: Projection) -> Self {
        self.page.projection = projection;
        self
    }

    /// Returns with each hit how its score adds up (`explain`, SPEC §7.7).
    #[must_use]
    pub fn with_explain(mut self, explain: bool) -> Self {
        self.page.explain = explain;
        self
    }

    /// Overrides the collection's relaxation settings, one setting at a time (`relax`, SPEC §7.6);
    /// [`RelaxOptions::disabled`] is `relax: false`.
    #[must_use]
    pub fn with_relax(mut self, relax: RelaxOptions) -> Self {
        self.relax = relax;
        self
    }

    /// Scores the proximity bonus (`proximity`, SPEC §7.2, ADR-0036). Only a search that does not relax takes it:
    /// with it, [`Self::run`] needs `relax: false` ([`QueryRequestError::ProximityRelaxed`]).
    #[must_use]
    pub fn with_proximity(mut self, proximity: bool) -> Self {
        self.proximity = proximity;
        self
    }

    /// The user text (`q`).
    pub fn q(&self) -> &str {
        &self.q
    }

    /// The fields plain words search (`fields`); `None` for every `text` field with its schema `boost`.
    pub fn fields(&self) -> Option<&DefaultFields> {
        self.fields.as_ref()
    }

    /// The hard filter (`filter`).
    pub fn filter(&self) -> Option<&Filter> {
        self.filter.as_ref()
    }

    /// The soft filter (`soft_filter`).
    pub fn soft_filter(&self) -> Option<&Filter> {
        self.soft_filter.as_ref()
    }

    /// The sort, the page (`offset`, `limit`, `search_after`), the facets, the projection and `explain`.
    pub fn page(&self) -> &SearchRequest {
        &self.page
    }

    /// The relaxation settings the request overrides (`relax`).
    pub fn relax(&self) -> &RelaxOptions {
        &self.relax
    }

    /// Whether the search scores the proximity bonus (`proximity`).
    pub fn proximity(&self) -> bool {
        self.proximity
    }

    /// The fingerprint every hit's cursor of this request records when it runs with `limits` on a collection of
    /// `schema` ([`crate::db::Cursor::request`], SPEC §7.4, ROADMAP 10.1.24): a hash of `q`, the fields it searches
    /// (every `text` field with its schema boost when [`Self::fields`] is `None`), `filter`, `soft_filter` and
    /// `limits`, the parts its ranking depends on that the cursor records nowhere else. [`Self::run`] refuses a cursor
    /// of another fingerprint ([`RequestError::CursorRequest`]). The page, the facets, the projection and `explain` do
    /// not count: they do not change the ranking.
    ///
    /// ```
    /// use persia_engine::query::QueryLimits;
    /// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    /// use persia_engine::search::request::QueryRequest;
    ///
    /// let schema = SchemaBuilder::new("c").field("title", FieldKind::Text(TextOptions::default())).build()?;
    /// let limits = QueryLimits::default();
    /// let red = QueryRequest::new("red");
    /// // Another page of the same query is the same request; another query is not.
    /// assert_eq!(red.fingerprint(&schema, &limits), red.clone().with_limit(5).fingerprint(&schema, &limits));
    /// assert_ne!(red.fingerprint(&schema, &limits), QueryRequest::new("blue").fingerprint(&schema, &limits));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn fingerprint(&self, schema: &CollectionSchema, limits: &QueryLimits) -> u64 {
        let fields = self
            .fields
            .as_ref()
            .map_or_else(|| Cow::Owned(DefaultFields::all(schema)), Cow::Borrowed);
        self.fingerprint_over(&fields, limits)
    }

    fn fingerprint_over(&self, fields: &DefaultFields, limits: &QueryLimits) -> u64 {
        fingerprint(
            &self.q,
            fields,
            (self.filter.as_ref(), self.soft_filter.as_ref()),
            limits,
        )
    }

    /// Runs the request on `collection` (SPEC §7.1, ROADMAP 10.1.10): compiles `q` with the collection's analyzers
    /// ([`CollectionSnapshot::compiler`]) over the request's fields, joins `filter` and `soft_filter` to it as a
    /// request's filters join `q` ([`crate::query::Compiled::with_filter`],
    /// [`crate::query::Compiled::with_soft_filter`]), holds the joined query to `options`' limits (SPEC §7.1.3), and
    /// searches the collection's segments and a view of its memtable taken now.
    ///
    /// The relaxation settings are the collection's as the request's `relax` overrides them (SPEC §7.6). When they
    /// relax, the search is the relaxed one ([`crate::db::CollectionViews::search_relaxed`], through
    /// `RelaxedQuery::from(&Compiled)`, so `filter` stays hard at every level), with its steps held to the same
    /// limits. When they run L0 alone (`relax: false`, or a collection that disables the cascade), it is the plain
    /// search ([`crate::db::CollectionViews::search_with`]), planned with the proximity bonus when the request asks
    /// for it (`SearchPlan::with_proximity`) and without it otherwise (`without_proximity`, ROADMAP 10.1.7).
    ///
    /// # Errors
    /// [`QueryRequestError::ProximityRelaxed`] for `proximity` on a search that relaxes;
    /// [`QueryRequestError::Schema`] for a part built for another schema; [`QueryRequestError::Query`] for a joined
    /// query over the limits or a filter that does not fit the schema; [`QueryRequestError::Relax`] for a `relax`
    /// value that breaks its rule; [`QueryRequestError::Request`] for a page, cursor, sort or facet the search
    /// refuses ([`RequestError::CursorProximity`] for a cursor from a search with the other proximity setting), a
    /// view that cannot be read, a deadline or a cancellation; [`QueryRequestError::Plan`] for a query that cannot be
    /// planned.
    pub fn run(
        &self,
        collection: &CollectionSnapshot,
        options: &RunOptions,
    ) -> Result<QueryResponse, QueryRequestError> {
        let schema = collection.schema();
        let limits = options.limits;
        self.check_schema(collection)?;
        let relax = RelaxPlan::new(schema.relaxation(), &self.relax)?.with_limits(limits);
        // The relaxed search plans every step without the bonus (SPEC §7.2): asked for, it would be ignored.
        if self.proximity && relax.relaxes() {
            return Err(QueryRequestError::ProximityRelaxed);
        }
        let fields = self
            .fields
            .as_ref()
            .map_or_else(|| Cow::Owned(DefaultFields::all(schema)), Cow::Borrowed);
        let compiled = collection
            .compiler()
            .compile(&self.q, &fields, &limits)
            .with_filter(self.filter.clone())
            .with_soft_filter(self.soft_filter.clone());
        // The filters add their nodes, terms and a level of depth to the compiled query (SPEC §7.1.3).
        compiled.query().validate(schema, &limits)?;
        // After the validation: the filters are within the limits' depth.
        let fingerprint = self.fingerprint_over(&fields, &limits);
        let memtable = collection.memtable();
        let views = collection.views(&memtable).map_err(RequestError::from)?;
        let results = if relax.relaxes() {
            QueryResults::Relaxed(views.search_relaxed_for(
                (&compiled).into(),
                &self.page,
                &relax,
                &options.exec,
                Some(fingerprint),
            )?)
        } else {
            let stats = collection.search_statistics(&memtable);
            let terms = options.exec.terms(&views);
            let plan = SearchPlan::with_terms(compiled.query(), schema, &stats, &terms)?;
            let plan = if self.proximity {
                plan.with_proximity()
            } else {
                plan.without_proximity()
            };
            QueryResults::Plain(views.search_for(
                &plan,
                &self.page,
                &options.exec,
                Some(fingerprint),
            )?)
        };
        Ok(QueryResponse {
            results,
            query_truncated: compiled.truncated,
            exact: Relaxation::exact(&compiled),
        })
    }

    /// Checks the parts that name fields by id against `collection`'s schema, as their constructors did: default
    /// fields of this schema, a sort over fields with a column, facets over `facet: true` keywords. The filters are
    /// checked with the query they join.
    fn check_schema(&self, collection: &CollectionSnapshot) -> Result<(), QueryRequestError> {
        let schema = collection.schema();
        if self.fields.as_ref().is_some_and(|f| !f.is_for(schema)) {
            return Err(QueryRequestError::Schema {
                collection: schema.name().to_owned(),
            });
        }
        Sort::new(self.page.sort.keys(), schema)?;
        for facet in &self.page.facets {
            FacetRequest::new(facet.field(), facet.size(), schema)?;
        }
        Ok(())
    }
}

/// How a request runs (SPEC §7.3, §7.6): the threads its views run on, a deadline, a cancellation token, the clock of
/// the relaxation budget, and the query limits (SPEC §7.1.3). The default runs on the shared pool
/// ([`Threads::Global`]) with neither a deadline nor a token, reads the budget on the system clock, and holds the
/// query to [`QueryLimits::default`].
///
/// ```
/// use std::sync::Arc;
/// use std::time::Duration;
/// use persia_engine::db::{CancelToken, Clock, ManualClock, Threads};
/// use persia_engine::search::request::RunOptions;
///
/// let clock = Arc::new(ManualClock::new(0));
/// let at = clock.now() + Duration::from_millis(50);
/// let token = CancelToken::new();
/// let options = RunOptions::new().threads(Threads::Current).deadline(clock, at).cancel(token.clone());
/// # let _ = options;
/// ```
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    exec: Execution,
    limits: QueryLimits,
}

impl RunOptions {
    /// The defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs the search's views on `threads`.
    #[must_use]
    pub fn threads(mut self, threads: Threads) -> Self {
        self.exec = self.exec.threads(threads);
        self
    }

    /// Stops the search with [`Interrupt::DeadlineExceeded`] once `clock` reads `at` or later (SPEC §7.3). The
    /// relaxation budget reads the same clock unless [`Self::budget_clock`] gives another.
    #[must_use]
    pub fn deadline(mut self, clock: Arc<dyn Clock>, at: Instant) -> Self {
        self.exec = self.exec.deadline(clock, at);
        self
    }

    /// Stops the search with [`Interrupt::Cancelled`] once `token` is cancelled.
    #[must_use]
    pub fn cancel(mut self, token: CancelToken) -> Self {
        self.exec = self.exec.cancel(token);
        self
    }

    /// Reads the relaxation cascade's time budget (`relax.budget_ms`, SPEC §7.6) on `clock`.
    #[must_use]
    pub fn budget_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.exec = self.exec.budget_clock(clock);
        self
    }

    /// Holds the query, its filters and every relaxation step to `limits` (SPEC §7.1.3), and reads
    /// `limits.max_text_len` bytes of `q`.
    #[must_use]
    pub fn limits(mut self, limits: QueryLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// What [`QueryRequest::run`] returns.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct QueryResponse {
    /// The hits and facet counts.
    pub results: QueryResults,
    /// Whether part of `q` was left out to stay within the limits (`query_truncated`, SPEC §7.1.1); not the
    /// relaxation budget's `truncated` ([`crate::db::Relaxation::truncated`]).
    pub query_truncated: bool,
    /// What a plain search states as its `relaxation` (SPEC §7.6): level `exact`, nothing applied, L0's text.
    pub(super) exact: Relaxation,
}

impl QueryResponse {
    /// What the relaxation cascade did (SPEC §7.6): the relaxed search's [`RelaxedResults::relaxation`], or for the
    /// plain search (`relax: false`) level `exact` with nothing applied, not truncated, and the text of `q` as L0 states
    /// it (`original` as read, `effective` the clauses that compiled), as a relaxed search that stops at L0 states it.
    pub fn relaxation(&self) -> &Relaxation {
        match &self.results {
            QueryResults::Relaxed(results) => &results.relaxation,
            QueryResults::Plain(_) => &self.exact,
        }
    }
}

/// The results of a request, by the search that ran (SPEC §7.6).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum QueryResults {
    /// Of the relaxed search: the request's settings relax. Each hit states its level, and
    /// [`RelaxedResults::relaxation`] what the cascade did.
    Relaxed(RelaxedResults),
    /// Of the plain search: the settings run L0 alone (`relax: false`). A response states it as level `exact` with
    /// nothing applied (SPEC §7.6).
    Plain(SearchResults),
}

/// Why a request cannot be parsed or run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueryRequestError {
    /// The JSON is not a request (SPEC §7.1): malformed, longer than [`super::MAX_REQUEST_JSON_LEN`], not an object,
    /// with an unknown or repeated key, `null` for a key, or a key of another shape than its own.
    #[error("request: {0}")]
    Json(String),
    /// `fields`, `filter` or `soft_filter` does not fit the schema or the limits, or the query with its filters is
    /// over a limit (SPEC §7.1.2, §7.1.3).
    #[error(transparent)]
    Query(#[from] QueryError),
    /// `relax` is not its JSON form or breaks a rule (SPEC §7.6).
    #[error(transparent)]
    Relax(#[from] RelaxError),
    /// `search_after` is not a cursor token this build reads (SPEC §7.4).
    #[error("search_after: {0}")]
    Cursor(#[from] CursorTokenError),
    /// `proximity: true` on a search that relaxes (SPEC §7.2, ROADMAP 10.1.7): the relaxed search plans every step
    /// without the bonus, so the option is taken only by a search that runs L0 alone. The fix is in the message.
    #[error(
        "\"proximity\": true needs \"relax\": false: a search that relaxes does not score the proximity bonus"
    )]
    ProximityRelaxed,
    /// A part of the request was built for another schema than the collection's (its default fields).
    #[error("the request was built for another schema than collection {collection:?} has")]
    Schema {
        /// The collection searched.
        collection: String,
    },
    /// The sort, the page, the cursor, a facet or the projection does not fit (SPEC §7.4, §7.5), a view could not be
    /// read, or the search was cancelled or ran past its deadline.
    #[error(transparent)]
    Request(#[from] RequestError),
    /// The query, or a relaxation step, could not be planned.
    #[error(transparent)]
    Plan(#[from] SearchError),
}

impl QueryRequestError {
    /// The deadline or cancellation that stopped the search (SPEC §7.3), when that is why it failed: not an invalid
    /// request, and not a failure of the database.
    pub fn interrupt(&self) -> Option<Interrupt> {
        if let Self::Request(RequestError::Interrupted(i))
        | Self::Plan(SearchError::Interrupted(i)) = self
        {
            Some(*i)
        } else {
            None
        }
    }
}

impl From<RelaxedSearchError> for QueryRequestError {
    fn from(e: RelaxedSearchError) -> Self {
        match e {
            RelaxedSearchError::Plan(e) => Self::Plan(e),
            RelaxedSearchError::Request(e) => Self::Request(e),
        }
    }
}
