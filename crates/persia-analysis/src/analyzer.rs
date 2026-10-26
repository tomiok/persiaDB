//! Analyzers: char filters → tokenizer → token filters (SPEC §6.3).

use std::borrow::Cow;
use std::fmt;

use crate::char_filter::{CharFilter, CharFilterOutput, OffsetMap};
use crate::token::{Token, TokenFilter, Tokenizer};
use crate::token_stream::TokenStream;

/// A text analysis pipeline: char filters, then a tokenizer, then token filters (SPEC §6.3).
///
/// Built with [`Analyzer::builder`]; the built-in analyzers come from [`builtin_analyzer`](crate::builtin_analyzer)
/// or an [`AnalyzerRegistry`](crate::AnalyzerRegistry). Indexing uses [`analyze`](Self::analyze); queries and
/// synonym terms use [`analyze_query`](Self::analyze_query), which leaves out the filters that stack tokens
/// (SPEC §6.3.4).
///
/// ```
/// use persia_analysis::{TokenStream, Analyzer, LowercaseFilter, StandardTokenizer};
///
/// let analyzer = Analyzer::builder(StandardTokenizer).filter(LowercaseFilter).build();
/// let mut tokens = TokenStream::new();
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
    ///
    /// Token spans are byte offsets of `text`, also when char filters changed it (SPEC §6.3.1).
    pub fn analyze(&self, text: &str, out: &mut TokenStream) {
        self.run(text, out, |_| true);
    }

    /// Replaces the contents of `out` with the query-time tokens of `text`: the filters that stack tokens
    /// ([`TokenFilter::stacks_tokens`]) are left out, so a query term matches the stacked index terms as is
    /// (SPEC §6.3.4). The same as [`analyze`](Self::analyze) for an analyzer without such filters.
    pub fn analyze_query(&self, text: &str, out: &mut TokenStream) {
        self.run(text, out, |filter| !filter.stacks_tokens());
    }

    /// Whether any token filter stacks tokens, so query-time analysis differs from index-time analysis.
    pub fn stacks_tokens(&self) -> bool {
        self.filters.iter().any(|filter| filter.stacks_tokens())
    }

    fn run(&self, text: &str, out: &mut TokenStream, applies: impl Fn(&dyn TokenFilter) -> bool) {
        out.clear();
        let mut filtered = Cow::Borrowed(text);
        // One map per char filter that changed the text, in chain order.
        let mut maps: Vec<OffsetMap> = Vec::new();
        for char_filter in &self.char_filters {
            let mut output = CharFilterOutput::new(&filtered);
            char_filter.filter(&mut output);
            if let Some((text, map)) = output.finish() {
                filtered = Cow::Owned(text);
                maps.push(map);
            }
        }
        self.tokenizer.tokenize(&filtered, out);
        if !maps.is_empty() {
            map_spans(&maps, text.len(), out);
        }
        for filter in &self.filters {
            if applies(filter.as_ref()) {
                filter.apply(out);
            }
        }
    }
}

/// Maps the spans of `tokens`, offsets of the last char filter's output, back to the original value of `len` bytes
/// through `maps`, the last filter's map first (SPEC §6.3.1).
fn map_spans(maps: &[OffsetMap], len: usize, tokens: &mut [Token]) {
    for token in tokens {
        let start = maps.iter().rev().fold(token.start, |o, map| map.start(o));
        let end = maps.iter().rev().fold(token.end, |o, map| map.end(o));
        token.end = end.min(len);
        token.start = start.min(token.end);
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
    /// Appends a char filter; char filters run before the tokenizer, in the order they are added, whenever that is.
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
