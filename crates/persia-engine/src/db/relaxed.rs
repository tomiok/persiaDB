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
//! looser one, whatever their scores. A hit's cursor records its step, the last step of its page's cascade, and its
//! hit step, so the
//! next page builds the same steps, searches the cursor's step after the cursor and the later ones from their top,
//! and the pages are slices of one ranking.

use std::borrow::Cow;
use std::time::Instant;

use super::exec::{ExecTerms, Execution};
use super::search::{Collected, Cursor, Part, Ranked, SearchHit, SearchRequest, StepCursor};
use super::views::CollectionViews;
use crate::query::{BoolQuery, Compiled, Filter, Fit, Query, QueryLimits, StepLimits};
pub use crate::relax::Correction;
use crate::relax::{
    Expansion, FallbackOrder, LevelInput, PlannedLevel, RelaxLevel, RelaxPlan, RelaxSettings,
    rank_score,
};
use crate::schema::{CollectionSchema, FieldId, FieldKind};
use crate::search::{
    Explanation, FacetCounter, FacetCounts, Interrupt, RelaxedExplanation, RequestError,
    SearchError, SearchPlan,
};
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
    /// [`SearchHit::cursor`] of [`Self::hit`], which records the hit's step, its cascade's last step, and its hit step.
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
/// // A programmatic query has no user text, so no `original`, `effective` or `corrected`.
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
    /// The text of the *hit step*'s query (`"effective"`), as it reads after relaxing: `"zapatillas rojas"`. The hit step
    /// is the last step that found a hit, L0 when none did (SPEC §7.6, ROADMAP 8.1.22): a last step whose matches an
    /// earlier step all took does not name what no hit was found by. Without `fill`, every page of a query states
    /// the same, as a cursor records it ([`StepCursor::hit`](super::StepCursor::hit)); with `fill`, a deeper page may
    /// build a later step that finds a hit, and state its text. Rendered from its clauses' source text
    /// ([`crate::query::CompiledClause::span`]), so at L0 it is [`Compiled::effective`]: the clauses that compiled,
    /// without stopwords or text past a limit. `None` when [`Self::original`] is, and when the hit step's level
    /// renders no text for its query. At L2 `drop_terms` it is the clauses the step keeps: its count of the most
    /// important terms and every excluded clause, in text order (`"purple -blue"`).
    pub effective: Option<String>,
    /// The words the fuzzy level replaced, in query order (`"corrected"`): when the hit step is L4's, each required
    /// word no document holds and the term it searched instead ("did you mean", ROADMAP 8.2.4.3), which
    /// [`Self::effective`] reads in its place. Empty at any other level, and for a query not compiled from `q`.
    pub corrected: Vec<Correction>,
    /// The budget (`budget_ms`) passed before the cascade finished (SPEC §7.6): the hits are those of the steps
    /// completed before it, and `level` and `applied` describe those steps.
    pub truncated: bool,
    /// The explicit flag that L5 `fallback` found the hits (`"fallback"`, SPEC §7.6, ROADMAP 8.2.5): the order it
    /// ranked them in when L5 is the hit step, `None` otherwise. Those hits are the collection's ranking of the
    /// documents the filters keep and no stricter step matched, not matches of the query's words. Every page of a
    /// query states the same, as [`Self::effective`] does, also a page holding none of L5's hits: each hit's
    /// [`RelaxedHit::level`] says which are.
    pub fallback: Option<FallbackOrder>,
    /// The explicit flag that the filter step found hits (`"filters_dropped"`, SPEC §7.6, ROADMAP 8.2.6, ADR-0047): its
    /// step is the hit step. Only `filters: "last_resort"` plans it, as the cascade's last step, so some hit of the
    /// ranking matches L0's words without passing the request's filters. Every page of a query states the same, as
    /// [`Self::effective`] does; each hit's [`RelaxedHit::level`] ([`RelaxLevel::Filters`]) says which. With it,
    /// [`Self::fallback`] is `None`.
    pub filters_dropped: bool,
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
    /// for a query `compiled` from text (`None` for a programmatic one) whose hit step reads `effective` and
    /// `corrected` those words.
    fn of(
        (level, relaxed): (RelaxLevel, bool),
        applied: Vec<String>,
        compiled: Option<&Compiled>,
        (effective, corrected): StepText,
        truncated: bool,
    ) -> Self {
        // A query without user text has no text form, whatever a level states.
        let Some(compiled) = compiled else {
            return Self {
                level,
                applied,
                original: None,
                effective: None,
                corrected: Vec::new(),
                fallback: None,
                filters_dropped: false,
                truncated,
                relaxed,
            };
        };
        Self {
            level,
            applied,
            original: Some(compiled.text.clone()),
            effective,
            corrected,
            fallback: None,
            filters_dropped: false,
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

/// The query a relaxed search starts from ([`CollectionViews::search_relaxed`]): a [`Query`] built in code, or one
/// [`Compiled`] from user text, whose text the response states (`original`, `effective`, SPEC §7.6).
#[derive(Debug, Clone, Copy)]
pub struct RelaxedQuery<'a> {
    query: &'a Query,
    compiled: Option<&'a Compiled>,
    /// Root filters a caller marked soft ([`Self::with_soft_filter`]).
    soft: &'a [Filter],
}

impl<'a> RelaxedQuery<'a> {
    /// This query with `soft` marking the filters of its root `bool` that the filter step under
    /// `filters: "last_resort"` may drop (SPEC §7.6, ROADMAP 8.2.13, ADR-0056): a root filter equal to one of them is
    /// soft, and every other one hard. A programmatic query's root filters are hard unless marked, so without marks it
    /// builds no filter step. A compiled query's soft filters are its request's `soft_filter`
    /// ([`Compiled::with_soft_filter`]) and nothing else (SPEC §7.6): marks are ignored, so its request's `filter`
    /// ([`Compiled::filter`]) and the filters the root of a query a caller put in holds itself
    /// ([`Compiled::with_query`]) stay hard even if marked. Marking a filter the root does not hold changes nothing:
    /// the query is searched as it is.
    ///
    /// ```
    /// use persia_engine::db::RelaxedQuery;
    /// use persia_engine::query::{BoolQuery, Filter, FilterValue, Query, TermQuery};
    /// use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
    ///
    /// let schema = SchemaBuilder::new("c")
    ///     .field("body", FieldKind::Text(TextOptions::default()))
    ///     .field("tenant", FieldKind::Keyword(KeywordOptions::default()))
    ///     .field("brand", FieldKind::Keyword(KeywordOptions::default()))
    ///     .build()?;
    /// let id = |name: &str| schema.field(name).map(|f| f.id()).ok_or("no field");
    /// let keyword = |field, value: &str| Filter::Term { field, value: FilterValue::Keyword(value.into()) };
    /// let (tenant, nike) = (keyword(id("tenant")?, "t1"), keyword(id("brand")?, "nike"));
    /// let shoes = Query::Term(TermQuery { field: id("body")?, term: "shoes".into() });
    /// let query = Query::Bool(BoolQuery { must: vec![shoes], filter: vec![tenant, nike.clone()], ..BoolQuery::default() });
    /// // The filter step may drop `brand: nike`, never `tenant: t1`.
    /// let soft = [nike];
    /// let _relaxed = RelaxedQuery::from(&query).with_soft_filter(&soft);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn with_soft_filter(self, soft: &'a [Filter]) -> Self {
        Self { soft, ..self }
    }
}

