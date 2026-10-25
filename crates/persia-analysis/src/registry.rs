//! The built-in analyzers and the registry that looks analyzers up by name and version (SPEC §6.3, §6.3.3).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::analyzer::Analyzer;
use crate::{
    ApostropheFilter, EnglishStemmer, FoldingFilter, KeywordTokenizer, LowercaseFilter, NfkcFilter,
    PortugueseStemmer, SpanishStemmer, StandardTokenizer, StemmerFilter, StopwordFilter,
    TokenLengthGuard, WhitespaceTokenizer,
};

/// A built-in analyzer version (SPEC §6.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BuiltinAnalyzer {
    /// The analyzer's name, as a schema names it.
    pub name: &'static str,
    /// The version; versions start at 1.
    pub version: u32,
}

/// Every built-in analyzer version, ascending by name, then version. A version, once released, is never removed or
/// changed: an index built with it must analyze queries the same way forever (SPEC §6.3).
pub const BUILTIN_ANALYZERS: &[BuiltinAnalyzer] = &[
    BuiltinAnalyzer {
        name: "en",
        version: 1,
    },
    BuiltinAnalyzer {
        name: "es",
        version: 1,
    },
    BuiltinAnalyzer {
        name: "keyword",
        version: 1,
    },
    BuiltinAnalyzer {
        name: "pt",
        version: 1,
    },
    BuiltinAnalyzer {
        name: "standard",
        version: 1,
    },
    BuiltinAnalyzer {
        name: "whitespace",
        version: 1,
    },
];

/// The analyzer a text field gets when it names none (SPEC §6.2.2).
pub const DEFAULT_ANALYZER: &str = "standard";

/// The current (highest) version of built-in analyzer `name`, or `None` if there is no such analyzer.
pub fn current_builtin_version(name: &str) -> Option<u32> {
    BUILTIN_ANALYZERS
        .iter()
        .filter(|a| a.name == name)
        .map(|a| a.version)
        .max()
}

/// Version `version` of built-in analyzer `name` (SPEC §6.3.3), or `None` if it does not exist.
///
/// ```
/// use persia_analysis::builtin_analyzer;
///
/// let es = builtin_analyzer("es", 1).ok_or("no es@1")?;
/// let mut tokens = Vec::new();
/// es.analyze("Los años de las canciones", &mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["años", "cancion"]);
/// # Ok::<(), &str>(())
/// ```
pub fn builtin_analyzer(name: &str, version: u32) -> Option<Analyzer> {
    // Each arm is one released version: never edit it, add a new version instead (SPEC §6.3).
    let analyzer = match (name, version) {
        ("standard", 1) => Analyzer::builder(StandardTokenizer)
            .filter(NfkcFilter)
            .filter(LowercaseFilter)
            .filter(TokenLengthGuard::default()),
        ("keyword", 1) => Analyzer::builder(KeywordTokenizer).filter(TokenLengthGuard::default()),
        ("whitespace", 1) => {
            Analyzer::builder(WhitespaceTokenizer).filter(TokenLengthGuard::default())
        }
        ("en", 1) => Analyzer::builder(StandardTokenizer)
            .filter(NfkcFilter)
            .filter(LowercaseFilter)
            .filter(ApostropheFilter)
            .filter(StopwordFilter::english())
            .filter(StemmerFilter::new(EnglishStemmer))
            .filter(FoldingFilter::default())
            .filter(TokenLengthGuard::default()),
        ("es", 1) => Analyzer::builder(StandardTokenizer)
            .filter(NfkcFilter)
            .filter(LowercaseFilter)
            .filter(StopwordFilter::spanish())
            .filter(StemmerFilter::new(SpanishStemmer))
            .filter(FoldingFilter::preserving(['ñ']))
            .filter(TokenLengthGuard::default()),
        ("pt", 1) => Analyzer::builder(StandardTokenizer)
            .filter(NfkcFilter)
            .filter(LowercaseFilter)
            .filter(StopwordFilter::portuguese())
            .filter(StemmerFilter::new(PortugueseStemmer))
            .filter(FoldingFilter::default())
            .filter(TokenLengthGuard::default()),
        _ => return None,
    };
    Some(analyzer.build())
}

/// Analyzers by name and version (SPEC §6.3).
///
/// A schema names an analyzer as `name@version` (SPEC §6.2.2); the registry resolves that name to the pipeline, so
/// every segment of a field is indexed, and every query on it analyzed, by the same version.
///
/// ```
/// use persia_analysis::AnalyzerRegistry;
///
/// let registry = AnalyzerRegistry::builtin();
/// assert_eq!(registry.current_version("en"), Some(1));
/// assert!(registry.get("en", 1).is_some());
/// assert!(registry.get("en", 2).is_none());
/// ```
#[derive(Debug, Clone, Default)]
pub struct AnalyzerRegistry {
    analyzers: BTreeMap<(String, u32), Arc<Analyzer>>,
}

/// [`AnalyzerRegistry::register`] refused an analyzer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterError {
    /// The name is empty or contains `@`, so no `name@version` could refer to it.
    #[error("invalid analyzer name {0:?}")]
    InvalidName(String),
    /// Versions start at 1.
    #[error("analyzer {0:?}: version 0 is invalid")]
    InvalidVersion(String),
    /// The registry already has this name and version; a version never changes once used.
    #[error("analyzer {name}@{version} is already registered")]
    Exists {
        /// The analyzer's name.
        name: String,
        /// The version.
        version: u32,
    },
}

impl AnalyzerRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry holding every version of every built-in analyzer ([`BUILTIN_ANALYZERS`]).
    pub fn builtin() -> Self {
        let analyzers = BUILTIN_ANALYZERS
            .iter()
            .filter_map(|a| {
                let analyzer = builtin_analyzer(a.name, a.version)?;
                Some(((a.name.to_owned(), a.version), Arc::new(analyzer)))
            })
            .collect();
        Self { analyzers }
    }

    /// Adds `analyzer` as version `version` of `name`.
    ///
    /// # Errors
    /// [`RegisterError`] if the name is empty or contains `@`, the version is 0, or the name and version are taken.
    pub fn register(
        &mut self,
        name: &str,
        version: u32,
        analyzer: Analyzer,
    ) -> Result<(), RegisterError> {
        if name.is_empty() || name.contains('@') {
            return Err(RegisterError::InvalidName(name.to_owned()));
        }
        if version == 0 {
            return Err(RegisterError::InvalidVersion(name.to_owned()));
        }
        let key = (name.to_owned(), version);
        if self.analyzers.contains_key(&key) {
            return Err(RegisterError::Exists {
                name: name.to_owned(),
                version,
            });
        }
        self.analyzers.insert(key, Arc::new(analyzer));
        Ok(())
    }

    /// Version `version` of `name`, if registered.
    pub fn get(&self, name: &str, version: u32) -> Option<&Arc<Analyzer>> {
        self.analyzers.get(&(name.to_owned(), version))
    }

    /// The highest registered version of `name`.
    pub fn current_version(&self, name: &str) -> Option<u32> {
        self.versions(name).last()
    }

    /// The registered versions of `name`, ascending.
    pub fn versions<'a>(&'a self, name: &'a str) -> impl Iterator<Item = u32> + 'a {
        self.analyzers
            .keys()
            .filter(move |(n, _)| n == name)
            .map(|(_, version)| *version)
    }
}
