//! Field types and per-type options (SPEC §6.1, §6.2.2).

use std::fmt;
use std::hash::{Hash, Hasher};

use persia_analysis::{AnalyzerRegistry, current_builtin_version, is_analyzer_name};
pub use persia_analysis::{BUILTIN_ANALYZERS, BuiltinAnalyzer, DEFAULT_ANALYZER};

/// The value types of SPEC §6.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
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

// The setters of a `#[non_exhaustive]` options struct (ROADMAP 10.1.16, ADR-0063 amended): another crate cannot write
// its struct literal, so it starts from `default()` and sets what it needs.
impl TextOptions {
    /// These options with `analyzer`.
    ///
    /// ```
    /// use persia_engine::schema::{AnalyzerRef, FieldKind, TextOptions};
    ///
    /// let en = AnalyzerRef::builtin("en").ok_or("no en")?;
    /// let title = TextOptions::default().analyzer(en.clone()).boost(3.0).positions(true);
    /// assert_eq!((&title.analyzer, title.boost, title.positions), (&en, 3.0, true));
    /// let body = TextOptions::default().k1(1.5).b(0.5).stored(false);
    /// assert_eq!((body.k1, body.b, body.stored), (1.5, 0.5, false));
    /// let _ = FieldKind::Text(title);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn analyzer(mut self, analyzer: AnalyzerRef) -> Self {
        self.analyzer = analyzer;
        self
    }

    /// These options with BM25F field weight `boost`.
    #[must_use]
    pub const fn boost(mut self, boost: f64) -> Self {
        self.boost = boost;
        self
    }

    /// These options with positions indexed, or not.
    #[must_use]
    pub const fn positions(mut self, positions: bool) -> Self {
        self.positions = positions;
        self
    }

    /// These options with BM25 term-frequency saturation `k1`.
    #[must_use]
    pub const fn k1(mut self, k1: f64) -> Self {
        self.k1 = k1;
        self
    }

    /// These options with BM25 length normalization `b`.
    #[must_use]
    pub const fn b(mut self, b: f64) -> Self {
        self.b = b;
        self
    }

    /// These options with the value stored (`get` returns it), or not.
    #[must_use]
    pub const fn stored(mut self, stored: bool) -> Self {
        self.stored = stored;
        self
    }
}

/// Options of a `keyword` field (SPEC §6.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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

impl KeywordOptions {
    /// These options with facets on, or off.
    ///
    /// ```
    /// use persia_engine::schema::KeywordOptions;
    ///
    /// let brand = KeywordOptions::default().facet(true).stored(false);
    /// assert_eq!((brand.facet, brand.stored), (true, false));
    /// ```
    #[must_use]
    pub const fn facet(mut self, facet: bool) -> Self {
        self.facet = facet;
        self
    }

    /// These options with the value stored (`get` returns it), or not.
    #[must_use]
    pub const fn stored(mut self, stored: bool) -> Self {
        self.stored = stored;
        self
    }
}