impl<'a> From<&'a Query> for RelaxedQuery<'a> {
    fn from(query: &'a Query) -> Self {
        Self {
            query,
            compiled: None,
            soft: &[],
        }
    }
}

impl<'a> From<&'a Compiled> for RelaxedQuery<'a> {
    fn from(compiled: &'a Compiled) -> Self {
        Self {
            query: compiled.query(),
            compiled: Some(compiled),
            soft: &[],
        }
    }
}

/// What a relaxed search returns.
#[derive(Debug, Clone, PartialEq)]
pub struct RelaxedResults {
    /// The page, best first: the hits of each step in step order.
    pub hits: Vec<RelaxedHit>,
    /// One per requested facet, in request order, over the matches of every step a page holding every hit builds (SPEC
    /// §7.5, ROADMAP 8.1.9): the same on every page of the query, but on a page the budget truncated, or one where it
    /// passed in a step searched only to count it (the page is then whole, its counts miss that step and later ones).
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

/// A step to search: its query, the earlier steps it excludes, and whether it is L5's, which ranks its own hits.
#[derive(Clone, Copy)]
struct StepSearch<'a, 'q> {
    step: &'a Query,
    earlier: &'a [Cow<'q, Query>],
    fallback: bool,
}

/// What a step's search found.
struct Searched {
    /// The step's plan, with its earlier steps excluded: it explains the step's hits.
    plan: SearchPlan,
    collected: Collected,
    /// L5's explanations of its hits, in rank order (empty unless the page is explained): L5 ranks its hits by static
    /// rank and recency, which its plan does not score. `None` for any other step.
    fallback: Option<Vec<Option<Explanation>>>,
}

