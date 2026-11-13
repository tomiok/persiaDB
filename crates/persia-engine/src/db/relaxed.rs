//! A search that relaxes its query when it finds too few hits (SPEC §7.6, ROADMAP 8.1.2, 8.1.3): the relaxation
//! cascade over [`CollectionViews`].
//!
//! The cascade runs [`RelaxPlan`]'s steps in order, L0 (the query itself) first. Each step after the first searches
//! its query with every earlier step's query excluded (`must_not`), so a document is a hit of the first step that
//! matches it, at that step's score, and appears once. After each step the hits found so far are counted: the cascade
//! stops once they reach `min_results` (and, with `fill`, the page's window). The page is the steps' hits in step
//! order, each step's by the request's sort, from `offset`.
//!
//! Each step collects only the hits still needed (`max(offset + limit, min_results)` from the top, less those found),
//! so the decision to relax counts hits from the top whatever the page, and a query that has `min_results` hits at L0
//! runs one search of that query, as [`CollectionViews::search_with`] does.
//!
//! Hits rank by step first, then by the sort (ROADMAP 8.1.3): every hit of a stricter step above every hit of a
//! looser one, whatever their scores. A hit's cursor records its step and the last step of its page's cascade, so the
//! next page builds the same steps, searches the cursor's step after the cursor and the later ones from their top,
//! and the pages are slices of one ranking.

use std::borrow::Cow;
use std::time::Instant;

use super::exec::{ExecTerms, Execution};
use super::search::{Collected, Cursor, Part, Ranked, SearchHit, SearchRequest, StepCursor};
use super::views::CollectionViews;
use crate::query::{BoolQuery, Compiled, Fit, Query, QueryLimits, StepLimits};
use crate::relax::{Expansion, LevelInput, PlannedLevel, RelaxLevel, RelaxPlan, rank_score};
use crate::schema::{CollectionSchema, FieldId, FieldKind};
use crate::search::{FacetCounter, FacetCounts, Interrupt, RequestError, SearchError, SearchPlan};
use crate::stats::SearchStatistics;

/// A hit of a relaxed search: the hit, and the step of the cascade that found it.
#[derive(Debug, Clone, PartialEq)]
pub struct RelaxedHit {
    /// The hit. Its score is its step's.
    pub hit: SearchHit,
    /// The level of the step that matched it first.
    pub level: RelaxLevel,
    /// That step's index in the cascade: 0 is L0's query.
    pub step: usize,
    /// [`rank_score`] of the step and the score: never below a later step's, so it does not increase down a page
    /// sorted by score descending.
    pub rank_score: f64,
}

impl RelaxedHit {
    /// The cursor of the page after this hit (SPEC §7.4, §7.6), for [`CollectionViews::search_relaxed`]: the same as
    /// [`SearchHit::cursor`] of [`Self::hit`], which records the hit's step and the last step of its cascade too.
    pub fn cursor(&self) -> Cursor {
        self.hit.cursor()
    }
}

