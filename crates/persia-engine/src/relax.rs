//! Zero-results avoidance (SPEC §7.6, ROADMAP M8): the relaxation cascade's plan.
//!
//! A [`RelaxPlan`] is the ordered list of levels a search may run, L0 (`exact`, the query as compiled) first, each
//! with the transform that builds its queries from the ones before it. The collection's `relaxation` settings
//! (SPEC §6.2.3, [`RelaxationConfig`]) give the defaults, and a query's `relax` ([`RelaxOptions`]) overrides them,
//! `relax: false` ([`RelaxOptions::disabled`]) leaving L0 alone.
//!
//! A level's transform runs only once the cascade reaches it, so a query that has `min_results` hits at L0 builds
//! nothing more. L0, L1 `synonyms` (ROADMAP 8.2.1), L2 `drop_terms` (ROADMAP 8.2.2), L3 `prefix` (ROADMAP 8.2.3), L4
//! `fuzzy` (ROADMAP 8.2.4) and L5 `fallback` (ROADMAP 8.2.5) build queries; L5's step only selects the documents the
//! cascade then ranks by static rank and recency ([`FallbackOrder`]). With `filters: "last_resort"` the plan ends with
//! [`RelaxLevel::Filters`] (ROADMAP 8.2.6): L0's query without its filters, only when every earlier step found too few.
//!
//! ```
//! use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
//! use persia_engine::schema::{Fallback, FilterRelaxation, Level, Levels, RelaxationConfig};
//!
//! // The collection's defaults: every level, then the static-rank fallback.
//! let collection = RelaxationConfig::default();
//! let plan = RelaxPlan::new(&collection, &RelaxOptions::default())?;
//! assert_eq!(plan.levels().collect::<Vec<_>>(), [
//!     RelaxLevel::Exact, RelaxLevel::Synonyms, RelaxLevel::DropTerms,
//!     RelaxLevel::Prefix, RelaxLevel::Fuzzy, RelaxLevel::Fallback,
//! ]);
//!
//! // A query asks for more hits, and only the fuzzy level, without the fallback.
//! let options = RelaxOptions {
//!     min_results: Some(5),
//!     levels: Some(Levels::Custom(vec![Level::Fuzzy])),
//!     fallback: Some(Fallback::None),
//!     ..RelaxOptions::default()
//! };
//! let plan = RelaxPlan::new(&collection, &options)?;
//! assert_eq!(plan.min_results(), 5);
//! assert_eq!(plan.levels().collect::<Vec<_>>(), [RelaxLevel::Exact, RelaxLevel::Fuzzy]);
//!
//! // `filters: "last_resort"` is the explicit opt-in to one last step without the filters.
//! let options = RelaxOptions { filters: Some(FilterRelaxation::LastResort), ..RelaxOptions::default() };
//! let plan = RelaxPlan::new(&collection, &options)?;
//! assert_eq!(plan.levels().last(), Some(RelaxLevel::Filters));
//!
//! // `relax: false` runs L0 only.
//! let plan = RelaxPlan::new(&collection, &RelaxOptions::disabled())?;
//! assert_eq!(plan.levels().collect::<Vec<_>>(), [RelaxLevel::Exact]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::query::{Compiled, Query, QueryLimits};
use crate::schema::{Fallback, FieldId, FilterRelaxation, Level, RankingConfig, RelaxationConfig};
use crate::search::TermSource;
use crate::segment::ReadError;
use crate::stats::StatisticsProvider;

mod drop_terms;
mod fallback;
mod filters;
mod fuzzy;
mod options;
mod prefix;
mod synonyms;
mod words;

pub use options::{RelaxError, RelaxOptions, RelaxSettings};
pub(crate) use synonyms::Expansion;

/// A level of the relaxation cascade (SPEC §7.6), ordered as the cascade runs them: L0 first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum RelaxLevel {
    /// L0: the query as compiled, every term required.
    Exact,
    /// L1: terms expanded with the collection's synonyms (ROADMAP 8.2.1).
    Synonyms,
    /// L2: fewer of the query's terms required (ROADMAP 8.2.2).
    DropTerms,
    /// L3: terms prefix-expanded (ROADMAP 8.2.3).
    Prefix,
    /// L4: terms matched within an edit distance (ROADMAP 8.2.4).
    Fuzzy,
    /// L5: the collection's fallback ranking (ROADMAP 8.2.5).
    Fallback,
    /// The last resort (ROADMAP 8.2.6, ADR-0047): L0's query without its filters, planned only with
    /// `filters: "last_resort"` and built only when every earlier step found fewer than `min_results` hits.
    Filters,
}