/// A step the cascade built, and what its search found when it ran one.
struct Ran {
    level: RelaxLevel,
    searched: Option<Searched>,
    /// Its text and corrections (SPEC §7.6 `effective`, `corrected`), which the response states when it is the hit step.
    text: StepText,
    /// Not searched: it does not fit the limits (SPEC §7.1.3), and later steps do not exclude it.
    skipped: bool,
    /// Built for the page. `false` for a step a `fill` page with facets builds past its own only to count it (SPEC
    /// §7.5, ROADMAP 8.1.9): it collects no hit, is not listed in `applied` or `level`, and is not the cascade's last
    /// step a cursor records.
    page: bool,
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

    /// The level of the last step built and not skipped, and whether it is after L0: L0's when there is none.
    fn last_level(&self) -> (RelaxLevel, bool) {
        self.ran
            .iter()
            .enumerate()
            .rfind(|(_, r)| !r.skipped && r.page)
            .map_or((RelaxLevel::Exact, false), |(i, r)| (r.level, i > 0))
    }

    /// The cascade's hit step (SPEC §7.6, ROADMAP 8.1.22): the later of the last step whose search collected a hit
    /// (`found`) and the `carried` one a cursor recorded (its page may not search that step again), each `None` when
    /// there is none.
    fn hit_steps(&self, carried: Option<usize>) -> (Option<usize>, Option<usize>) {
        let found = self.ran.iter().rposition(|r| {
            r.searched
                .as_ref()
                .is_some_and(|s| !s.collected.ranked.is_empty())
        });
        (found, carried)
    }

    /// The steps built, what they applied, the hit step's text and where the page is in the cascade, from `start`.
    /// The text is the later of the carried hit step, when this page built it (not when the budget truncated the page
    /// before it, or another plan no longer builds it), and the last step that found a hit here (later with `fill`),
    /// else L0's.
    fn finish(self, start: Start) -> Finished {
        let (found, carried) = self.hit_steps(start.hit);
        let Self {
            mut ran, applied, ..
        } = self;
        let built = carried.filter(|&h| h < ran.len());
        let text = built.max(found).unwrap_or(0);
        let (text, hit_level) = ran.get_mut(text).map_or_else(
            || ((None, Vec::new()), RelaxLevel::Exact),
            |r| (core::mem::take(&mut r.text), r.level),
        );
        // The cursor's page built at least its last step; with fewer steps now (another plan), the page ends sooner.
        let last = ran
            .iter()
            .rposition(|r| r.page)
            .unwrap_or(0)
            .max(start.last.unwrap_or(0));
        let at = PageSteps {
            first: start.step,
            last,
            // The next page carries the hit step on, as the first page found it.
            hit: found.max(carried).unwrap_or(0),
            relax: start.relax,
        };
        Finished {
            ran,
            applied,
            text,
            at,
            hit_level,
        }
    }

    /// Whether the cascade built L2's last rung and did not skip it (ROADMAP 8.2.7).
    fn drops_terms(&self) -> bool {
        self.ran
            .iter()
            .rfind(|r| r.level == RelaxLevel::DropTerms)
            .is_some_and(|r| !r.skipped)
    }

    /// The steps pushed from now on are the next level's.
    fn begin_level(&mut self) {
        self.level_start = self.earlier.len();
    }

    /// A step of `level` that does not fit: listed (when it is the `page`'s), not searched, excluded from none.
    fn skip(&mut self, level: RelaxLevel, label: String, page: bool) {
        if page {
            self.applied.push(label);
        }
        self.ran.push(Ran {
            level,
            searched: None,
            text: (None, Vec::new()),
            skipped: true,
            page,
        });
    }

    /// A step of `level` built (and `searched`, unless no hit or facet was wanted of it), for the `page` or only to count
    /// its facets: excluded from every later one.
    /// One that `covers` its level's earlier steps (a nested level's step as built, ROADMAP 8.2.2.4) or every earlier
    /// step (L5's, ADR-0047) matches every document they match, so it replaces them in what the later steps exclude.
    fn push(
        &mut self,
        level: RelaxLevel,
        step: Cow<'q, Query>,
        label: String,
        text: StepText,
        searched: Option<Searched>,
        (covers, page): (Covers, bool),
    ) {
        let first = self.ran.is_empty();
        if !first && page {
            self.applied.push(label);
        }
        self.ran.push(Ran {
            level,
            searched,
            text,
            skipped: false,
            page,
        });
        let from = match covers {
            Covers::Itself => None,
            Covers::Level => Some(self.level_start),
            Covers::Earlier => Some(0),
        };
        let replaced = from.is_some_and(|from| self.earlier.len() > from);
        if let Some(from) = from.filter(|_| replaced) {
            self.earlier.truncate(from);
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
    }
}

/// Which earlier steps a step built uncut matches every document of, so later steps need not exclude them beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Covers {
    /// None but itself: every earlier step stays excluded.
    Itself,
    /// Its level's earlier steps (a nested level, ROADMAP 8.2.2.4).
    Level,
    /// Every earlier step (L5, whose `match_all` keeps L0's root exclusions and filters, ADR-0047).
    Earlier,
}