/// What the relaxation cascade did, as a response states it (SPEC §7.6): its `level`, what it `applied`, the query's
/// `original` and `effective` text, the terms it `corrected`, and whether its budget `truncated` it.
///
/// ```
/// use persia_engine::db::{Database, DatabaseOptions, Execution, SearchRequest};
/// use persia_engine::document::{Document, Value};
/// use persia_engine::query::{Query, TermQuery};
/// use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
/// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
/// use persia_engine::wal::Durability;
/// use persia_storage::container::sim::SimFile;
///
/// let schema = SchemaBuilder::new("c").field("body", FieldKind::Text(TextOptions::default())).build()?;
/// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
/// let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default())?;
/// db.create_collection(schema)?;
/// db.put("c", &Document::new("a").with(body, Value::String("red shoes".into())), Durability::Fsync)?;
///
/// let snapshot = db.snapshot();
/// let c = snapshot.collection("c").ok_or("no collection")?;
/// let memtable = c.memtable();
/// let views = c.views(&memtable)?;
/// // The collection's defaults, as a query that gives no `relax` gets them.
/// let relax = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::default())?;
/// let red = Query::Term(TermQuery { field: body, term: "red".into() });
/// let out = views.search_relaxed(&red, &SearchRequest::default(), &relax, &Execution::default())?;
/// let relaxation = &out.relaxation;
/// // `red` has a hit at L0: nothing was relaxed.
/// assert_eq!(relaxation.level, RelaxLevel::Exact);
/// assert_eq!(relaxation.level.name(), "exact");
/// assert!(!relaxation.relaxed());
/// assert!(!relaxation.truncated);
/// // A programmatic query has no user text, so no `original` or `effective`; `corrected` waits on ROADMAP 8.2.4.3.
/// assert_eq!((relaxation.original.as_deref(), relaxation.effective.as_deref()), (None, None));
/// assert!(relaxation.corrected.is_empty());
/// // Each hit states the step that found it, at L0 here.
/// assert_eq!(out.hits[0].level, RelaxLevel::Exact);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Relaxation {
    /// The level of the last step built and not skipped (`"level"`): [`RelaxLevel::Exact`] when L0 found enough or no
    /// later level built a step that fits the limits. A step built is one the cascade reached: searched, or on a
    /// cursor's page only excluded. Without `fill`, every page of one query on one snapshot states the same, unless the
    /// budget truncated it; with `fill`, a deeper page may build, and state, more steps.
    pub level: RelaxLevel,
    /// What each step after L0 applied, in the order they were built (`"applied"`): `"drop_terms:75%"`, `"fuzzy"`.
    /// Empty when no level built a step. A step held to the limits (SPEC §7.1.3, [`RelaxPlan::with_limits`]) states
    /// it: [`Self::CUT`] after one whose expansions were cut to fit (`"prefix:cut"`), [`Self::SKIPPED`] after one that
    /// fits no cut, which was not searched (`"prefix:skipped"`).
    pub applied: Vec<String>,
    /// The user text the query was compiled from (`"original"`), as read: [`Compiled::text`]. `None` when the query
    /// was not compiled from `q` (a programmatic query): it has no user text, and the AST has no faithful `q` form
    /// (combined weights, boost, prefix, fuzzy).
    pub original: Option<String>,
    /// The text of the last step's query (`"effective"`), as it reads after relaxing: `"zapatillas rojas"`. Rendered
    /// from its clauses' source text ([`crate::query::CompiledClause::span`]), so at L0 it is [`Compiled::effective`]:
    /// the clauses that compiled, without stopwords or text past a limit. `None` when [`Self::original`] is, and when the
    /// last step's level renders no text for its query. At L2 `drop_terms` it is the clauses the step keeps: its count of
    /// the most important terms and every excluded clause, in text order (`"purple -blue"`).
    pub effective: Option<String>,
    /// The terms the fuzzy level replaced, in query order (`"corrected"`). Empty until ROADMAP 8.2.4.3 synthesizes
    /// the corrected query ("did you mean").
    pub corrected: Vec<Correction>,
    /// The budget (`budget_ms`) passed before the cascade finished (SPEC §7.6): the hits are those of the steps
    /// completed before it, and `level` and `applied` describe those steps.
    pub truncated: bool,
    /// Whether a step after L0 was built and not skipped: [`Self::relaxed`].
    relaxed: bool,
}

impl Relaxation {
    /// Ends an [`Self::applied`] entry whose step had its `prefix` and `fuzzy` expansions cut to fit the limits beside
    /// the steps it excludes: the largest `max_expansions` first, none below 1 (SPEC §7.1.3).
    pub const CUT: &str = ":cut";
    /// Ends an [`Self::applied`] entry whose step did not fit the limits even cut: it was not searched, and later
    /// steps do not exclude its matches (SPEC §7.1.3).
    pub const SKIPPED: &str = ":skipped";

    /// What a cascade that built steps through `level` (`relaxed` when that step is after L0), applying `applied`, did,
    /// for a query compiled from `original` whose last step reads `effective`. The corrections wait on ROADMAP 8.2.4.3.
    fn of(
        (level, relaxed): (RelaxLevel, bool),
        applied: Vec<String>,
        text: (Option<String>, Option<String>),
        truncated: bool,
    ) -> Self {
        let (original, effective) = text;
        Self {
            level,
            applied,
            original,
            effective,
            corrected: Vec::new(),
            truncated,
            relaxed,
        }
    }

    /// Whether the cascade built a step after L0 that it did not skip, so the response may hold hits a strict query
    /// does not match. False when every step after L0 was skipped.
    pub fn relaxed(&self) -> bool {
        self.relaxed
    }
}