impl RelaxLevel {
    /// The level's name in responses (SPEC §7.6).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Synonyms => "synonyms",
            Self::DropTerms => "drop_terms",
            Self::Prefix => "prefix",
            Self::Fuzzy => "fuzzy",
            Self::Fallback => "fallback",
            Self::Filters => "filters",
        }
    }
}

impl From<Level> for RelaxLevel {
    fn from(level: Level) -> Self {
        match level {
            Level::Synonyms => Self::Synonyms,
            Level::DropTerms => Self::DropTerms,
            Level::Prefix => Self::Prefix,
            Level::Fuzzy => Self::Fuzzy,
        }
    }
}

/// Every level, in cascade order.
const LEVELS: [RelaxLevel; 7] = [
    RelaxLevel::Exact,
    RelaxLevel::Synonyms,
    RelaxLevel::DropTerms,
    RelaxLevel::Prefix,
    RelaxLevel::Fuzzy,
    RelaxLevel::Fallback,
    RelaxLevel::Filters,
];

impl fmt::Display for RelaxLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How L5 `fallback` ranked its hits (SPEC §7.6, ROADMAP 8.2.5): the response's `"fallback"`, set when the hits were
/// found by L5, so a client can tell they are the collection's ranking, not matches of the query's words.
///
/// ```
/// use persia_engine::relax::FallbackOrder;
/// use persia_engine::schema::{RankingConfig, StaticRank};
///
/// assert_eq!(FallbackOrder::of(&RankingConfig::default()), FallbackOrder::Recency);
/// let ranking = RankingConfig { static_rank: Some(StaticRank::new("sales")) };
/// assert_eq!(FallbackOrder::of(&ranking), FallbackOrder::StaticRank);
/// assert_eq!(FallbackOrder::StaticRank.name(), "static_rank");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FallbackOrder {
    /// By the collection's static rank (SPEC §6.2.3), the hits' score, then recency.
    StaticRank,
    /// By recency alone, most recent first: the collection has no static rank, or one of `weight` 0, so every
    /// score is 0. Recent by where the index holds a document (SPEC §7.6), not by write time.
    Recency,
}

impl FallbackOrder {
    /// The order L5 ranks by in a collection ranked by `ranking`.
    pub fn of(ranking: &RankingConfig) -> Self {
        match &ranking.static_rank {
            Some(rank) if rank.weight > 0.0 => Self::StaticRank,
            _ => Self::Recency,
        }
    }

    /// Its name in responses: `"static_rank"` or `"recency"`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::StaticRank => "static_rank",
            Self::Recency => "recency",
        }
    }
}

impl fmt::Display for FallbackOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a level's transform builds from: L0's query, the user text it was compiled from, and the collection's
/// statistics. Every level builds on L0's query, not on an earlier level's steps (SPEC §7.6: L4 `fuzzy` corrects
/// every term, not only those the last L2 step requires). The levels of ROADMAP 8.2 add what else they need.
#[derive(Clone, Copy)]
pub(crate) struct LevelInput<'a> {
    /// L0's query, as compiled.
    pub(crate) original: &'a Query,
    /// Its compilation from user text, with each clause's span (ROADMAP 8.1.7): `None` for a programmatic query.
    pub(crate) compiled: Option<&'a Compiled>,
    /// The statistics the search scores with (SPEC §7.2): L2 ranks its terms by their IDF.
    pub(crate) stats: &'a dyn StatisticsProvider,
    /// The schema `boost` of a field whose terms L3 may prefix-expand (SPEC §7.6), `None` for any other: a `text` field
    /// whose analyzer does not stack tokens (SPEC §6.3.1). None by default ([`Self::new`]); the search sets it
    /// ([`Self::expanding`]). A `prefix` scores with that boost as its field weight (SPEC §7.2), so L3 scales by a
    /// `fields` weight relative to it.
    pub(crate) expands: &'a dyn Fn(FieldId) -> Option<f64>,
    /// The terms of the views the search runs on, which L4 walks for its corrections (SPEC §7.6, ROADMAP 8.2.4.3), as
    /// planning walks them. None by default ([`Self::new`]): L4 then corrects nothing. The search sets them
    /// ([`Self::walking`]).
    pub(crate) terms: Option<&'a dyn TermSource>,
    /// Whether the cascade built L2's last rung (one term) before this level and did not skip it (ROADMAP 8.2.7): that
    /// rung takes every document that holds one of the query's terms as typed, so L3 builds no step requiring some of
    /// them exact (it would match nothing). True by default ([`Self::new`]); the search sets it from the steps it built
    /// ([`Self::dropping`]). A query L2 builds no step for (one required clause, even one the analyzer splits in
    /// several terms) or a plan without L2 leaves it false.
    pub(crate) drops_terms: bool,
}