impl Covers {
    /// What a step of `planned` covers under `relax`, `cut` to fit the limits or not: a cut step matches less than its
    /// level built, so it covers no earlier step (SPEC §7.1.3).
    fn of(relax: &RelaxPlan, planned: &PlannedLevel, cut: bool) -> Self {
        if cut || !relax.excludes_latest_only() {
            Self::Itself
        } else if planned.transform.covers_earlier() {
            Self::Earlier
        } else if planned.transform.nested() {
            Self::Level
        } else {
            Self::Itself
        }
    }
}

/// What decides whether the cascade reaches a step, beside its level ([`Start::builds`]): the plan, `min_results` and
/// the hits needed from the top.
#[derive(Clone, Copy)]
struct Reach<'a> {
    relax: &'a RelaxPlan,
    min_results: usize,
    need: usize,
}

impl<'a> Reach<'a> {
    /// For `relax` from `start`, with the request's `window` (`offset + limit`).
    fn new(relax: &'a RelaxPlan, start: Start, window: usize) -> Self {
        let min_results = usize::try_from(relax.min_results()).unwrap_or(usize::MAX);
        Self {
            relax,
            min_results,
            need: start.need(window, min_results),
        }
    }
}

/// Where the cascade starts and how far it must go: from a request's cursor, or from the top.
#[derive(Debug, Clone, Copy)]
struct Start {
    /// The step whose hits the page starts in, after the cursor.
    step: usize,
    /// The cascade builds at least the steps through this one (the cursor's page did).
    last: Option<usize>,
    /// The hit step the cursor's page stated (ROADMAP 8.1.22).
    hit: Option<usize>,
    /// The settings of the plan, which every hit's cursor records (ROADMAP 8.3.1).
    relax: Option<RelaxSettings>,
}