/// A term the fuzzy level replaced (SPEC §7.6 `"corrected"`, ROADMAP 8.2.4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    /// The term as the user wrote it: `"zapatilas"`.
    pub original: String,
    /// What the response searched instead: `"zapatillas"`.
    pub corrected: String,
}

/// The query a relaxed search starts from ([`CollectionViews::search_relaxed`]): a [`Query`] built in code, or one
/// [`Compiled`] from user text, whose text the response states (`original`, `effective`, SPEC §7.6).
#[derive(Debug, Clone, Copy)]
pub struct RelaxedQuery<'a> {
    query: &'a Query,
    compiled: Option<&'a Compiled>,
}

impl<'a> From<&'a Query> for RelaxedQuery<'a> {
    fn from(query: &'a Query) -> Self {
        Self {
            query,
            compiled: None,
        }
    }
}

impl<'a> From<&'a Compiled> for RelaxedQuery<'a> {
    fn from(compiled: &'a Compiled) -> Self {
        Self {
            query: &compiled.query,
            compiled: Some(compiled),
        }
    }
}

/// What a relaxed search returns.
#[derive(Debug, Clone, PartialEq)]
pub struct RelaxedResults {
    /// The page, best first: the hits of each step in step order.
    pub hits: Vec<RelaxedHit>,
    /// One per requested facet, in request order, over the matches of every step built.
    pub facets: Vec<FacetCounts>,
    /// What the cascade did.
    pub relaxation: Relaxation,
}

/// Why a relaxed search failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RelaxedSearchError {
    /// A step's query could not be planned (or planning was interrupted).
    #[error(transparent)]
    Plan(#[from] SearchError),
    /// The request does not fit, or a step's search failed.
    #[error(transparent)]
    Request(#[from] RequestError),
}

/// A step the cascade built, and what its search found when it ran one.
struct Ran {
    level: RelaxLevel,
    searched: Option<(SearchPlan, Collected)>,
    /// Not searched: it does not fit the limits (SPEC §7.1.3), and later steps do not exclude it.
    skipped: bool,
}

/// The steps the cascade built so far, and what each excludes from the next.
struct Built<'q, 's> {
    ran: Vec<Ran>,
    /// The queries of the steps built and not skipped that every later step excludes: each such step, except a step of
    /// a nested level that a later step of its level stands for (ROADMAP 8.2.2.4).
    earlier: Vec<Cow<'q, Query>>,
    /// Where the current level's steps start in `earlier`.
    level_start: usize,
    applied: Vec<String>,
    /// The text of the last step searched (SPEC §7.6 `effective`).
    effective: Option<String>,
    schema: &'s CollectionSchema,
    limits: QueryLimits,
    /// Each step after L0 is held to the limits beside the steps it excludes, L0 first (SPEC §7.1.3). Built at the
    /// first such step: a query with enough hits at L0 does not walk its query for them.
    fitting: Option<StepLimits<'s>>,
}

impl<'q, 's> Built<'q, 's> {
    fn new(schema: &'s CollectionSchema, limits: QueryLimits) -> Self {
        Self {
            ran: Vec::new(),
            earlier: Vec::new(),
            level_start: 0,
            applied: Vec::new(),
            effective: None,
            schema,
            limits,
            fitting: None,
        }
    }

    /// The next step held to the limits ([`held`]); L0 (`query`) as it is.
    ///
    /// # Errors
    /// [`SearchError::Query`] for an invalid step, or an invalid L0 (checked at the first step after it, as a cursor's
    /// page may not plan L0).
    fn fit(
        &mut self,
        query: &Query,
        step: Cow<'q, Query>,
        label: String,
        alternatives: Option<&Expansion>,
    ) -> Result<Held<'q>, SearchError> {
        if self.ran.is_empty() {
            return Ok(Held {
                step: Some(step),
                label,
                cut: false,
            });
        }
        let fitting = match &mut self.fitting {
            Some(fitting) => fitting,
            slot @ None => slot.insert(StepLimits::new(self.schema, self.limits, query)?),
        };
        held(fitting, step, label, alternatives)
    }

    /// The steps pushed from now on are the next level's.
    fn begin_level(&mut self) {
        self.level_start = self.earlier.len();
    }

    /// A step of `level` that does not fit: listed, not searched, excluded from none.
    fn skip(&mut self, level: RelaxLevel, label: String) {
        self.applied.push(label);
        self.ran.push(Ran {
            level,
            searched: None,
            skipped: true,
        });
    }

    /// A step of `level` built (and `searched`, unless no hit or facet was wanted of it): excluded from every later one.
    /// One that `stands_for_level` (a nested level's step as built, ROADMAP 8.2.2.4) matches every document its level's
    /// earlier steps match, so it replaces them in what the later steps exclude.
    fn push(
        &mut self,
        level: RelaxLevel,
        step: Cow<'q, Query>,
        label: String,
        text: Option<String>,
        searched: Option<(SearchPlan, Collected)>,
        stands_for_level: bool,
    ) {
        let first = self.ran.is_empty();
        if !first {
            self.applied.push(label);
        }
        self.ran.push(Ran {
            level,
            searched,
            skipped: false,
        });
        let replaced = stands_for_level && self.earlier.len() > self.level_start;
        if replaced {
            self.earlier.truncate(self.level_start);
        }
        self.earlier.push(step);
        // L0 is counted when the fitting is built, at the first step after it.
        if let Some(fitting) = self.fitting.as_mut().filter(|_| !first) {
            if replaced {
                fitting.recount(self.earlier.iter().map(AsRef::as_ref));
            } else if let Some(step) = self.earlier.last() {
                fitting.exclude(step);
            }
        }
        self.effective = text;
    }
}

