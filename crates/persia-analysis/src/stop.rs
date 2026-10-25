//! Stopword filter and its embedded lists (SPEC §6.3.2).
//!
//! The lists are closed-class words (articles, prepositions, conjunctions, pronouns and the most common auxiliary
//! forms), lowercase NFC with their accents, sorted by bytes. They are part of the definition of every analyzer
//! that uses them: changing a list changes those analyzers' versions (SPEC §6.3).

use std::collections::HashSet;

use crate::token::{Token, TokenFilter};

/// English stopwords.
pub const ENGLISH_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

/// Spanish stopwords.
pub const SPANISH_STOPWORDS: &[&str] = &[
    "a", "al", "ante", "bajo", "como", "con", "contra", "cuando", "de", "del", "desde", "donde",
    "durante", "e", "el", "ella", "ellas", "ellos", "en", "entre", "era", "es", "esa", "esas",
    "ese", "eso", "esos", "esta", "estas", "este", "esto", "estos", "está", "fue", "ha", "han",
    "hasta", "hay", "la", "las", "le", "les", "lo", "los", "me", "mi", "mis", "muy", "más", "mí",
    "ni", "no", "nos", "nosotros", "o", "os", "para", "pero", "por", "porque", "que", "se", "ser",
    "si", "sin", "sino", "sobre", "son", "su", "sus", "sí", "también", "te", "ti", "tras", "tu",
    "tus", "tú", "u", "un", "una", "unas", "uno", "unos", "usted", "ustedes", "vosotros", "y",
    "ya", "yo", "él",
];

/// Portuguese stopwords.
pub const PORTUGUESE_STOPWORDS: &[&str] = &[
    "a", "ao", "aos", "as", "com", "como", "da", "das", "de", "do", "dos", "e", "ela", "elas",
    "ele", "eles", "em", "entre", "era", "essa", "essas", "esse", "esses", "esta", "estas", "este",
    "estes", "está", "eu", "foi", "há", "isso", "isto", "já", "lhe", "lhes", "mais", "mas", "me",
    "meu", "minha", "muito", "na", "nas", "nem", "no", "nos", "num", "numa", "nós", "o", "os",
    "ou", "para", "pela", "pelas", "pelo", "pelos", "por", "porque", "quando", "que", "se", "sem",
    "ser", "seu", "seus", "sua", "suas", "são", "também", "te", "tu", "um", "uma", "umas", "uns",
    "você", "vocês", "à", "às", "é",
];

/// Removes tokens whose text is exactly one of a list of words (SPEC §6.3.2), leaving a gap in positions.
///
/// The embedded lists are lowercase with their accents, so the filter runs after
/// [`LowercaseFilter`](crate::LowercaseFilter) and before [`FoldingFilter`](crate::FoldingFilter); a custom list is
/// matched as given.
///
/// ```
/// use persia_analysis::{StopwordFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = Vec::new();
/// WhitespaceTokenizer.tokenize("la casa de papel", &mut tokens);
/// StopwordFilter::spanish().apply(&mut tokens);
/// let kept: Vec<(&str, u32)> = tokens.iter().map(|t| (t.text.as_str(), t.position)).collect();
/// assert_eq!(kept, [("casa", 1), ("papel", 3)]);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StopwordFilter {
    words: HashSet<Box<str>>,
}

impl StopwordFilter {
    /// A filter removing the given words.
    pub fn new<W: Into<Box<str>>>(words: impl IntoIterator<Item = W>) -> Self {
        Self {
            words: words.into_iter().map(Into::into).collect(),
        }
    }

    /// The embedded English list, [`ENGLISH_STOPWORDS`].
    pub fn english() -> Self {
        Self::new(ENGLISH_STOPWORDS.iter().copied())
    }

    /// The embedded Spanish list, [`SPANISH_STOPWORDS`].
    pub fn spanish() -> Self {
        Self::new(SPANISH_STOPWORDS.iter().copied())
    }

    /// The embedded Portuguese list, [`PORTUGUESE_STOPWORDS`].
    pub fn portuguese() -> Self {
        Self::new(PORTUGUESE_STOPWORDS.iter().copied())
    }

    /// Whether `text` is one of the filter's words.
    pub fn is_stopword(&self, text: &str) -> bool {
        self.words.contains(text)
    }
}

impl TokenFilter for StopwordFilter {
    fn apply(&self, tokens: &mut Vec<Token>) {
        tokens.retain(|token| !token.text.is_empty() && !self.is_stopword(&token.text));
    }
}
