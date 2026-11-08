//! Zero-results avoidance (SPEC §7.6, ROADMAP M8): the relaxation cascade's plan.
//!
//! A [`RelaxPlan`] is the ordered list of levels a search may run, L0 (`exact`, the query as compiled) first, each
//! with the transform that builds its queries from the ones before it. The collection's `relaxation` settings
//! (SPEC §6.2.3, [`RelaxationConfig`]) give the defaults, and a query's `relax` ([`RelaxOptions`]) overrides them,
//! `relax: false` ([`RelaxOptions::disabled`]) leaving L0 alone.
//!
//! A level's transform runs only once the cascade reaches it, so a query that has `min_results` hits at L0 builds
//! nothing more. Only L0's transform exists so far: the others (L1 `synonyms` to L5 `fallback`, ROADMAP 8.2) build
//! no query yet, so the cascade skips them.
//!
//! ```
//! use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
//! use persia_engine::schema::{Fallback, Level, Levels, RelaxationConfig};
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
//! // `relax: false` runs L0 only.
//! let plan = RelaxPlan::new(&collection, &RelaxOptions::disabled())?;
//! assert_eq!(plan.levels().collect::<Vec<_>>(), [RelaxLevel::Exact]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::query::Query;
use crate::schema::{
    CollectionSchema, Fallback, FilterRelaxation, Level, Levels, RelaxationConfig,
};

/// `min_results`' range (SPEC §6.2.3): a query's override is held to the collection's own.
pub const MIN_RESULTS_RANGE: core::ops::RangeInclusive<u32> = 1..=10_000;

/// `budget_ms`' range (SPEC §6.2.3).
pub const BUDGET_MS_RANGE: core::ops::RangeInclusive<u32> = 1..=60_000;

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

impl fmt::Display for RelaxLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A query's `relax` (SPEC §7.1, §7.6): each setting it gives overrides the collection's ([`RelaxationConfig`]), and
/// those it leaves `None` keep it. The default overrides nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelaxOptions {
    /// `false` runs L0 only (`relax: false`).
    pub enabled: Option<bool>,
    /// Relax while there are fewer hits than this, in [`MIN_RESULTS_RANGE`].
    pub min_results: Option<u32>,
    /// The levels after L0, before the fallback.
    pub levels: Option<Levels>,
    /// What L5 does.
    pub fallback: Option<Fallback>,
    /// Keep relaxing to fill the page once `min_results` is reached.
    pub fill: Option<bool>,
    /// Whether filters may be relaxed.
    pub filters: Option<FilterRelaxation>,
    /// The cascade's time budget in milliseconds, in [`BUDGET_MS_RANGE`].
    pub budget_ms: Option<u32>,
}

impl RelaxOptions {
    /// `relax: false`: L0 only, whatever the collection's settings.
    pub fn disabled() -> Self {
        Self {
            enabled: Some(false),
            ..Self::default()
        }
    }
}

/// A query's `relax` that breaks a rule of SPEC §6.2.3.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RelaxError {
    /// A setting outside its range.
    #[error("relax.{option} = {value}: must be {allowed}")]
    OutOfRange {
        /// The setting.
        option: &'static str,
        /// The value given.
        value: u32,
        /// The range allowed.
        allowed: &'static str,
    },
    /// `levels` repeats a level or lists them out of cascade order.
    #[error("relax.levels: {problem}")]
    Levels {
        /// What is wrong.
        problem: &'static str,
    },
}

/// What a level's transform builds from: the query as compiled and the queries the cascade built before.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct LevelInput<'a> {
    /// L0's query, as compiled.
    pub original: &'a Query,
    /// The query of the last step built before this level: L0's for L1.
    pub previous: &'a Query,
    /// The collection's schema.
    pub schema: &'a CollectionSchema,
}

/// One query a level runs (ROADMAP 8.1.2): a level may run several, strictest first (L2's `minimum_should_match`
/// ladder, ROADMAP 8.2.2.2), each its own step of the cascade.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The query.
    pub query: Query,
    /// What the step applied, as the response's `applied` lists it (SPEC §7.6): `"drop_terms:75%"`, `"fuzzy"`.
    pub applied: String,
}

/// Builds a level's steps (SPEC §7.6). Called only once the cascade reaches the level; no step skips the level.
// The orchestrator (ROADMAP 8.1.2) is what calls it; until then only the tests do.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait LevelTransform: Send + Sync + fmt::Debug {
    /// The level's steps for `input`, strictest first.
    fn steps(&self, input: &LevelInput<'_>) -> Vec<Step>;
}

/// L0: the query as compiled.
#[derive(Debug)]
struct Exact;

impl LevelTransform for Exact {
    fn steps(&self, input: &LevelInput<'_>) -> Vec<Step> {
        vec![Step {
            query: input.original.clone(),
            applied: String::new(),
        }]
    }
}