/// Where the cascade starts and how far it must go: from a request's cursor, or from the top.
#[derive(Debug, Clone, Copy)]
struct Start {
    /// The step whose hits the page starts in, after the cursor.
    step: usize,
    /// The cascade builds at least the steps through this one (the cursor's page did).
    last: Option<usize>,
}

impl Start {
    /// From the top without a cursor; from `cursor`'s step otherwise, one without a step being step 0 of a cascade
    /// that stopped at L0.
    ///
    /// # Errors
    /// [`RequestError::CursorStep`] for a cursor from a step after L0 when `relax` does not relax: every hit it
    /// returns ranks before the cursor, as for a plain search.
    fn of(cursor: Option<&Cursor>, relax: &RelaxPlan) -> Result<Self, RequestError> {
        let Some(cursor) = cursor else {
            return Ok(Self {
                step: 0,
                last: None,
            });
        };
        let at = cursor.relaxed.unwrap_or(StepCursor { step: 0, last: 0 });
        if at.step > 0 && !relax.relaxes() {
            return Err(RequestError::CursorStep {
                step: at.step,
                last: at.last,
                reason: "a search that does not relax returns L0's hits only, which rank before it",
            });
        }
        Ok(Self {
            step: at.step,
            last: Some(if relax.relaxes() { at.last } else { 0 }),
        })
    }

    /// The hits the cascade counts: from the top, enough to tell `min_results` and fill the window; after a cursor
    /// only the window, the decision being its page's (`last`).
    fn need(self, window: usize, min_results: usize) -> usize {
        if self.last.is_none() {
            window.max(min_results)
        } else {
            window
        }
    }

    /// Whether the cascade builds step `i`, with `found` hits found before it, `need` from [`Self::need`].
    fn builds(
        self,
        i: usize,
        found: usize,
        relax: &RelaxPlan,
        min_results: usize,
        need: usize,
    ) -> bool {
        i == 0
            || relax.relaxes()
                && match self.last {
                    None => found < min_results || relax.fill() && found < need,
                    Some(last) => i <= last || relax.fill() && found < need,
                }
    }
}

