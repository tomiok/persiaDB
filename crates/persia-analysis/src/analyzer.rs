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

    /// As [`analyze_query`](Self::analyze_query), and whether a filter that removes stopwords
    /// ([`TokenFilter::removes_stopwords`]) removed a token: `the` in an `en` field, not `—`, in which the tokenizer
    /// finds no word.
    ///
    /// ```
    /// use persia_analysis::{builtin_analyzer, TokenStream};
    ///
    /// let en = builtin_analyzer("en", 1).ok_or("no en@1")?;
    /// let mut tokens = TokenStream::new();
    /// assert!(en.analyze_query_stopwords("The,", &mut tokens) && tokens.is_empty());
    /// assert!(!en.analyze_query_stopwords("—", &mut tokens) && tokens.is_empty());
    /// assert!(!en.analyze_query_stopwords("shoes", &mut tokens) && !tokens.is_empty());
    /// # Ok::<(), &str>(())
    /// ```
    pub fn analyze_query_stopwords(&self, text: &str, out: &mut TokenStream) -> bool {
        self.analyze_query_counted(text, out).stopword
    }

    /// As [`analyze_query`](Self::analyze_query), and what else the query compiler reads of it ([`QueryAnalysis`]):
    /// whether a stopword was removed, and how many positions the text took.
    ///
    /// ```
    /// use persia_analysis::{builtin_analyzer, TokenStream};
    ///
    /// let en = builtin_analyzer("en", 1).ok_or("no en@1")?;
    /// let mut tokens = TokenStream::new();
    /// // `of` and `the` are removed, and still take their positions.
    /// let analysis = en.analyze_query_counted("statue-of-the", &mut tokens);
    /// assert!(analysis.stopword);
    /// assert_eq!((tokens.len(), analysis.positions), (1, 3));
    /// assert_eq!(en.analyze_query_counted("—", &mut tokens).positions, 0);
    /// # Ok::<(), &str>(())
    /// ```
    pub fn analyze_query_counted(&self, text: &str, out: &mut TokenStream) -> QueryAnalysis {
        self.run(text, out, |filter| !filter.stacks_tokens())
    }

    /// Whether any token filter stacks tokens, so query-time analysis differs from index-time analysis.
    pub fn stacks_tokens(&self) -> bool {
        self.filters.iter().any(|filter| filter.stacks_tokens())
    }

    /// Runs the chain, and returns whether a filter that removes stopwords removed a token and how many positions the
    /// tokenizer gave.
    fn run(
        &self,
        text: &str,
        out: &mut TokenStream,
        applies: impl Fn(&dyn TokenFilter) -> bool,
    ) -> QueryAnalysis {
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
        // Before the filters remove any: a removed token keeps its position (SPEC §6.3.1).
        let positions = out
            .iter()
            .map(|t| t.position.saturating_add(1))
            .max()
            .unwrap_or(0);
        if !maps.is_empty() {
            map_spans(&maps, text.len(), out);
        }
        let mut removed = false;
        for filter in &self.filters {
            if applies(filter.as_ref()) {
                let before = out.len();
                filter.apply(out);
                removed |= filter.removes_stopwords() && out.len() < before;
            }
        }
        QueryAnalysis {
            stopword: removed,
            positions,
        }
    }
}

/// What [`Analyzer::analyze_query_counted`] reports besides the tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct QueryAnalysis {
    /// Whether a filter that removes stopwords ([`TokenFilter::removes_stopwords`]) removed a token.
    pub stopword: bool,
    /// How many positions the text took: one past the tokenizer's last position, 0 when it found no token. Tokens the
    /// filters removed count, as they leave gaps (SPEC §6.3.1). So a caller that analyzes words one at a time, starting
    /// each at the sum of the ones before, numbers their tokens as one analysis of the words, one space apart, would
    /// under a tokenizer that splits at whitespace.
    pub positions: u32,
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
    /// Appends a char filter; char filters run before the tokenizer, in the order they are added.
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
