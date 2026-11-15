//! A query's `relax` (SPEC §6.2.3, §7.6, ROADMAP 8.3.1, ADR-0045): the settings it overrides, their JSON form, their
//! rules, and the settings a cursor records so a page under other settings is an error.

use std::fmt;

use super::{RelaxLevel, RelaxPlan};
use crate::schema::{
    Fallback, FilterRelaxation, Levels, RawRelaxation, RelaxationConfig, relax_from_json,
};

/// A query's `relax` (SPEC §7.1, §7.6): each setting it gives overrides the collection's ([`RelaxationConfig`]), and
/// those it leaves `None` keep it, one setting at a time. The default overrides nothing. Every setting of SPEC §6.2.3's
/// `relaxation` may be given; the limits each step is held to ([`RelaxPlan::with_limits`]) are the engine's, not a
/// query's.
///
/// ```
/// use persia_engine::relax::{RelaxError, RelaxOptions};
///
/// let options = RelaxOptions::from_json(r#"{"min_results": 5, "fill": true}"#)?;
/// assert_eq!((options.min_results, options.fill, options.levels), (Some(5), Some(true), None));
/// // `relax: false` is `enabled: false`.
/// assert_eq!(RelaxOptions::from_json("false")?, RelaxOptions::disabled());
/// // Out of range, unknown or repeated keys are errors.
/// assert!(matches!(RelaxOptions::from_json(r#"{"min_results": 0}"#), Err(RelaxError::OutOfRange { .. })));
/// assert!(matches!(RelaxOptions::from_json(r#"{"minimum": 5}"#), Err(RelaxError::Json(_))));
/// # Ok::<(), RelaxError>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelaxOptions {
    /// `false` runs L0 only (`relax: false`).
    pub enabled: Option<bool>,
    /// Relax while there are fewer hits than this, in [`RelaxationConfig::MIN_RESULTS`].
    pub min_results: Option<u32>,
    /// The levels after L0, before the fallback.
    pub levels: Option<Levels>,
    /// What L5 does.
    pub fallback: Option<Fallback>,
    /// Keep relaxing to fill the page once `min_results` is reached (SPEC §7.6, ROADMAP 8.3.2).
    pub fill: Option<bool>,
    /// Whether filters may be relaxed: `"last_resort"` plans the filter step after every other level (ROADMAP 8.2.6,
    /// ADR-0047).
    pub filters: Option<FilterRelaxation>,
    /// The cascade's time budget in milliseconds, in [`RelaxationConfig::BUDGET_MS`].
    pub budget_ms: Option<u32>,
}

impl RelaxOptions {
    /// The longest JSON [`Self::from_json`] reads: far more than every key with a long `levels` array needs.
    pub const MAX_JSON_LEN: usize = 64 << 10;

    /// `relax: false`: L0 only, whatever the collection's settings.
    pub fn disabled() -> Self {
        Self {
            enabled: Some(false),
            ..Self::default()
        }
    }

    /// Parses a query's `relax` (SPEC §7.6): `false` (`enabled: false`), `true` (`enabled: true`), or an object of
    /// SPEC §6.2.3's `relaxation` keys, read strictly as a collection definition is (ADR-0022), and checks it
    /// ([`Self::validate`]).
    ///
    /// # Errors
    /// [`RelaxError::Json`] for JSON over [`Self::MAX_JSON_LEN`], malformed, of another shape, with an unknown or
    /// repeated key, or with `null` for a key; the errors of [`Self::validate`].
    pub fn from_json(json: &str) -> Result<Self, RelaxError> {
        if json.len() > Self::MAX_JSON_LEN {
            return Err(RelaxError::Json(format!(
                "{} bytes, at most {}",
                json.len(),
                Self::MAX_JSON_LEN
            )));
        }
        let RawRelaxation {
            enabled,
            min_results,
            levels,
            fallback,
            fill,
            filters,
            budget_ms,
        } = relax_from_json(json).map_err(RelaxError::Json)?;
        let options = Self {
            enabled,
            min_results,
            levels,
            fallback,
            fill,
            filters,
            budget_ms,
        };
        options.validate()?;
        Ok(options)
    }