impl CollectionViews<'_> {
    /// Searches `query` with `request`, relaxing it as `relax` says when it finds fewer than `min_results` hits (SPEC
    /// §7.6): each of the plan's steps in turn, until the hits found reach `min_results` (with
    /// [`RelaxPlan::fill`], also `offset + limit`) or the steps run out. Each step is planned over these views
    /// ([`SearchPlan::with_terms`] under `exec`, with [`super::CollectionSnapshot::search_statistics`] of their
    /// memtable view) and searched as [`Self::search_with`] searches, its earlier steps' matches excluded.
    /// Each step after L0 is held to [`RelaxPlan::limits`] (else a compiled query's own) as the query it runs, with the
    /// earlier steps it excludes (SPEC §7.1.3): over them, its expansions are cut to fit ([`Relaxation::CUT`]), or it is
    /// skipped, not searched and not excluded from later steps ([`Relaxation::SKIPPED`]).
    ///
    /// With a `search_after` cursor ([`RelaxedHit::cursor`]), the steps through the cursor's last one are built
    /// again, those before the cursor's step are not searched for hits (they rank before it; with facets they are
    /// still counted), the cursor's step is searched after the cursor and later ones from their top, until the page
    /// is full or the last step is passed (with `fill`, until the page is full). A cursor without a step
    /// ([`SearchHit::cursor`]) is step 0 of a cascade that stopped at L0.
    ///
    /// `query` is a [`Query`] or a [`Compiled`] one ([`RelaxedQuery`]); only a compiled query has the user text the
    /// response's [`Relaxation::original`] and [`Relaxation::effective`] state.
    ///
    /// ```
    /// use persia_analysis::AnalyzerRegistry;
    /// use persia_engine::db::{Database, DatabaseOptions, Execution, SearchRequest};
    /// use persia_engine::document::{Document, Value};
    /// use persia_engine::query::{DefaultFields, Query, QueryCompiler, QueryLimits, TermQuery};
    /// use persia_engine::relax::{RelaxOptions, RelaxPlan};
    /// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    /// use persia_engine::wal::Durability;
    /// use persia_storage::container::sim::SimFile;
    ///
    /// let schema = SchemaBuilder::new("c").field("body", FieldKind::Text(TextOptions::default())).build()?;
    /// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
    /// let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default())?;
    /// db.create_collection(schema)?;
    /// for (id, text) in [("a", "red shoes"), ("b", "red hat")] {
    ///     db.put("c", &Document::new(id).with(body, Value::String(text.into())), Durability::Fsync)?;
    /// }
    /// let snapshot = db.snapshot();
    /// let c = snapshot.collection("c").ok_or("no collection")?;
    /// let memtable = c.memtable();
    /// let views = c.views(&memtable)?;
    /// let relax = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::default())?;
    /// let red = Query::Term(TermQuery { field: body, term: "red".into() });
    /// let exec = Execution::default();
    ///
    /// let page = SearchRequest { limit: 1, ..SearchRequest::default() };
    /// let first = views.search_relaxed(&red, &page, &relax, &exec)?;
    /// // The next page starts after the last hit, at that hit's step of the cascade.
    /// let next = SearchRequest { search_after: first.hits.last().map(|h| h.cursor()), ..page };
    /// let second = views.search_relaxed(&red, &next, &relax, &exec)?;
    /// let ids: Vec<&str> = first.hits.iter().chain(&second.hits).map(|h| h.hit.document.id()).collect();
    /// assert_eq!(ids.len(), 2);
    /// assert_ne!(ids[0], ids[1]);
    ///
    /// // Compiled from user text, the response states that text, and the text of the query it searched.
    /// let compiler = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin())?;
    /// let compiled = compiler.compile("red   shoes", &DefaultFields::all(c.schema()), &QueryLimits::default());
    /// let out = views.search_relaxed(&compiled, &SearchRequest::default(), &relax, &exec)?;
    /// assert_eq!(out.relaxation.original.as_deref(), Some("red   shoes"));
    /// assert_eq!(out.relaxation.effective.as_deref(), Some("red shoes"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Self::search_with`]; [`RequestError::CursorStep`] for a cursor whose step is after its last, or from a step
    /// after L0 when `relax` does not relax;
    /// [`RelaxedSearchError::Plan`] when a step cannot be planned, or a level cannot read the statistics it builds its steps
    /// from (L2 reads its terms' `df`).
    pub fn search_relaxed<'q>(
        &self,
        query: impl Into<RelaxedQuery<'q>>,
        request: &SearchRequest,
        relax: &RelaxPlan,
        exec: &Execution,
    ) -> Result<RelaxedResults, RelaxedSearchError> {
        let RelaxedQuery { query, compiled } = query.into();
        // Every step is planned as `SearchPlan::with_terms` plans by default: without proximity (SPEC §7.4).
        let window = self.check_request(request, false)?;
        let collection = self.collection();
        let stats = collection.search_statistics(self.memtable());
        let start = Start::of(request.search_after.as_ref(), relax)?;
        let min_results = usize::try_from(relax.min_results()).unwrap_or(usize::MAX);
        let need = start.need(window, min_results);
        let builds = |i: usize, found: usize| start.builds(i, found, relax, min_results, need);
        let counting = !request.facets.is_empty();

        // The plan's limits, else those the query was compiled with (SPEC §7.1.3).
        let limits = relax
            .limits()
            .copied()
            .or_else(|| compiled.map(|c| c.limits))
            .unwrap_or_default();
        let expands = |field: FieldId| self.expands(field);
        let mut built = Built::new(collection.schema(), limits);
        let mut found = 0usize;
        // The end of the budget, read from when L0 finishes (SPEC §7.6): L0 itself is never cut by it.
        let mut budget: Option<Instant> = None;
        let clock = exec.relax_clock();
        let mut truncated = false;
        'levels: for planned in relax.planned() {
            // Checked before a level builds its steps: one the cascade does not reach builds nothing.
            if !builds(built.ran.len(), found) {
                break;
            }
            if budget.is_some_and(|end| clock.now() >= end) {
                truncated = true;
                break;
            }
            let input = LevelInput::new(query, compiled, &stats).expanding(&expands);
            let steps = level_steps(planned, built.ran.is_empty(), input, exec)?;
            built.begin_level();
            let nested = planned.transform.nested() && relax.excludes_latest_only();
            for (step, label, text, alternatives) in steps {
                let i = built.ran.len();
                if !builds(i, found) {
                    break 'levels;
                }
                if budget.is_some_and(|end| clock.now() >= end) {
                    truncated = true;
                    break 'levels;
                }
                let Held { step, label, cut } =
                    built.fit(query, step, label, alternatives.as_ref())?;
                let Some(step) = step else {
                    built.skip(planned.level, label);
                    continue;
                };
                // Hits this step is to collect: none before the cursor's step, or once the page is full.
                let wanted = if i >= start.step {
                    need.saturating_sub(found)
                } else {
                    0
                };
                let searched = if wanted > 0 || counting {
                    let part = Part {
                        sort: &request.sort,
                        after: request.search_after.as_ref().filter(|_| i == start.step),
                        facets: &request.facets,
                        explain: request.explain,
                        window: wanted,
                    };
                    let Some(searched) =
                        self.budgeted_step(&step, &built.earlier, &stats, &part, exec, budget)?
                    else {
                        truncated = true;
                        break 'levels;
                    };
                    found = found.saturating_add(searched.1.ranked.len());
                    Some(searched)
                } else {
                    None
                };
                // A cut step matches less than its level built (SPEC §7.1.3): it stands for no earlier step, which an
                // uncut later one does again (a cut step's matches are its uncut ones', which the later step holds).
                built.push(planned.level, step, label, text, searched, nested && !cut);
                if budget.is_none() {
                    budget = clock.now().checked_add(relax.budget());
                }
            }
        }
        let Built {
            ran,
            applied,
            effective,
            ..
        } = built;
        // The last step built and not skipped, and whether it is after L0.
        let level = ran
            .iter()
            .enumerate()
            .rfind(|(_, r)| !r.skipped)
            .map_or((RelaxLevel::Exact, false), |(i, r)| (r.level, i > 0));
        // The cursor's page built at least its last step; with fewer steps now (another plan), the page ends sooner.
        let last = ran.len().saturating_sub(1).max(start.last.unwrap_or(0));
        let (hits, facets) = self.relaxed_page(ran, request, start.step, last, exec)?;
        Ok(RelaxedResults {
            hits,
            facets,
            relaxation: Relaxation::of(
                level,
                applied,
                // A query without user text has no text form, whatever a level states.
                (compiled.map(|c| c.text.clone()), compiled.and(effective)),
                truncated,
            ),
        })
    }

    /// The schema `boost` of `field` when L3 may prefix-expand its terms (SPEC §7.6, §6.3.1): a `text` field whose analyzer,
    /// as the memtable indexes with it, does not stack tokens. `None` for any other field.
    fn expands(&self, field: FieldId) -> Option<f64> {
        match self.collection().schema().field_by_id(field)?.kind() {
            FieldKind::Text(o) if !self.memtable().stacks_tokens(field) => Some(o.boost),
            _ => None,
        }
    }

    /// [`Self::search_step`], within the budget's `end` once there is one. `None` when the budget passed while the
    /// step ran: the step is dropped whole, as a page cut short would rank hits a full step does not return. The
    /// request's own deadline or token stays an error.
    fn budgeted_step(
        &self,
        step: &Query,
        earlier: &[Cow<'_, Query>],
        stats: &SearchStatistics<'_>,
        part: &Part<'_>,
        exec: &Execution,
        budget: Option<Instant>,
    ) -> Result<Option<(SearchPlan, Collected)>, RelaxedSearchError> {
        let Some(end) = budget else {
            return self.search_step(step, earlier, stats, part, exec).map(Some);
        };
        match self.search_step(step, earlier, stats, part, &exec.within_budget(end)) {
            Ok(searched) => Ok(Some(searched)),
            Err(e) if is_deadline(&e) && exec.interrupt().is_ok() => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// Plans `step` with the `earlier` steps excluded ([`plan_step`]) and collects `part` of it under `exec`.
    fn search_step(
        &self,
        step: &Query,
        earlier: &[Cow<'_, Query>],
        stats: &SearchStatistics<'_>,
        part: &Part<'_>,
        exec: &Execution,
    ) -> Result<(SearchPlan, Collected), RelaxedSearchError> {
        let terms = exec.terms(self);
        let plan = plan_step(step, earlier, self.collection().schema(), stats, &terms)?;
        let collected = self.collect(&plan, part, exec)?;
        Ok((plan, collected))
    }
    /// The page of the steps `ran`: their hits in step order from `request.offset` (counted from the cursor's step
    /// `first`), at most `request.limit`, each explained by its step's plan when asked, and the facets of every step
    /// summed. `last` is the cascade's last step, for the hits' cursors.
    fn relaxed_page(
        &self,
        ran: Vec<Ran>,
        request: &SearchRequest,
        first: usize,
        last: usize,
        exec: &Execution,
    ) -> Result<(Vec<RelaxedHit>, Vec<FacetCounts>), RequestError> {
        // Reading the page's documents is the last stage a deadline stops.
        exec.interrupt()?;
        let segments: Vec<_> = self.segments().collect();
        let mut facets: Vec<FacetCounter> = request
            .facets
            .iter()
            .copied()
            .map(FacetCounter::new)
            .collect();
        let mut skip = request.offset;
        let mut room = request.limit;
        let mut hits = Vec::with_capacity(room);
        for (
            step,
            Ran {
                level, searched, ..
            },
        ) in ran.into_iter().enumerate()
        {
            let Some((plan, collected)) = searched else {
                continue;
            };
            for (facet, counted) in facets.iter_mut().zip(collected.facets) {
                facet.merge(counted);
            }
            // A step before the cursor's was searched only to count its facets: its hits rank before the cursor.
            if step < first {
                continue;
            }
            let skipped = skip.min(collected.ranked.len());
            skip -= skipped;
            let page: Vec<Ranked> = collected
                .ranked
                .into_iter()
                .skip(skipped)
                .take(room)
                .collect();
            room -= page.len();
            if page.is_empty() {
                continue;
            }
            let explanations = if request.explain {
                self.explain_page(&plan, &segments, &page, collected.kept, exec)?
            } else {
                Vec::new()
            };
            let page = self.page_hits(
                &segments,
                page,
                explanations,
                &request.projection,
                plan.proximity(),
            )?;
            // Step 0 of a cascade that stopped at L0 is what a cursor without a step means.
            let at = (step, last) != (0, 0);
            hits.extend(page.into_iter().map(|mut hit| {
                hit.relaxed = at.then_some(StepCursor { step, last });
                RelaxedHit {
                    rank_score: rank_score(step, hit.score),
                    hit,
                    level,
                    step,
                }
            }));
        }
        Ok((hits, facets.into_iter().map(FacetCounter::finish).collect()))
    }
}

/// A step as a level built it: its query, what it applied, its text, and the synonym alternatives it may drop to fit.
type BuiltStep<'a> = (Cow<'a, Query>, String, Option<String>, Option<Expansion>);

/// The steps `planned` builds for `input`: its query, what it applied and its text. L0 (`first`) searches the query
/// itself, not a copy.
///
/// # Errors
/// A statistic the level cannot read (L2 reads its terms' `df`); [`SearchError::Interrupted`] once the request's deadline
/// has passed or it was cancelled.
fn level_steps<'a>(
    planned: &PlannedLevel,
    first: bool,
    input: LevelInput<'a>,
    exec: &Execution,
) -> Result<Vec<BuiltStep<'a>>, SearchError> {
    if first {
        return Ok(vec![(
            Cow::Borrowed(input.original),
            String::new(),
            input.compiled.map(Compiled::effective),
            None,
        )]);
    }
    // A level may read statistics (L2 its terms' `df`): the request's deadline and cancellation stop it first (SPEC §7.3).
    exec.interrupt().map_err(SearchError::Interrupted)?;
    Ok(planned
        .transform
        .steps(&input)?
        .into_iter()
        .map(|s| (Cow::Owned(s.query), s.applied, s.effective, s.alternatives))
        .collect())
}

