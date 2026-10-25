//! Analyzers: char filters → tokenizer → token filters (SPEC §6.3).

use std::borrow::Cow;
use std::fmt;

use crate::token::{Token, TokenFilter, Tokenizer};

/// Rewrites the text of a field value before it is tokenized (SPEC §6.3).
///
/// Object safe, so an analyzer can chain any char filters behind pointers. Until char filters record an offset
/// map (ROADMAP 4.4.7), the spans of an analyzer whose char filters change the text index the filtered text, not
/// the original value; no built-in analyzer has a char filter.
pub trait CharFilter: Send + Sync {
    /// The filtered text, borrowing `text` when the filter leaves it unchanged. Never fails or panics.
    fn filter<'a>(&self, text: &'a str) -> Cow<'a, str>;
}

/// A text analysis pipeline: char filters, then a tokenizer, then token filters (SPEC §6.3).
///
/// Built with [`Analyzer::builder`]; the built-in analyzers come from [`builtin_analyzer`](crate::builtin_analyzer)
/// or an [`AnalyzerRegistry`](crate::AnalyzerRegistry). Indexing uses [`analyze`](Self::analyze); queries and
/// synonym terms use [`analyze_query`](Self::analyze_query), which leaves out the filters that stack tokens
/// (SPEC §6.3.4).
///
/// ```
/// use persia_analysis::{Analyzer, LowercaseFilter, StandardTokenizer};
///
/// let analyzer = Analyzer::builder(StandardTokenizer).filter(LowercaseFilter).build();
/// let mut tokens = Vec::new();
/// analyzer.analyze("Hello, World!", &mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["hello", "world"]);
/// ```
pub struct Analyzer {
    char_filters: Vec<Box<dyn CharFilter>>,
    tokenizer: Box<dyn Tokenizer>,
    filters: Vec<Box<dyn TokenFilter>>,
}

impl Analyzer {
    /// Starts an analyzer that tokenizes with `tokenizer`.
    pub fn builder(tokenizer: impl Tokenizer + 'static) -> AnalyzerBuilder {
        AnalyzerBuilder {
            analyzer: Self {
                char_filters: Vec::new(),
                tokenizer: Box::new(tokenizer),
                filters: Vec::new(),
            },
        }
    }

    /// Replaces the contents of `out` with the index-time tokens of `text`: every filter applies.
    pub fn analyze(&self, text: &str, out: &mut Vec<Token>) {
        self.run(text, out, |_| true);
    }

    /// Replaces the contents of `out` with the query-time tokens of `text`: the filters that stack tokens
    /// ([`TokenFilter::stacks_tokens`]) are left out, so a query term matches the stacked index terms as is
    /// (SPEC §6.3.4). The same as [`analyze`](Self::analyze) for an analyzer without such filters.
    pub fn analyze_query(&self, text: &str, out: &mut Vec<Token>) {
        self.run(text, out, |filter| !filter.stacks_tokens());
    }

    /// Whether any token filter stacks tokens, so query-time analysis differs from index-time analysis.
    pub fn stacks_tokens(&self) -> bool {
        self.filters.iter().any(|filter| filter.stacks_tokens())
    }

    fn run(&self, text: &str, out: &mut Vec<Token>, applies: impl Fn(&dyn TokenFilter) -> bool) {
        out.clear();
        let mut filtered = Cow::Borrowed(text);
        for char_filter in &self.char_filters {
            let changed = match char_filter.filter(&filtered) {
                Cow::Owned(text) => Some(text),
                Cow::Borrowed(_) => None,
            };
            if let Some(text) = changed {
                filtered = Cow::Owned(text);
            }
        }
        self.tokenizer.tokenize(&filtered, out);
        for filter in &self.filters {
            if applies(filter.as_ref()) {
                filter.apply(out);
            }
        }
    }
}

impl fmt::Debug for Analyzer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Analyzer")
            .field("char_filters", &self.char_filters.len())
            .field("filters", &self.filters.len())
            .finish_non_exhaustive()
    }
}

/// Builds an [`Analyzer`]: char filters and token filters run in the order they are added.
#[must_use]
pub struct AnalyzerBuilder {
    analyzer: Analyzer,
}

impl AnalyzerBuilder {
    /// Appends a char filter; char filters run before the tokenizer, whenever they are added.
    pub fn char_filter(mut self, filter: impl CharFilter + 'static) -> Self {
        self.analyzer.char_filters.push(Box::new(filter));
        self
    }

    /// Appends a token filter.
    pub fn filter(mut self, filter: impl TokenFilter + 'static) -> Self {
        self.analyzer.filters.push(Box::new(filter));
        self
    }

    /// The analyzer.
    pub fn build(self) -> Analyzer {
        self.analyzer
    }
}

impl fmt::Debug for AnalyzerBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AnalyzerBuilder")
            .field(&self.analyzer)
            .finish()
    }
}
