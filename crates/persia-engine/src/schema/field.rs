//! Field types and per-type options (SPEC §6.1, §6.2.2).

use std::fmt;

/// Built-in analyzers of SPEC §6.3, all at version 1. M4 replaces this list with the analyzer registry
/// (ROADMAP 4.4.1, 4.4.3; ADR-0022).
pub const BUILTIN_ANALYZERS: &[&str] = &[
    "standard",
    "keyword",
    "whitespace",
    "en",
    "es",
    "pt",
    "fr",
    "de",
    "it",
];

/// The analyzer a text field gets when it names none.
pub const DEFAULT_ANALYZER: &str = "standard";

/// The only version each built-in analyzer has so far.
const CURRENT_VERSION: u32 = 1;

/// The value types of SPEC §6.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldType {
    /// Analyzed full text, scored with BM25F.
    Text,
    /// An exact term with doc values: filters, facets, sort.
    Keyword,
    /// A signed 64-bit integer.
    I64,
    /// A 64-bit float.
    F64,
    /// A boolean.
    Bool,
    /// Microseconds since the Unix epoch, UTC.
    Datetime,
    /// Kept in the `DocStore` only; not searchable.
    Stored,
}

impl FieldType {
    /// The type's name in a collection definition (`"text"`, `"i64"`, ...).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Keyword => "keyword",
            Self::I64 => "i64",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::Datetime => "datetime",
            Self::Stored => "stored",
        }
    }
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A field's type with the options that apply to it (SPEC §6.2.2).
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    /// Full text.
    Text(TextOptions),
    /// Exact keyword.
    Keyword(KeywordOptions),
    /// Signed integer.
    I64 {
        /// Whether `get` returns the value (SPEC §6.2.2).
        stored: bool,
    },
    /// Float.
    F64 {
        /// Whether `get` returns the value.
        stored: bool,
    },
    /// Boolean.
    Bool {
        /// Whether `get` returns the value.
        stored: bool,
    },
    /// Timestamp in microseconds UTC.
    Datetime {
        /// Whether `get` returns the value.
        stored: bool,
    },
    /// Stored only.
    Stored,
}

impl FieldKind {
    /// The kind's type.
    pub const fn field_type(&self) -> FieldType {
        match self {
            Self::Text(_) => FieldType::Text,
            Self::Keyword(_) => FieldType::Keyword,
            Self::I64 { .. } => FieldType::I64,
            Self::F64 { .. } => FieldType::F64,
            Self::Bool { .. } => FieldType::Bool,
            Self::Datetime { .. } => FieldType::Datetime,
            Self::Stored => FieldType::Stored,
        }
    }

    /// Whether `get` returns the field's value: always for [`FieldKind::Stored`].
    pub const fn stored(&self) -> bool {
        match self {
            Self::Text(o) => o.stored,
            Self::Keyword(o) => o.stored,
            Self::I64 { stored }
            | Self::F64 { stored }
            | Self::Bool { stored }
            | Self::Datetime { stored } => *stored,
            Self::Stored => true,
        }
    }
}

/// Options of a `text` field (SPEC §6.2.2).
#[derive(Debug, Clone, PartialEq)]
pub struct TextOptions {
    /// The analyzer terms are produced with.
    pub analyzer: AnalyzerRef,
    /// BM25F field weight, `0 < boost ≤ 1000`.
    pub boost: f64,
    /// Whether positions are indexed (phrase queries).
    pub positions: bool,
    /// BM25 term-frequency saturation, `0 ≤ k1 ≤ 10`.
    pub k1: f64,
    /// BM25 length normalization, `0 ≤ b ≤ 1`.
    pub b: f64,
    /// Whether `get` returns the value.
    pub stored: bool,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self {
            analyzer: AnalyzerRef::default(),
            boost: 1.0,
            positions: false,
            k1: 1.2,
            b: 0.75,
            stored: true,
        }
    }
}

/// Options of a `keyword` field (SPEC §6.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeywordOptions {
    /// Whether the field can be faceted (SPEC §7.5).
    pub facet: bool,
    /// Whether `get` returns the value.
    pub stored: bool,
}

impl Default for KeywordOptions {
    fn default() -> Self {
        Self {
            facet: false,
            stored: true,
        }
    }
}

/// A built-in analyzer at a specific version (SPEC §6.3); written as `name@version`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AnalyzerRef {
    name: String,
    version: u32,
}

impl AnalyzerRef {
    /// The current version of built-in analyzer `name`, or `None` if there is no such analyzer.
    pub fn builtin(name: &str) -> Option<Self> {
        Self::new(name, CURRENT_VERSION)
    }

    /// Version `version` of built-in analyzer `name`, or `None` if it does not exist.
    pub fn new(name: &str, version: u32) -> Option<Self> {
        (BUILTIN_ANALYZERS.contains(&name) && version == CURRENT_VERSION).then(|| Self {
            name: name.to_owned(),
            version,
        })
    }

    /// Parses `"name"` (the current version) or `"name@version"` (canonical decimal, no sign or leading zero).
    pub fn parse(text: &str) -> Option<Self> {
        let Some((name, version)) = text.split_once('@') else {
            return Self::builtin(text);
        };
        let canonical = !version.is_empty()
            && version.bytes().all(|b| b.is_ascii_digit())
            && !version.starts_with('0');
        if !canonical {
            return None;
        }
        Self::new(name, version.parse().ok()?)
    }

    /// The analyzer's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The analyzer's version.
    pub fn version(&self) -> u32 {
        self.version
    }
}

impl Default for AnalyzerRef {
    fn default() -> Self {
        Self {
            name: DEFAULT_ANALYZER.to_owned(),
            version: CURRENT_VERSION,
        }
    }
}

impl fmt::Display for AnalyzerRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_parse_accepts_name_and_canonical_version() {
        assert_eq!(AnalyzerRef::parse("es"), AnalyzerRef::builtin("es"));
        assert_eq!(AnalyzerRef::parse("es@1"), AnalyzerRef::builtin("es"));
        assert_eq!(
            AnalyzerRef::parse("es@1").map(|a| a.to_string()),
            Some("es@1".to_owned())
        );
        for bad in [
            "", "ES", "klingon", "es@", "es@0", "es@01", "es@+1", "es@2", "es@1@1", "@1", "es@ 1",
        ] {
            assert_eq!(AnalyzerRef::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn every_builtin_resolves_at_version_one() {
        for name in BUILTIN_ANALYZERS {
            assert_eq!(
                AnalyzerRef::builtin(name).map(|a| a.version()),
                Some(1),
                "{name}"
            );
        }
        assert_eq!(AnalyzerRef::default().name(), DEFAULT_ANALYZER);
    }

    #[test]
    fn stored_flag_per_kind() {
        assert!(FieldKind::Stored.stored());
        assert!(!FieldKind::I64 { stored: false }.stored());
        assert!(FieldKind::Text(TextOptions::default()).stored());
        assert!(
            !FieldKind::Keyword(KeywordOptions {
                stored: false,
                facet: true
            })
            .stored()
        );
    }
}
