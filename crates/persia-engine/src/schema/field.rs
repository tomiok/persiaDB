//! Field types and per-type options (SPEC §6.1, §6.2.2).

use std::fmt;
use std::hash::{Hash, Hasher};

use persia_analysis::current_builtin_version;
pub use persia_analysis::{BUILTIN_ANALYZERS, BuiltinAnalyzer, DEFAULT_ANALYZER};

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
///
/// Its identity is the name and version: two refs are equal when they name the same analyzer version. A ref also
/// remembers whether the definition it came from named the version ([`is_pinned`](Self::is_pinned)), so that
/// evolution can bind an unversioned `analyzer` to the version a field was indexed with (SPEC §6.2.4).
#[derive(Debug, Clone)]
pub struct AnalyzerRef {
    name: String,
    version: u32,
    pinned: bool,
}

impl AnalyzerRef {
    /// The current version of built-in analyzer `name`, not pinned, or `None` if there is no such analyzer.
    pub fn builtin(name: &str) -> Option<Self> {
        let version = current_builtin_version(name)?;
        Some(Self {
            name: name.to_owned(),
            version,
            pinned: false,
        })
    }

    /// Version `version` of built-in analyzer `name`, pinned, or `None` if it does not exist.
    pub fn new(name: &str, version: u32) -> Option<Self> {
        let exists = BUILTIN_ANALYZERS
            .iter()
            .any(|a| a.name == name && a.version == version);
        exists.then(|| Self {
            name: name.to_owned(),
            version,
            pinned: true,
        })
    }

    /// Parses `"name"` (the current version, not pinned) or `"name@version"` (pinned; canonical decimal, no sign or
    /// leading zero).
    pub fn parse(text: &str) -> Option<Self> {
        if !text.contains('@') {
            return Self::builtin(text);
        }
        let (name, version) = split_versioned(text)?;
        Self::new(name, version)
    }

    /// Whether `text` is a well-formed `"name@version"` (a name of built-in shape, a canonical version from 1), whether or
    /// not this build has that analyzer version: a stored schema naming one it lacks was written by a newer build.
    pub(crate) fn is_well_formed_versioned(text: &str) -> bool {
        split_versioned(text).is_some()
    }

    /// The analyzer's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The analyzer's version.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Whether the definition named the version (`"es@1"`) rather than leaving it to the current one (`"es"`).
    /// A schema the engine stored always pins its versions (SPEC §6.2.2).
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// The same analyzer at `version`, pinned, without checking that the version exists: tests of version binding
    /// need a version the build does not ship yet.
    #[cfg(test)]
    pub(crate) fn at_unchecked(&self, version: u32) -> Self {
        Self {
            name: self.name.clone(),
            version,
            pinned: true,
        }
    }
}

/// `"name@version"` split into a name of built-in shape ([`is_builtin_name`]) and a version written in canonical decimal (no sign or leading zero,
/// so from 1) that fits a `u32`.
fn split_versioned(text: &str) -> Option<(&str, u32)> {
    let (name, version) = text.split_once('@')?;
    let canonical = is_builtin_name(name)
        && !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit())
        && !version.starts_with('0');
    if !canonical {
        return None;
    }
    Some((name, version.parse().ok()?))
}

/// Whether `name` has the shape of a built-in analyzer name, `[a-z][a-z0-9_-]*` (SPEC §6.3.3), whether or not this
/// build has it.
fn is_builtin_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl PartialEq for AnalyzerRef {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.version == other.version
    }
}

impl Eq for AnalyzerRef {}

impl Hash for AnalyzerRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.version.hash(state);
    }
}

impl Default for AnalyzerRef {
    /// The current version of [`DEFAULT_ANALYZER`], not pinned.
    fn default() -> Self {
        Self::builtin(DEFAULT_ANALYZER).unwrap_or_else(|| Self {
            name: DEFAULT_ANALYZER.to_owned(),
            version: 1,
            pinned: false,
        })
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
    fn every_builtin_resolves_through_the_registry() {
        for a in BUILTIN_ANALYZERS {
            let pinned = AnalyzerRef::new(a.name, a.version).unwrap();
            assert!(pinned.is_pinned());
            assert_eq!((pinned.name(), pinned.version()), (a.name, a.version));
            assert!(AnalyzerRef::builtin(a.name).unwrap().version() >= a.version);
        }
        for gone in ["nl", "zh", "ru"] {
            assert_eq!(AnalyzerRef::builtin(gone), None, "{gone}");
        }
        // `Default` falls back only if the default analyzer is missing from the registry: never.
        assert_eq!(
            Some(AnalyzerRef::default()),
            AnalyzerRef::builtin(DEFAULT_ANALYZER)
        );
        assert_eq!(AnalyzerRef::default().name(), DEFAULT_ANALYZER);
        assert!(!AnalyzerRef::default().is_pinned());
    }

    #[test]
    fn pinning_is_remembered_but_not_identity() {
        use std::hash::BuildHasher;
        let unpinned = AnalyzerRef::parse("es").unwrap();
        let pinned = AnalyzerRef::parse("es@1").unwrap();
        assert!(!unpinned.is_pinned() && pinned.is_pinned());
        assert_eq!(unpinned, pinned);
        let state = std::collections::hash_map::RandomState::new();
        assert_eq!(state.hash_one(&unpinned), state.hash_one(&pinned));
        assert_ne!(pinned, pinned.at_unchecked(2));
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