impl<'a> LevelInput<'a> {
    pub(crate) fn new(
        original: &'a Query,
        compiled: Option<&'a Compiled>,
        stats: &'a dyn StatisticsProvider,
    ) -> Self {
        Self {
            original,
            compiled,
            stats,
            expands: &no_field,
            terms: None,
            drops_terms: true,
        }
    }

    /// This input with L3 expanding the terms of the fields `expands` accepts.
    pub(crate) fn expanding(self, expands: &'a dyn Fn(FieldId) -> Option<f64>) -> Self {
        Self { expands, ..self }
    }

    /// This input with L2's one-term rung built before it, or not (ROADMAP 8.2.7).
    pub(crate) fn dropping(self, drops_terms: bool) -> Self {
        Self {
            drops_terms,
            ..self
        }
    }

    /// This input with L4 walking `terms` for its corrections.
    pub(crate) fn walking(self, terms: &'a dyn TermSource) -> Self {
        Self {
            terms: Some(terms),
            ..self
        }
    }
}

fn no_field(_: FieldId) -> Option<f64> {
    None
}

impl fmt::Debug for LevelInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LevelInput")
            .field("original", &self.original)
            .field("compiled", &self.compiled)
            .finish_non_exhaustive()
    }
}

/// One query a level runs (ROADMAP 8.1.2): a level may run several, strictest first (L2's `minimum_should_match`
/// ladder, ROADMAP 8.2.2.2), each its own step of the cascade.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Step {
    /// The query.
    pub(crate) query: Query,
    /// What the step applied, as the response's `applied` lists it (SPEC §7.6): `"drop_terms:75%"`, `"fuzzy"`.
    pub(crate) applied: String,
    /// The step's query as user text (SPEC §7.6 `effective`): `None` when the query was not compiled from text, which
    /// a level then has none to render.
    pub(crate) effective: Option<String>,
    /// The synonym alternatives the step may drop to fit the limits (SPEC §7.1.3, ROADMAP 8.1.13): L1 builds its
    /// query from them, and the cascade rebuilds it with fewer when it does not fit. `None` for any other level. The
    /// rebuilt query keeps its `prefix` and `fuzzy` caps: a step with both is cut in one or the other, not both (no
    /// level builds such a step: compiled text has no `prefix` or `fuzzy`).
    pub(crate) alternatives: Option<Expansion>,
    /// The words the step replaced, in query order (SPEC §7.6 `corrected`): L4's corrections, empty for any other
    /// level.
    pub(crate) corrected: Vec<Correction>,
}

/// A word the fuzzy level replaced (SPEC §7.6 `"corrected"`, ROADMAP 8.2.4.3): a word no document holds, and the term
/// of highest collection `df` within its distance, which the response searched instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    /// The word as the user wrote it: `"Zapatilas"`.
    pub original: String,
    /// The term the step searched for it, as the index holds it (analyzed: lowercased, and stemmed under a
    /// stemming analyzer): `"zapatillas"`.
    pub corrected: String,
}