/// A level whose transform is not built yet (`leaf`, ROADMAP 8.2): no step, so the cascade skips it.
#[derive(Debug)]
struct Pending {
    #[allow(dead_code)] // Read by `Debug`, which names the leaf the level waits on.
    leaf: &'static str,
}

impl LevelTransform for Pending {
    fn steps(&self, _input: &LevelInput<'_>) -> Vec<Step> {
        Vec::new()
    }
}

/// The transform of `level` as this version of the engine defines it.
fn transform(level: RelaxLevel) -> Arc<dyn LevelTransform> {
    match level {
        RelaxLevel::Exact => Arc::new(Exact),
        RelaxLevel::Synonyms => Arc::new(Pending { leaf: "8.2.1" }),
        RelaxLevel::DropTerms => Arc::new(Pending { leaf: "8.2.2" }),
        RelaxLevel::Prefix => Arc::new(Pending { leaf: "8.2.3" }),
        RelaxLevel::Fuzzy => Arc::new(Pending { leaf: "8.2.4" }),
        RelaxLevel::Fallback => Arc::new(Pending { leaf: "8.2.5" }),
    }
}

/// A level of a [`RelaxPlan`] and its transform.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))] // Read by the orchestrator, ROADMAP 8.1.2.
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
}

impl RelaxPlan {
    /// The cascade of a collection whose defaults are `collection` (its schema's [`CollectionSchema::relaxation`]),
    /// as a query's `options` override them: L0, then the levels `levels` names in cascade order, then L5 unless
    /// `fallback` is [`Fallback::None`]. Disabled (`enabled: false` in either, the query's winning), it is L0 alone.
    ///
    /// # Errors
    /// [`RelaxError::OutOfRange`] for a `min_results` outside [`MIN_RESULTS_RANGE`] or a `budget_ms` outside
    /// [`BUDGET_MS_RANGE`]; [`RelaxError::Levels`] for custom levels that repeat one or are out of cascade order.
    /// A collection's settings were checked when its schema was built; a query's are checked here.
    pub fn new(collection: &RelaxationConfig, options: &RelaxOptions) -> Result<Self, RelaxError> {
        let min_results = options.min_results.unwrap_or(collection.min_results);
        check("min_results", min_results, MIN_RESULTS_RANGE, "1..=10000")?;
        let budget_ms = options.budget_ms.unwrap_or(collection.budget_ms);
        check("budget_ms", budget_ms, BUDGET_MS_RANGE, "1..=60000")?;
        let levels = options.levels.as_ref().unwrap_or(&collection.levels);
        check_levels(levels)?;
        let enabled = options.enabled.unwrap_or(collection.enabled);
        let fallback = options.fallback.unwrap_or(collection.fallback);
        let mut planned = vec![RelaxLevel::Exact];
        if enabled {
            planned.extend(levels.resolve().iter().copied().map(RelaxLevel::from));
            if fallback == Fallback::StaticRank {
                planned.push(RelaxLevel::Fallback);
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
            filters: options.filters.unwrap_or(collection.filters),
            budget: Duration::from_millis(u64::from(budget_ms)),
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

    /// The cascade runs while fewer hits than this were found.
    pub fn min_results(&self) -> u32 {
        self.min_results
    }

    /// Whether the cascade keeps relaxing to fill the page once it has `min_results` hits.
    pub fn fill(&self) -> bool {
        self.fill
    }

    /// Whether filters may be relaxed (ROADMAP 8.2.6 builds that level).
    pub fn filters(&self) -> FilterRelaxation {
        self.filters
    }

    /// The cascade's time budget (`budget_ms`). The cascade applies it from ROADMAP 8.1.4.
    pub fn budget(&self) -> Duration {
        self.budget
    }

    #[cfg_attr(not(test), allow(dead_code))] // Read by the orchestrator, ROADMAP 8.1.2.
    pub(crate) fn planned(&self) -> &[PlannedLevel] {
        &self.levels
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

fn check(
    option: &'static str,
    value: u32,
    range: core::ops::RangeInclusive<u32>,
    allowed: &'static str,
) -> Result<(), RelaxError> {
    if range.contains(&value) {
        Ok(())
    } else {
        Err(RelaxError::OutOfRange {
            option,
            value,
            allowed,
        })
    }
}

fn check_levels(levels: &Levels) -> Result<(), RelaxError> {
    let Levels::Custom(levels) = levels else {
        return Ok(());
    };
    for pair in levels.windows(2) {
        if let [a, b] = pair {
            if a == b {
                return Err(RelaxError::Levels {
                    problem: "a level is listed twice",
                });
            }
            if a > b {
                return Err(RelaxError::Levels {
                    problem: "levels must be in cascade order (synonyms, drop_terms, prefix, fuzzy)",
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