/// `step` held to the limits beside the steps it excludes (SPEC §7.1.3): as built, with its expansions cut or fewer of
/// its synonym `alternatives`, or with alternatives the compilation already left out (`label` gets [`Relaxation::CUT`]), or `None` when no cut fits (`label` gets
/// [`Relaxation::SKIPPED`]).
///
/// # Errors
/// [`SearchError::Query`] for a step that is invalid for a reason other than a limit.
fn held<'a>(
    fitting: &StepLimits<'_>,
    step: Cow<'a, Query>,
    label: String,
    alternatives: Option<&Expansion>,
) -> Result<Held<'a>, SearchError> {
    let (step, label, cut) = match fitting.fit(&step)? {
        // The compilation left out alternatives that larger limits would hold (SPEC §6.3.2); the step is as built.
        Fit::Fits if alternatives.is_some_and(Expansion::cut) => {
            (Some(step), label + Relaxation::CUT, false)
        }
        Fit::Fits => (Some(step), label, false),
        Fit::Cut(cut) => (Some(Cow::Owned(cut)), label + Relaxation::CUT, true),
        Fit::Skipped => match alternatives
            .and_then(|a| fitting.cut_alternatives(&a.counts(), |keep| a.query(keep)))
        {
            Some(cut) => (Some(Cow::Owned(cut)), label + Relaxation::CUT, true),
            None => (None, label + Relaxation::SKIPPED, false),
        },
    };
    Ok(Held { step, label, cut })
}