/// Builds a level's steps (SPEC §7.6). Called only once the cascade reaches the level; no step skips the level.
pub(crate) trait LevelTransform: Send + Sync + fmt::Debug {
    /// The level's steps for `input`, strictest first.
    ///
    /// # Errors
    /// A statistic that cannot be read ([`StatisticsProvider::df`]).
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError>;

    /// Whether the level's steps are *nested* (SPEC §7.6, ROADMAP 8.2.2.4): each, as built, matches every document
    /// the level's earlier steps match, with the same `must_not` and `filter`. Excluding the latest one then excludes
    /// them all, so the cascade excludes only that one of the level's steps from the next. A step cut to fit the limits
    /// (SPEC §7.1.3) matches less than it was built to: the cascade keeps excluding the earlier steps beside it.
    /// `false` unless a level proves it: L2's ladder is nested, L1's steps are not. Excluding fewer steps never makes
    /// the cascade skip one the full exclusions would search when, as at L2, every step of the level built uncut
    /// counts alike against the limits (its rungs differ only in `minimum_should_match`). A nested level whose steps
    /// grow needs its own argument.
    fn nested(&self) -> bool {
        false
    }

    /// Whether the level's step, as built, matches every document every earlier step of the cascade matches (SPEC §7.6,
    /// ADR-0047): L5's `match_all` under L0's root `must_not` and `filter`, which every earlier step keeps. Excluding
    /// it then excludes them all, so the cascade excludes only it from the later steps (the filter step). `false`
    /// unless a level proves it. It holds only while every level before L5 keeps L0's root `must_not` and `filter` in
    /// its steps (L1 to L4 do, and a cut never touches them, SPEC §7.1.3): a new level that rewrites them must not run
    /// before a level that covers the earlier steps, or the steps after it would return documents twice.
    fn covers_earlier(&self) -> bool {
        false
    }

    /// Whether the cascade builds the level's steps to fill the page (`fill`, SPEC §7.6), or only while the earlier
    /// steps found fewer than `min_results` hits. Every level fills but the filter step (ROADMAP 8.2.6, ADR-0047): a
    /// filtered page is never padded with documents the filters reject.
    fn fills(&self) -> bool {
        true
    }
}

/// L0: the query as compiled.
#[derive(Debug)]
struct Exact;

impl LevelTransform for Exact {
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        Ok(vec![Step {
            query: input.original.clone(),
            applied: String::new(),
            effective: input.compiled.map(Compiled::effective),
            alternatives: None,
            corrected: Vec::new(),
        }])
    }
}

/// The transform of `level` as this version of the engine defines it.
fn transform(level: RelaxLevel) -> Arc<dyn LevelTransform> {
    match level {
        RelaxLevel::Exact => Arc::new(Exact),
        RelaxLevel::Synonyms => Arc::new(synonyms::Synonyms),
        RelaxLevel::DropTerms => Arc::new(drop_terms::DropTerms),
        RelaxLevel::Prefix => Arc::new(prefix::Prefix),
        RelaxLevel::Fuzzy => Arc::new(fuzzy::Fuzzy),
        RelaxLevel::Fallback => Arc::new(fallback::Fallback),
        RelaxLevel::Filters => Arc::new(filters::Filters),
    }
}

/// A level of a [`RelaxPlan`] and its transform.
#[derive(Debug, Clone)]
pub(crate) struct PlannedLevel {
    pub(crate) level: RelaxLevel,
    pub(crate) transform: Arc<dyn LevelTransform>,
}

/// The relaxation cascade a search may run (SPEC §7.6): its levels in order, L0 first, and when it stops.
#[derive(Debug, Clone)]
pub struct RelaxPlan {
    levels: Vec<PlannedLevel>,
    min_results: u32,
    fill: bool,
    filters: FilterRelaxation,
    budget: Duration,
    limits: Option<QueryLimits>,
    /// Every step excludes every earlier step, nested or not: the rule before ROADMAP 8.2.2.4, which tests compare to.
    /// Only tests set it.
    exclude_every_step: bool,
}