/// An analyzer at a specific version (SPEC §6.3), built-in or registered with an [`AnalyzerRegistry`]; written as
/// `name@version`.
///
/// Its identity is the name and version: two refs are equal when they name the same analyzer version. A ref also
/// remembers whether the definition it came from named the version ([`is_pinned`](Self::is_pinned)), so that
/// evolution can bind an unversioned `analyzer` to the version a field was indexed with (SPEC §6.2.4).
///
/// A built-in ref comes from [`builtin`](Self::builtin), [`new`](Self::new) or [`parse`](Self::parse); a registered
/// one from [`parse_in`](Self::parse_in), which checks the registry. A database opened with a registry that lacks a
/// registered analyzer its schemas name fails to open (SPEC §6.2.2).
///
/// ```
/// use persia_analysis::{Analyzer, AnalyzerRegistry, WhitespaceTokenizer};
/// use persia_engine::schema::AnalyzerRef;
///
/// let mut registry = AnalyzerRegistry::builtin();
/// registry.register("tags", 2, Analyzer::builder(WhitespaceTokenizer).build())?;
/// let tags = AnalyzerRef::parse_in("tags", &registry).ok_or("no tags")?;
/// assert_eq!((tags.to_string().as_str(), tags.is_pinned(), tags.is_builtin()), ("tags@2", false, false));
/// assert_eq!(AnalyzerRef::parse_in("tags@1", &registry), None);
/// // Built-ins resolve whatever the registry holds; `parse` knows only them.
/// assert_eq!(AnalyzerRef::parse_in("en", &AnalyzerRegistry::new()), AnalyzerRef::parse("en"));
/// assert_eq!(AnalyzerRef::parse("tags@2"), None);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
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
    /// leading zero) of a built-in analyzer.
    pub fn parse(text: &str) -> Option<Self> {
        if !text.contains('@') {
            return Self::builtin(text);
        }
        let (name, version) = split_versioned(text)?;
        Self::new(name, version)
    }

    /// [`Self::parse`], also accepting an analyzer `registry` holds: `"name"` is then its highest registered version
    /// (not pinned) and `"name@version"` a registered version (pinned). A built-in name resolves only to a built-in
    /// version, whatever `registry` holds (SPEC §6.2.2).
    // Engine detail, hidden from the public API of `persia`, which re-exports this type (ROADMAP 10.1.16, ADR-0063).
    #[doc(hidden)]
    pub fn parse_in(text: &str, registry: &AnalyzerRegistry) -> Option<Self> {
        if let Some(builtin) = Self::parse(text) {
            return Some(builtin);
        }
        let (name, version, pinned) = if text.contains('@') {
            let (name, version) = split_versioned(text)?;
            (name, version, true)
        } else {
            (text, registry.current_version(text)?, false)
        };
        let registered = current_builtin_version(name).is_none()
            && is_analyzer_name(name)
            && registry.get(name, version).is_some();
        registered.then(|| Self {
            name: name.to_owned(),
            version,
            pinned,
        })
    }

    /// How a schema the engine stored reads `text` (SPEC §6.2.2, §8.7), with no registry: a built-in version this
    /// build has, a well-formed version of a built-in name it lacks (a newer build's), any other well-formed
    /// `name@version` (a registered analyzer, which the database open checks against its registry), or malformed.
    pub(crate) fn read_stored(text: &str) -> StoredAnalyzer {
        let Some((name, version)) = split_versioned(text) else {
            return StoredAnalyzer::Malformed;
        };
        if current_builtin_version(name).is_some() {
            return Self::new(name, version)
                .map_or(StoredAnalyzer::Unsupported, StoredAnalyzer::Known);
        }
        StoredAnalyzer::Known(Self {
            name: name.to_owned(),
            version,
            pinned: true,
        })
    }

    /// Whether `text` is a well-formed `"name@version"` (a name of built-in shape, a canonical version from 1), whether or
    /// not this build has that analyzer version. Builds before registered analyzers (ROADMAP 6.6.8) read a stored
    /// schema naming such a version they lack as written by a newer build: unsupported, not corrupt.
    #[cfg(test)]
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

    /// Whether the analyzer is a built-in one (SPEC §6.3.3) rather than one registered with an [`AnalyzerRegistry`].
    pub fn is_builtin(&self) -> bool {
        current_builtin_version(&self.name).is_some()
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

/// [`AnalyzerRef::read_stored`]'s reading of a stored `analyzer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoredAnalyzer {
    /// A built-in version this build has, or a registered analyzer (not checked against any registry).
    Known(AnalyzerRef),
    /// A built-in name at a version this build lacks: written by a newer build.
    Unsupported,
    /// Not a well-formed `name@version`: corruption.
    Malformed,
}