impl Start {
    /// From the top without a cursor; from `cursor`'s step otherwise, one without a step being step 0 of a cascade
    /// that stopped at L0. The request was checked first ([`CollectionViews::check_request`]): a cursor from a step
    /// after L0 has the settings of a plan that relaxes, as `relax` then does.
    fn of(cursor: Option<&Cursor>, relax: &RelaxPlan) -> Self {
        let relax = relax.settings();
        let Some(cursor) = cursor else {
            return Self {
                step: 0,
                last: None,
                hit: None,
                relax,
            };
        };
        let at = cursor.relaxed.unwrap_or(StepCursor {
            step: 0,
            last: 0,
            hit: 0,
        });
        // A plan of L0 alone stops at L0 whatever a hand-built cursor says of its cascade.
        let (last, hit) = if relax.is_some() {
            (at.last, at.hit)
        } else {
            (0, 0)
        };
        Self {
            step: at.step,
            last: Some(last),
            hit: Some(hit),
            relax,
        }
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

    /// Whether the cascade builds step `i`, with `found` hits found before it, of a level that `fills` the page or not
    /// (the filter step only reaches `min_results`, ADR-0047), `need` from [`Self::need`].
    fn builds(self, i: usize, found: usize, fills: bool, reach: Reach<'_>) -> bool {
        let Reach {
            relax,
            min_results,
            need,
        } = reach;
        let fill = fills && relax.fill() && found < need;
        i == 0
            || relax.relaxes()
                && match self.last {
                    None => found < min_results || fill,
                    Some(last) => i <= last || fill,
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
    /// is full or the last step is passed (with `fill`, until the page is full). A cursor without a step is step 0 of a
    /// cascade that stopped at L0. A cursor pages only under the relaxation settings that made it
    /// ([`RelaxPlan::settings`]): a plain search's ([`SearchHit::cursor`]) pages a plan of L0 alone (`relax: false`).
    /// An explained hit has a root [`Explanation::Relaxed`] naming its step (ROADMAP 8.3.4).
    ///
    /// Facets count every step a page holding every hit builds (SPEC §7.5, ROADMAP 8.1.9): with `fill`, a page that has
    /// its hits builds the remaining steps of the levels that fill and searches them only to count them, without
    /// listing them in [`Relaxation::applied`] or recording them in its cursors.
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
    /// after L0 when `relax` does not relax; [`RequestError::CursorRelax`] for a cursor from a search with other
    /// relaxation settings ([`RelaxPlan::settings`]);
    /// [`RelaxedSearchError::Plan`] when a step cannot be planned, or a level cannot read the statistics it builds its steps
    /// from (L2 reads its terms' `df`, L4 walks the term dictionaries of the words it corrects).
    pub fn search_relaxed<'q>(
        &self,
        query: impl Into<RelaxedQuery<'q>>,
        request: &SearchRequest,
        relax: &RelaxPlan,
        exec: &Execution,
    ) -> Result<RelaxedResults, RelaxedSearchError> {
        let RelaxedQuery {
            query,
            compiled,
            soft,
        } = query.into();
        let soft = soft_filters(compiled, soft);
        // Every step is planned as `SearchPlan::with_terms` plans by default: without proximity (SPEC §7.4).
        let window = self.check_request(request, false, relax.settings())?;
        let collection = self.collection();
        let stats = collection.search_statistics(self.memtable());
        let start = Start::of(request.search_after.as_ref(), relax);
        let reach = Reach::new(relax, start, window);
        let builds = |i: usize, found: usize, fills: bool| start.builds(i, found, fills, reach);
        let limits = step_limits(relax, compiled);
        // Counts a page of this query kept (ROADMAP 8.1.11): read, and no step is searched only to count it.
        let (key, cached) = self.kept_facets(request, relax, (query, compiled, &soft), limits);
        let counting = !request.facets.is_empty() && cached.is_none();
        let expands = |field: FieldId| self.expands(field);
        let mut built = Built::new(collection.schema(), limits);
        let mut found = 0usize;
        // The end of the budget, read from when L0 finishes (SPEC §7.6): L0 itself is never cut by it.
        let mut budget: Option<Instant> = None;
        let clock = exec.relax_clock();
        // Whether the budget passed in a step of the page (SPEC §7.6 `truncated`), or in one searched only to count its
        // facets: then the page is whole but its counts are not (ROADMAP 8.1.9), and no later page reads them.
        let mut passed = Passed::default();
        'levels: for planned in relax.planned() {
            let fills = planned.transform.fills();
            // A page holding every hit builds every step of the levels that fill (SPEC §7.5, ROADMAP 8.1.9): with `fill`
            // and facets, a page counts those past its own too, only to count them.
            let counts_past = counting && relax.fill() && fills;
            // Checked before a level builds its steps: one the cascade does not reach builds nothing.
            let page = builds(built.ran.len(), found, fills);
            if !page && !counts_past {
                break;
            }
            if budget.is_some_and(|end| clock.now() >= end) {
                passed.mark(page);
                break;
            }
            let first = built.ran.is_empty().then_some(query);
            // L3 builds its tier step only when L2's last rung (one term) was not built, or was skipped: that rung takes every
            // match of the tier step (ROADMAP 8.2.7). The same snapshot, query and limits build the same steps on every page.
            let input = LevelInput::new(query, compiled, &stats)
                .expanding(&expands)
                .dropping(built.drops_terms())
                .softening(&soft);
            let Some(steps) = self.budgeted_level(planned, first, input, exec, budget)? else {
                passed.mark(page);
                break;
            };
            built.begin_level();
            for (step, label, text, alternatives) in steps {
                let i = built.ran.len();
                let page = builds(i, found, fills);
                if !page && !counts_past {
                    break 'levels;
                }
                if budget.is_some_and(|end| clock.now() >= end) {
                    passed.mark(page);
                    break 'levels;
                }
                let Held { step, label, cut } =
                    built.fit(query, step, label, alternatives.as_ref())?;
                let Some(step) = step else {
                    built.skip(planned.level, label, page);
                    continue;
                };
                // Hits this step is to collect: none before the cursor's step, or once the page is full (a step built only
                // to count its facets collects none).
                let wanted = if page && i >= start.step {
                    reach.need.saturating_sub(found)
                } else {
                    0
                };
                let searched = if wanted > 0 || counting {
                    let part = part(request, i == start.step, wanted, counting);
                    let Some(searched) = self.budgeted_step(
                        StepSearch {
                            step: &step,
                            earlier: &built.earlier,
                            fallback: planned.level == RelaxLevel::Fallback,
                        },
                        &stats,
                        &part,
                        exec,
                        budget,
                    )?
                    else {
                        passed.mark(page);
                        break 'levels;
                    };
                    found = found.saturating_add(searched.collected.ranked.len());
                    Some(searched)
                } else {
                    None
                };
                // A cut step matches less than its level built (SPEC §7.1.3): it stands for no earlier step, which an
                // uncut later one does again (a cut step's matches are its uncut ones', which the later step holds).
                let covers = Covers::of(relax, planned, cut);
                built.push(planned.level, step, label, text, searched, (covers, page));
                if budget.is_none() {
                    budget = clock.now().checked_add(relax.budget());
                }
            }
        }
        let facets = Facets {
            key: key.filter(|_| start.last.is_none() && !passed.page && !passed.counts),
            cached,
        };
        self.respond(built, start, request, exec, (compiled, passed.page, facets))
    }

    /// The response of the cascade `built` from `start`: its page of `request` and what it did, for a query
    /// `compiled` from text (or not) and `truncated` by the budget (or not). Its facets are those `facets` read, else
    /// those its steps counted, which the snapshot keeps for later pages when `facets` has a key: a page from the top
    /// that counted every step it builds, without the budget passing (ROADMAP 8.1.11).
    fn respond(
        &self,
        built: Built<'_, '_>,
        start: Start,
        request: &SearchRequest,
        exec: &Execution,
        (compiled, truncated, kept): (Option<&Compiled>, bool, Facets<'_>),
    ) -> Result<RelaxedResults, RelaxedSearchError> {
        let level = built.last_level();
        let Finished {
            ran,
            applied,
            text,
            at,
            hit_level,
        } = built.finish(start);
        let (hits, counted) = self.relaxed_page(ran, &applied, request, at, exec)?;
        let facets = match kept {
            Facets {
                cached: Some(cached),
                ..
            } => cached,
            Facets { key, cached: None } => {
                if let Some(key) = key {
                    // Copied and sized before the cache is locked.
                    let kept = facet_cache::Kept::new(key.to_key(), counted.clone());
                    self.facet_cache().insert(kept);
                }
                counted
            }
        };
        let mut relaxation = Relaxation::of(level, applied, compiled, text, truncated);
        // The explicit flag (SPEC §7.6): L5 found the hits the response states.
        relaxation.fallback = (hit_level == RelaxLevel::Fallback)
            .then(|| FallbackOrder::of(self.collection().schema().ranking()));
        relaxation.filters_dropped = hit_level == RelaxLevel::Filters;
        Ok(RelaxedResults {
            hits,
            facets,
            relaxation,
        })
    }

    /// The key of `request`'s facets for `query` (its compilation and soft filters) under `relax` and `limits`, and the
    /// counts the snapshot kept for it (ROADMAP 8.1.11, SPEC §7.5). No key without facets, or for a plan whose levels
    /// are not this version's. The key borrows the query: it is copied only to keep new counts.
    fn kept_facets<'k>(
        &self,
        request: &'k SearchRequest,
        relax: &RelaxPlan,
        (query, compiled, soft): (&'k Query, Option<&'k Compiled>, &'k [Filter]),
        limits: QueryLimits,
    ) -> (Option<facet_cache::KeyRef<'k>>, Option<Vec<FacetCounts>>) {
        if request.facets.is_empty() || !relax.cacheable() {
            return (None, None);
        }
        let key = facet_cache::KeyRef {
            version: self.memtable().version(),
            query,
            compiled,
            soft,
            settings: relax.settings(),
            limits,
            facets: &request.facets,
        };
        let cached = self.facet_cache().get(&key);
        (Some(key), cached)
    }

    /// The snapshot's cache of facet counts (ROADMAP 8.1.11).
    fn facet_cache(&self) -> std::sync::MutexGuard<'_, FacetCache> {
        // A panic while it was held leaves whole entries: each is replaced or read as one.
        self.collection()
            .facets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The schema `boost` of `field` when L3 may prefix-expand its terms (SPEC §7.6, §6.3.1): a `text` field whose analyzer,
    /// as the memtable indexes with it, does not stack tokens. `None` for any other field.
    fn expands(&self, field: FieldId) -> Option<f64> {
        match self.collection().schema().field_by_id(field)?.kind() {
            FieldKind::Text(o) if !self.memtable().stacks_tokens(field) => Some(o.boost),
            _ => None,
        }
    }

    /// The steps `planned` builds for `input` ([`level_steps`]), with the views' terms to walk (L4's corrections, SPEC
    /// §7.6) under the budget's `end` once there is one, as a step's planning walks them. `None` when the budget passed
    /// while the level built them: the cascade stops there, as within a step. The request's own deadline or token stays
    /// an error.
    fn budgeted_level<'q>(
        &self,
        planned: &PlannedLevel,
        first: Option<&'q Query>,
        input: LevelInput<'_>,
        exec: &Execution,
        budget: Option<Instant>,
    ) -> Result<Option<Vec<BuiltStep<'q>>>, RelaxedSearchError> {
        let within = budget.map(|end| exec.within_budget(end));
        let terms = within.as_ref().unwrap_or(exec).terms(self);
        match level_steps(planned, first, input.walking(&terms), exec) {
            Ok(steps) => Ok(Some(steps)),
            Err(SearchError::Interrupted(Interrupt::DeadlineExceeded))
                if budget.is_some() && exec.interrupt().is_ok() =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// [`Self::search_step`], within the budget's `end` once there is one. `None` when the budget passed while the
    /// step ran: the step is dropped whole, as a page cut short would rank hits a full step does not return. The
    /// request's own deadline or token stays an error.
    fn budgeted_step(
        &self,
        step: StepSearch<'_, '_>,
        stats: &SearchStatistics<'_>,
        part: &Part<'_>,
        exec: &Execution,
        budget: Option<Instant>,
    ) -> Result<Option<Searched>, RelaxedSearchError> {
        let Some(end) = budget else {
            return self.search_step(step, stats, part, exec).map(Some);
        };
        match self.search_step(step, stats, part, &exec.within_budget(end)) {
            Ok(searched) => Ok(Some(searched)),
            Err(e) if is_deadline(&e) && exec.interrupt().is_ok() => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// Plans `step` with the `earlier` steps excluded ([`plan_step`]) and collects `part` of it under `exec`: by the
    /// request's sort, or by L5's own order for the fallback's step (SPEC §7.6, [`Self::collect_fallback`]).
    fn search_step(
        &self,
        StepSearch {
            step,
            earlier,
            fallback,
        }: StepSearch<'_, '_>,
        stats: &SearchStatistics<'_>,
        part: &Part<'_>,
        exec: &Execution,
    ) -> Result<Searched, RelaxedSearchError> {
        #[cfg(test)]
        SEARCHED.with(|n| n.set(n.get() + 1));
        let terms = exec.terms(self);
        let plan = plan_step(step, earlier, self.collection().schema(), stats, &terms)?;
        // `check_request` was given `false` for every step: a step planned with the bonus would record another
        // setting in its cursors than the one their next page is checked against.
        debug_assert!(
            !plan.proximity(),
            "a relaxation step planned with proximity"
        );
        if fallback {
            let found = self.collect_fallback(&plan, part, exec)?;
            return Ok(Searched {
                plan,
                collected: found.collected,
                fallback: Some(found.explanations),
            });
        }
        let collected = self.collect(&plan, part, exec)?;
        Ok(Searched {
            plan,
            collected,
            fallback: None,
        })
    }
    /// The page of the steps `ran`: their hits in step order from `request.offset` (counted from the cursor's step
    /// `at.first`), at most `request.limit`, each explained by its step's plan when asked, and the facets of every step
    /// summed. `at.last` is the cascade's last step and `at.hit` its hit step, for the hits' cursors. An explained hit
    /// states its level, step and the step's entry of `applied` (one per step after L0) around its step's tree (ROADMAP
    /// 8.3.4).
    fn relaxed_page(
        &self,
        ran: Vec<Ran>,
        applied: &[String],
        request: &SearchRequest,
        at: PageSteps,
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
            let Some(Searched {
                plan,
                collected,
                fallback,
            }) = searched
            else {
                continue;
            };
            for (facet, counted) in facets.iter_mut().zip(collected.facets) {
                facet.merge(counted);
            }
            // A step before the cursor's was searched only to count its facets: its hits rank before the cursor.
            if step < at.first {
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
            let explanations = match fallback {
                // L5 explained its hits as it ranked them, in rank order: the page's are from `skipped` on.
                Some(explained) => explained
                    .into_iter()
                    .skip(skipped)
                    .take(page.len())
                    .collect(),
                None if request.explain => {
                    self.explain_page(&plan, &segments, &page, collected.kept, exec)?
                }
                None => Vec::new(),
            };
            // Every step after L0 listed its entry, skipped ones included: step `i`'s is `applied[i - 1]`.
            let label = step.checked_sub(1).and_then(|i| applied.get(i)).cloned();
            let explanations = explanations
                .into_iter()
                .map(|e| {
                    e.map(|explanation| {
                        Explanation::Relaxed(RelaxedExplanation {
                            level,
                            step,
                            applied: label.clone(),
                            explanation: Box::new(explanation),
                        })
                    })
                })
                .collect();
            let page = self.page_hits(
                &segments,
                page,
                explanations,
                &request.projection,
                plan.proximity(),
            )?;
            // Step 0 of a cascade that stopped at L0 is what a cursor without a step means.
            let PageSteps {
                last,
                hit: found,
                relax,
                ..
            } = at;
            let relaxed = (step, last) != (0, 0);
            hits.extend(page.into_iter().map(|mut hit| {
                // Every hit's step found a hit, so the hit step is at least its own.
                hit.relaxed = relaxed.then_some(StepCursor {
                    step,
                    last,
                    hit: found.max(step),
                });
                hit.relax = relax;
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

/// What the cascade built, as the page and the response state it ([`Built::finish`]).
struct Finished {
    ran: Vec<Ran>,
    applied: Vec<String>,
    /// The hit step's text and corrections.
    text: StepText,
    at: PageSteps,
    /// The hit step's level (L0's when no step found a hit).
    hit_level: RelaxLevel,
}

/// Where a page is in its cascade: the cursor's step it starts in, the cascade's last step, and its hit step (SPEC §7.6).
#[derive(Debug, Clone, Copy)]
struct PageSteps {
    first: usize,
    last: usize,
    hit: usize,
    relax: Option<RelaxSettings>,
}

/// A search's facets as the snapshot keeps them (ROADMAP 8.1.11): the key to keep them under, `None` when the page's
/// counts are not to be kept, and the counts kept.
struct Facets<'a> {
    key: Option<facet_cache::KeyRef<'a>>,
    cached: Option<Vec<FacetCounts>>,
}

/// Where the budget passed (SPEC §7.6): in a step of the page, which truncates it, or in a step searched only to count
/// its facets (ROADMAP 8.1.9), which leaves the page whole and its counts partial.
#[derive(Debug, Default, Clone, Copy)]
struct Passed {
    /// In a step of the page, or before a level of the page's steps: the response's `truncated`.
    page: bool,
    /// In a step built only to count its facets: the counts miss it and the steps after it.
    counts: bool,
}

impl Passed {
    /// The budget passed at a step of the `page` (or one searched only to count it).
    fn mark(&mut self, page: bool) {
        if page {
            self.page = true;
        } else {
            self.counts = true;
        }
    }
}

/// A step's text (SPEC §7.6 `effective`) and the words it corrected (`corrected`).
type StepText = (Option<String>, Vec<Correction>);

/// A step as a level built it: its query, what it applied, its text and corrections, and the synonym alternatives it
/// may drop to fit.
type BuiltStep<'a> = (Cow<'a, Query>, String, StepText, Option<Expansion>);

/// The steps `planned` builds for `input`: its query, what it applied and its text. L0 (`first`, its query) searches
/// the query itself, not a copy.
///
/// # Errors
/// A statistic or a term walk the level cannot read (L2 reads its terms' `df`, L4 walks term dictionaries); [`SearchError::Interrupted`] once the request's deadline
/// has passed or it was cancelled.
fn level_steps<'q>(
    planned: &PlannedLevel,
    first: Option<&'q Query>,
    input: LevelInput<'_>,
    exec: &Execution,
) -> Result<Vec<BuiltStep<'q>>, SearchError> {
    if let Some(original) = first {
        return Ok(vec![(
            Cow::Borrowed(original),
            String::new(),
            (input.compiled.map(Compiled::effective), Vec::new()),
            None,
        )]);
    }
    // A level may read statistics (L2 its terms' `df`): the request's deadline and cancellation stop it first (SPEC §7.3).
    exec.interrupt().map_err(SearchError::Interrupted)?;
    Ok(planned
        .transform
        .steps(&input)?
        .into_iter()
        .map(|s| {
            (
                Cow::Owned(s.query),
                s.applied,
                (s.effective, s.corrected),
                s.alternatives,
            )
        })
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

/// The limits a plan's steps are held to (SPEC §7.1.3): the plan's, else those the query was compiled with, else the
/// defaults.
fn step_limits(relax: &RelaxPlan, compiled: Option<&Compiled>) -> QueryLimits {
    relax
        .limits()
        .copied()
        .or_else(|| compiled.map(|c| c.limits))
        .unwrap_or_default()
}

/// What a step collects of `request`: `wanted` hits by its sort, after its cursor when the step is the cursor's
/// (`at_cursor`), each explained when asked, and its facets when `counting`.
fn part(request: &SearchRequest, at_cursor: bool, wanted: usize, counting: bool) -> Part<'_> {
    Part {
        sort: &request.sort,
        after: request.search_after.as_ref().filter(|_| at_cursor),
        facets: if counting { &request.facets } else { &[] },
        explain: request.explain,
        window: wanted,
    }
}

/// The filters the filter step may drop (SPEC §7.6, ROADMAP 8.2.13): of a compiled query, its request's `soft_filter`
/// but those a caller's own root holds ([`Compiled::droppable`]), and nothing else, so a filter a caller `marked` is
/// ignored (its request's `filter` and the caller's own root filters are hard); of a programmatic query, those a
/// caller `marked`.
fn soft_filters(compiled: Option<&Compiled>, marked: &[Filter]) -> Vec<Filter> {
    match compiled {
        Some(compiled) => compiled.droppable().cloned().collect(),
        None => marked.to_vec(),
    }
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
thread_local! {
    /// How many steps this thread searched (tests: a page whose facet counts are kept searches none only to count it,
    /// ROADMAP 8.1.11).
    static SEARCHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many steps this thread searched so far.
#[cfg(test)]
pub(crate) fn searched() -> usize {
    SEARCHED.with(std::cell::Cell::get)
}

mod facet_cache;
mod fallback;

pub(super) use facet_cache::FacetCache;

#[cfg(test)]
mod tests;