impl RelaxPlan {
    /// The cascade of a collection whose defaults are `collection` (its schema's [`CollectionSchema::relaxation`]),
    /// as a query's `options` override them: L0, then the levels `levels` names in cascade order, then L5 unless
    /// `fallback` is [`Fallback::None`], then [`RelaxLevel::Filters`] when `filters` is [`FilterRelaxation::LastResort`].
    /// Disabled (`enabled: false` in either, the query's winning), it is L0 alone.
    ///
    /// # Errors
    /// [`RelaxError::OutOfRange`] for a `min_results` outside [`RelaxationConfig::MIN_RESULTS`] or a `budget_ms` outside
    /// [`RelaxationConfig::BUDGET_MS`]; [`RelaxError::Levels`] for custom levels that repeat one or are out of cascade order.
    /// A collection's settings were checked when its schema was built; a query's are checked here.
    pub fn new(collection: &RelaxationConfig, options: &RelaxOptions) -> Result<Self, RelaxError> {
        let min_results = options.min_results.unwrap_or(collection.min_results);
        options::check(
            "min_results",
            min_results,
            RelaxationConfig::MIN_RESULTS,
            RelaxationConfig::MIN_RESULTS_ALLOWED,
        )?;
        let budget_ms = options.budget_ms.unwrap_or(collection.budget_ms);
        options::check(
            "budget_ms",
            budget_ms,
            RelaxationConfig::BUDGET_MS,
            RelaxationConfig::BUDGET_MS_ALLOWED,
        )?;
        let levels = options.levels.as_ref().unwrap_or(&collection.levels);
        if let Some(problem) = levels.problem() {
            return Err(RelaxError::Levels { problem });
        }
        let enabled = options.enabled.unwrap_or(collection.enabled);
        let fallback = options.fallback.unwrap_or(collection.fallback);
        let filters = options.filters.unwrap_or(collection.filters);
        let mut planned = vec![RelaxLevel::Exact];
        if enabled {
            planned.extend(levels.resolve().iter().copied().map(RelaxLevel::from));
            if fallback == Fallback::StaticRank {
                planned.push(RelaxLevel::Fallback);
            }
            // The last resort, after every other level (ADR-0047): only the explicit opt-in plans it.
            if filters == FilterRelaxation::LastResort {
                planned.push(RelaxLevel::Filters);
            }
        }
        Ok(Self {
            levels: planned
                .into_iter()
                .map(|level| PlannedLevel {
                    level,
                    transform: transform(level),
                })
                .collect(),
            min_results,
            fill: options.fill.unwrap_or(collection.fill),
            filters,
            budget: Duration::from_millis(u64::from(budget_ms)),
            limits: None,
            exclude_every_step: false,
        })
    }

    /// The levels, in the order the cascade runs them: [`RelaxLevel::Exact`] first.
    pub fn levels(&self) -> impl ExactSizeIterator<Item = RelaxLevel> + '_ {
        self.levels.iter().map(|l| l.level)
    }

    /// Whether the plan runs more than L0.
    pub fn relaxes(&self) -> bool {
        self.levels.len() > 1
    }

    /// The settings its cursors record (ROADMAP 8.3.1): `None` when it runs L0 alone, as a plain search does, so their
    /// cursors page each other.
    pub fn settings(&self) -> Option<RelaxSettings> {
        self.relaxes().then(|| RelaxSettings::of(self))
    }

    /// The cascade runs while fewer hits than this were found.
    pub fn min_results(&self) -> u32 {
        self.min_results
    }

    /// Whether the cascade keeps relaxing to fill the page once it has `min_results` hits.
    pub fn fill(&self) -> bool {
        self.fill
    }

    /// Whether filters may be relaxed: with [`FilterRelaxation::LastResort`] the plan ends with
    /// [`RelaxLevel::Filters`] (ROADMAP 8.2.6, ADR-0047), unless it runs L0 alone.
    pub fn filters(&self) -> FilterRelaxation {
        self.filters
    }

    /// The cascade's time budget (`budget_ms`).
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// The limits each step after L0 is held to, with the earlier steps it excludes (SPEC §7.1.3, §7.6), when
    /// [`Self::with_limits`] set them. Otherwise a compiled query's own ([`Compiled::limits`]) apply, and
    /// [`QueryLimits::default`] to a programmatic one.
    pub fn limits(&self) -> Option<&QueryLimits> {
        self.limits.as_ref()
    }

    /// This plan with its steps held to `limits`: those a programmatic query was validated with (the engine's, when
    /// they are not the defaults: under lower limits than its own L0 every later step is skipped). A step that does
    /// not fit them beside the earlier steps it excludes has its `prefix` and `fuzzy` expansions cut, and is skipped
    /// when no cut is enough (SPEC §7.6). A cursor's next page needs the same limits to build the same steps.
    #[must_use]
    pub fn with_limits(mut self, limits: QueryLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    pub(crate) fn planned(&self) -> &[PlannedLevel] {
        &self.levels
    }

    /// Whether a step of a nested level ([`LevelTransform::nested`]) may stand for the level's earlier steps in the
    /// later steps' exclusions (ROADMAP 8.2.2.4). Always, outside tests.
    pub(crate) fn excludes_latest_only(&self) -> bool {
        !self.exclude_every_step
    }

    /// This plan with every step excluding every earlier one, nested or not: the rule ROADMAP 8.2.2.4 replaced.
    #[cfg(test)]
    pub(crate) fn excluding_every_step(mut self) -> Self {
        self.exclude_every_step = true;
        self
    }

    /// This plan with `level`'s transform replaced, or added in cascade order when the plan lacks the level.
    #[cfg(test)]
    pub(crate) fn with_level(
        mut self,
        level: RelaxLevel,
        transform: Arc<dyn LevelTransform>,
    ) -> Self {
        if let Some(planned) = self.levels.iter_mut().find(|l| l.level == level) {
            planned.transform = transform;
        } else {
            let at = self.levels.partition_point(|l| l.level < level);
            self.levels.insert(at, PlannedLevel { level, transform });
        }
        self
    }
}

