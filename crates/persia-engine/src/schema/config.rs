//! Ranking and relaxation settings of a collection (SPEC §6.2.3, §7.6).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Ranking settings (SPEC §6.2.3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RankingConfig {
    /// A per-document score added to the text score, or none.
    pub static_rank: Option<StaticRank>,
}

/// `weight · function(value of field)`, added to every hit's score (SPEC §6.2.3, §7.2).
#[derive(Debug, Clone, PartialEq)]
pub struct StaticRank {
    /// An `i64`, `f64` or `datetime` field.
    pub field: String,
    /// `0 ≤ weight ≤ 1000`.
    pub weight: f64,
    /// How the field's value is shaped.
    pub function: RankFn,
}

impl StaticRank {
    /// A static rank on `field` with weight 1 and [`RankFn::Log1p`].
    pub fn new(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            weight: 1.0,
            function: RankFn::default(),
        }
    }
}

/// The function of a [`StaticRank`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RankFn {
    /// `ln(1 + max(value, 0))`: diminishing returns, the default (SPEC §6.2.3).
    #[default]
    Log1p,
    /// `max(value, 0)`.
    Linear,
}

impl RankFn {
    /// The function's name in a collection definition.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Log1p => "log1p",
            Self::Linear => "linear",
        }
    }
}

/// Collection defaults of the relaxation cascade (SPEC §6.2.3, §7.6); a query's `relax` overrides them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaxationConfig {
    /// `false` runs L0 (`exact`) only.
    pub enabled: bool,
    /// Relax while there are fewer hits than this, `1..=10_000`.
    pub min_results: u32,
    /// The levels after L0, before the fallback.
    pub levels: Levels,
    /// What L5 does.
    pub fallback: Fallback,
    /// Keep relaxing to fill the page once `min_results` is reached.
    pub fill: bool,
    /// Whether filters may be relaxed.
    pub filters: FilterRelaxation,
    /// Work budget of the cascade in milliseconds, `1..=60_000`.
    pub budget_ms: u32,
}

impl Default for RelaxationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_results: 1,
            levels: Levels::Default,
            fallback: Fallback::StaticRank,
            fill: false,
            filters: FilterRelaxation::Never,
            budget_ms: 50,
        }
    }
}

/// Which relaxation levels run after L0 (SPEC §7.6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Levels {
    /// Every level, as this version of the engine defines them: a later version may add levels.
    #[default]
    Default,
    /// These levels only: distinct and in cascade order.
    Custom(Vec<Level>),
}

impl Levels {
    /// The levels that run, in cascade order.
    pub fn resolve(&self) -> &[Level] {
        match self {
            Self::Default => &Level::ALL,
            Self::Custom(levels) => levels,
        }
    }
}

/// A relaxation level between L0 (`exact`) and L5 (`fallback`); ordered as the cascade runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// L1: expand terms with the collection's synonyms.
    Synonyms,
    /// L2: require fewer of the query's terms.
    DropTerms,
    /// L3: prefix-expand terms.
    Prefix,
    /// L4: edit-distance matching.
    Fuzzy,
}

impl Level {
    /// Every level, in cascade order.
    pub const ALL: [Self; 4] = [Self::Synonyms, Self::DropTerms, Self::Prefix, Self::Fuzzy];

    /// The level's name in a collection definition and in responses.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Synonyms => "synonyms",
            Self::DropTerms => "drop_terms",
            Self::Prefix => "prefix",
            Self::Fuzzy => "fuzzy",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What the last relaxation level (L5) returns (SPEC §7.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fallback {
    /// The top documents by static rank, or the most recently indexed without one.
    #[default]
    StaticRank,
    /// Nothing: the query may return no hits.
    None,
}

impl Fallback {
    /// The fallback's name in a collection definition.
    pub const fn name(self) -> &'static str {
        match self {
            Self::StaticRank => "static_rank",
            Self::None => "none",
        }
    }
}

/// Whether the cascade may relax filters (SPEC §7.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterRelaxation {
    /// Filters always hold.
    #[default]
    Never,
    /// Filters are dropped only after every other level failed.
    LastResort,
}

impl FilterRelaxation {
    /// The setting's name in a collection definition.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::LastResort => "last_resort",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let r = RelaxationConfig::default();
        assert!(r.enabled);
        assert_eq!(r.min_results, 1);
        assert_eq!(r.levels, Levels::Default);
        assert_eq!(r.fallback, Fallback::StaticRank);
        assert!(!r.fill);
        assert_eq!(r.filters, FilterRelaxation::Never);
        assert_eq!(r.budget_ms, 50);
        assert_eq!(RankingConfig::default().static_rank, None);
        let s = StaticRank::new("sales");
        assert_eq!((s.weight, s.function), (1.0, RankFn::Log1p));
    }

    #[test]
    fn default_levels_are_the_whole_cascade_in_order() {
        let all = Levels::Default.resolve();
        assert_eq!(all, Level::ALL);
        assert!(all.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(Levels::Custom(vec![Level::Fuzzy]).resolve(), [Level::Fuzzy]);
    }
}