/// `"name@version"` split into a name of analyzer shape ([`is_analyzer_name`]) and a version written in canonical
/// decimal (no sign or leading zero, so from 1) that fits a `u32`.
fn split_versioned(text: &str) -> Option<(&str, u32)> {
    let (name, version) = text.split_once('@')?;
    let canonical = is_analyzer_name(name)
        && !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit())
        && !version.starts_with('0');
    if !canonical {
        return None;
    }
    Some((name, version.parse().ok()?))
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

    fn registry() -> AnalyzerRegistry {
        let mut registry = AnalyzerRegistry::new();
        for version in [1, 3] {
            let analyzer =
                persia_analysis::Analyzer::builder(persia_analysis::WhitespaceTokenizer).build();
            registry.register("my-tags_2", version, analyzer).unwrap();
        }
        registry
    }

    /// ROADMAP 6.6.8: `parse_in` resolves a registered analyzer as `"name"` (its highest version, not pinned) or
    /// `"name@version"` (pinned), and a built-in name only to a built-in version, whatever the registry holds.
    #[test]
    fn parse_in_resolves_registered_analyzers() {
        let registry = registry();
        let current = AnalyzerRef::parse_in("my-tags_2", &registry).unwrap();
        assert_eq!(current.to_string(), "my-tags_2@3");
        assert!(!current.is_pinned() && !current.is_builtin());
        let first = AnalyzerRef::parse_in("my-tags_2@1", &registry).unwrap();
        assert!(first.is_pinned() && first.version() == 1);
        assert_ne!(first, current);
        for missing in [
            "my-tags_2@2",
            "my-tags_2@01",
            "My-tags_2",
            "other",
            "other@1",
            "es@2",
            "",
            "@1",
        ] {
            assert_eq!(
                AnalyzerRef::parse_in(missing, &registry),
                None,
                "{missing:?}"
            );
        }
        // Built-ins resolve through `parse_in` with any registry, an empty one included.
        for text in ["es", "es@1", "standard"] {
            let empty = AnalyzerRegistry::new();
            assert_eq!(
                AnalyzerRef::parse_in(text, &empty),
                AnalyzerRef::parse(text)
            );
            assert!(AnalyzerRef::parse_in(text, &registry).unwrap().is_builtin());
        }
        // `parse` stays built-in only.
        assert_eq!(AnalyzerRef::parse("my-tags_2@1"), None);
    }

    /// ROADMAP 6.6.8, SPEC §6.2.2: how a stored `analyzer` reads without a registry.
    #[test]
    fn read_stored_classifies_builtin_registered_and_malformed() {
        assert_eq!(
            AnalyzerRef::read_stored("es@1"),
            StoredAnalyzer::Known(AnalyzerRef::new("es", 1).unwrap())
        );
        for unsupported in ["es@2", "standard@4294967295"] {
            assert_eq!(
                AnalyzerRef::read_stored(unsupported),
                StoredAnalyzer::Unsupported,
                "{unsupported}"
            );
        }
        for registered in ["my-tags_2@3", "nl@1", "x@4294967295"] {
            let StoredAnalyzer::Known(a) = AnalyzerRef::read_stored(registered) else {
                panic!("{registered}")
            };
            assert_eq!(a.to_string(), registered);
            assert!(a.is_pinned() && !a.is_builtin());
        }
        for malformed in [
            "",
            "es",
            "mine",
            "ES@1",
            "Mine@1",
            "mine@0",
            "mine@01",
            "mine@",
            "@1",
            "mine@1@1",
            "mine@4294967296",
            "2x@1",
            "my tags@1",
        ] {
            assert_eq!(
                AnalyzerRef::read_stored(malformed),
                StoredAnalyzer::Malformed,
                "{malformed:?}"
            );
        }
    }

    proptest::proptest! {
        /// ROADMAP 6.6.8: every name `register` accepts, at every version, reads as a registered analyzer here and as
        /// *unsupported* by a build from before registered analyzers, whose rule was "not a built-in version
        /// (`AnalyzerRef::parse`), but well-formed" (SPEC §6.2.2): never as corruption. A name `register` refuses
        /// is never written, since no registry holds it.
        #[test]
        fn registered_names_read_as_unsupported_by_older_builds(
            name in "[a-zA-Z0-9_@. -]{0,6}|en|es|standard|nl",
            version in 1u32..,
        ) {
            let mut registry = AnalyzerRegistry::new();
            let analyzer =
                persia_analysis::Analyzer::builder(persia_analysis::WhitespaceTokenizer).build();
            if registry.register(&name, version, analyzer).is_ok() {
                let text = format!("{name}@{version}");
                proptest::prop_assert!(AnalyzerRef::parse(&text).is_none(), "{}", text);
                proptest::prop_assert!(AnalyzerRef::is_well_formed_versioned(&text), "{}", text);
                let StoredAnalyzer::Known(read) = AnalyzerRef::read_stored(&text) else {
                    return Err(proptest::test_runner::TestCaseError::fail(text));
                };
                proptest::prop_assert_eq!(AnalyzerRef::parse_in(&text, &registry), Some(read));
            }
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

    /// SPEC §6.2.2's defaults of a `text` field. The nDCG gate (ROADMAP 7.8.4) ranks by them but sees only a change
    /// that reorders a judged top 10: a `k1` of 1.3 moves none (found by its mutation check).
    #[test]
    fn text_defaults_are_the_specs() {
        let d = TextOptions::default();
        assert_eq!(d.analyzer, AnalyzerRef::parse("standard").unwrap());
        assert_eq!(d.boost.to_bits(), 1.0_f64.to_bits());
        assert!(!d.positions);
        assert_eq!(d.k1.to_bits(), 1.2_f64.to_bits());
        assert_eq!(d.b.to_bits(), 0.75_f64.to_bits());
        assert!(d.stored);
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