/// A hit's *rank score* (SPEC §7.6, ROADMAP 8.1.3): one number that orders hits as the cascade does, for clients that
/// sort or display by one. `2^−(step+1) · (1 + g(score))`, where `g(s) = 1 − 1/(1 + s)`, rounded at each operation.
///
/// Each operation is monotonic in its argument (`1 + s` and `1 + g` non-decreasing, `1/x` and `1 − y`
/// non-increasing) and the power of two is exact, so for every score `s ≥ 0`, `+∞` included:
/// - `g(s)` is in `[0, 1]` (`1/(1 + s)` is in `(0, 1]`, and rounds `1 − y` up to at most 1), so step `i`'s rank
///   scores lie in `[2^−(i+1), 2^−i]`: none is below a later step's, whose band ends where this one starts;
/// - within a step, a higher score never has a lower rank score.
///
/// So down a relaxed page whose request sorts by score descending (the cascade's order: step ascending, then score
/// descending), the rank scores never increase. They can tie where scores differ, as rounding merges them: from
/// about `2^52.4` every score gives the step's top rank score (`1 + g` rounds to 2; `g` itself reaches 1 at `2^54`),
/// and up to `2^−53` its bottom one. So a step's lowest rank score can equal the next step's highest, and ordering
/// by the rank score alone cannot tell those hits' steps apart: the hit's step can. From step 1022 on, where
/// `2^−(step+1)` is no longer a normal number, it is 0. The hit's score itself is its step's.
///
/// ```
/// use persia_engine::relax::rank_score;
///
/// assert_eq!(rank_score(0, 0.0), 0.5);
/// assert_eq!(rank_score(0, f64::MAX), 1.0);
/// // A stricter step's worst hit is never below a looser step's best.
/// assert!(rank_score(0, 0.0) >= rank_score(1, f64::MAX));
/// assert!(rank_score(1, 2.0) > rank_score(1, 1.0));
/// ```
pub fn rank_score(step: usize, score: f64) -> f64 {
    // `max` maps a NaN, which no score is, to 0 as well.
    let s = score.max(0.0);
    let g = 1.0 - 1.0 / (1.0 + s);
    band(step) * (1.0 + g)
}

/// `2^−(step+1)` exactly, for the steps where it is a normal `f64`; 0 after them.
fn band(step: usize) -> f64 {
    u64::try_from(step)
        .ok()
        .and_then(|s| s.checked_add(1))
        .filter(|&e| e <= 1022)
        .map_or(0.0, |e| f64::from_bits((1023 - e) << 52))
}

#[cfg(test)]
mod tests;