    /// Checks the settings it gives against SPEC §6.2.3's rules, whether or not it disables the cascade (SPEC §7.6): a
    /// `min_results` in [`RelaxationConfig::MIN_RESULTS`], a `budget_ms` in [`RelaxationConfig::BUDGET_MS`], custom
    /// `levels` distinct and in cascade order. [`RelaxPlan::new`] checks the same.
    ///
    /// # Errors
    /// [`RelaxError::OutOfRange`] or [`RelaxError::Levels`] for the first setting that breaks its rule.
    pub fn validate(&self) -> Result<(), RelaxError> {
        if let Some(min_results) = self.min_results {
            check(
                "min_results",
                min_results,
                RelaxationConfig::MIN_RESULTS,
                RelaxationConfig::MIN_RESULTS_ALLOWED,
            )?;
        }
        if let Some(budget_ms) = self.budget_ms {
            check(
                "budget_ms",
                budget_ms,
                RelaxationConfig::BUDGET_MS,
                RelaxationConfig::BUDGET_MS_ALLOWED,
            )?;
        }
        match self.levels.as_ref().and_then(Levels::problem) {
            Some(problem) => Err(RelaxError::Levels { problem }),
            None => Ok(()),
        }
    }
}

/// A query's `relax` that breaks a rule of SPEC §6.2.3, or whose JSON is not its form (SPEC §7.6).
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
    /// JSON that is not a `relax` ([`RelaxOptions::from_json`]).
    #[error("relax: {0}")]
    Json(String),
}

pub(super) fn check(
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

/// The settings of a relaxing cascade that decide its ranking (SPEC §7.4, §7.6, ROADMAP 8.3.1, ADR-0045): its levels,
/// `min_results`, `fill` and `filters`. A cursor records them ([`crate::db::Cursor::relax`]), and a page under other
/// settings is [`crate::search::RequestError::CursorRelax`]: its steps or where they stop differ, so it would not be a
/// slice of the cursor's ranking. `budget_ms` is not among them: the budget only truncates a page, which the cursor's
/// last step already carries (SPEC §7.6). A plan that runs L0 alone has none ([`RelaxPlan::settings`]), as a plain
/// search has none: their pages rank alike.
///
/// ```
/// use persia_engine::relax::{RelaxOptions, RelaxPlan};
/// use persia_engine::schema::RelaxationConfig;
///
/// let collection = RelaxationConfig::default();
/// let plan = RelaxPlan::new(&collection, &RelaxOptions::default())?;
/// let more = RelaxPlan::new(&collection, &RelaxOptions { min_results: Some(5), ..RelaxOptions::default() })?;
/// let slower = RelaxPlan::new(&collection, &RelaxOptions { budget_ms: Some(500), ..RelaxOptions::default() })?;
/// assert_ne!(plan.settings(), more.settings());
/// assert_eq!(plan.settings(), slower.settings());
/// assert_eq!(RelaxPlan::new(&collection, &RelaxOptions::disabled())?.settings(), None);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RelaxSettings {
    /// One bit per [`RelaxLevel`] the plan runs, by [`level_bit`].
    levels: u16,
    min_results: u32,
    fill: bool,
    filters: FilterRelaxation,
}

impl RelaxSettings {
    /// The settings of `plan`.
    pub(super) fn of(plan: &RelaxPlan) -> Self {
        Self {
            levels: plan.levels().fold(0, |bits, level| bits | level_bit(level)),
            min_results: plan.min_results(),
            fill: plan.fill(),
            filters: plan.filters(),
        }
    }

    /// The levels the plan runs, in cascade order, L0 first.
    pub fn levels(&self) -> impl Iterator<Item = RelaxLevel> + '_ {
        super::LEVELS
            .into_iter()
            .filter(|&level| self.levels & level_bit(level) != 0)
    }

    /// Its `min_results`.
    pub fn min_results(self) -> u32 {
        self.min_results
    }

    /// Its `fill`.
    pub fn fill(self) -> bool {
        self.fill
    }

    /// Its `filters`.
    pub fn filters(self) -> FilterRelaxation {
        self.filters
    }
}

impl fmt::Display for RelaxSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("levels [")?;
        for (i, level) in self.levels().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(level.name())?;
        }
        write!(
            f,
            "], min_results {}, fill {}, filters {}",
            self.min_results,
            self.fill,
            self.filters.name()
        )
    }
}

/// `level`'s bit in [`RelaxSettings`]: its place in cascade order.
const fn level_bit(level: RelaxLevel) -> u16 {
    1 << match level {
        RelaxLevel::Exact => 0,
        RelaxLevel::Synonyms => 1,
        RelaxLevel::DropTerms => 2,
        RelaxLevel::Prefix => 3,
        RelaxLevel::Fuzzy => 4,
        RelaxLevel::Fallback => 5,
        RelaxLevel::Filters => 6,
    }
}