/// A step held to the limits ([`held`]).
struct Held<'a> {
    /// The query to search: as built, or cut. `None` when it is skipped.
    step: Option<Cow<'a, Query>>,
    /// What it applied, with [`Relaxation::CUT`] or [`Relaxation::SKIPPED`] when the limits cut or skipped it.
    label: String,
    /// Whether `step` matches less than the level built (fewer expansions or alternatives): a nested level's step
    /// then stands for none of its level's earlier steps (ROADMAP 8.2.2.4).
    cut: bool,
}

/// Whether `e` is a deadline that passed, while planning or searching.
fn is_deadline(e: &RelaxedSearchError) -> bool {
    matches!(
        e,
        RelaxedSearchError::Plan(SearchError::Interrupted(Interrupt::DeadlineExceeded))
            | RelaxedSearchError::Request(RequestError::Interrupted(Interrupt::DeadlineExceeded))
    )
}

/// The plan of a step's `query` with the `earlier` steps' queries excluded (SPEC §7.6): a document they match is
/// theirs. L0, with none earlier, is planned as it is (no copy). A `bool` with one `must` clause and only `must_not` beside it
/// scores that clause's score exactly (`0.0 + s`), so the step ranks as its query alone.
fn plan_step(
    query: &Query,
    earlier: &[Cow<'_, Query>],
    schema: &CollectionSchema,
    stats: &SearchStatistics<'_>,
    terms: &ExecTerms<'_>,
) -> Result<SearchPlan, SearchError> {
    if earlier.is_empty() {
        return SearchPlan::with_terms(query, schema, stats, terms);
    }
    let excluded = Query::Bool(BoolQuery {
        must: vec![query.clone()],
        must_not: earlier.iter().map(|q| q.as_ref().clone()).collect(),
        ..BoolQuery::default()
    });
    SearchPlan::with_terms(&excluded, schema, stats, terms)
}

#[cfg(test)]
mod tests;
