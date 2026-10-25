//! Stemmers and the stemming filter (SPEC §6.3.2).
//!
//! The stemmers implement the Snowball algorithms (snowballstem.org) for English (Porter2), Spanish and Portuguese
//! as of Snowball 3, written from their published definitions; the Snowball test vocabularies pin their output.

mod english;
mod portuguese;
mod spanish;
mod word;

use std::borrow::Cow;

pub use english::EnglishStemmer;
pub use portuguese::PortugueseStemmer;
pub use spanish::SpanishStemmer;

use crate::token::{Token, TokenFilter};

/// Reduces a word to its stem (SPEC §6.3.2).
///
/// A stemmer never fails or panics, on any input. Its output is not a dictionary word, only a key that the
/// inflections of one word share. Stemming is not idempotent: a stem may stem further
/// (`accelerated` → `acceler` → `accel` in English), so query terms and indexed terms must go through the same
/// analyzer. Object safe, so an analyzer can hold any stemmer behind a pointer.
pub trait Stemmer: Send + Sync {
    /// The stem of `word`, borrowing `word` when it is its own stem.
    fn stem<'a>(&self, word: &'a str) -> Cow<'a, str>;
}

/// Replaces each token's text with its stem (SPEC §6.3.2), keeping its span and position.
///
/// The stemmers expect lowercase text with its accents, so the filter runs after
/// [`LowercaseFilter`](crate::LowercaseFilter) and [`StopwordFilter`](crate::StopwordFilter) and before
/// [`FoldingFilter`](crate::FoldingFilter). A token whose stem is empty is removed.
///
/// ```
/// use persia_analysis::{EnglishStemmer, StemmerFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = Vec::new();
/// WhitespaceTokenizer.tokenize("connected connecting connections", &mut tokens);
/// StemmerFilter::new(EnglishStemmer).apply(&mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["connect", "connect", "connect"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StemmerFilter<S> {
    stemmer: S,
}

impl<S: Stemmer> StemmerFilter<S> {
    /// A filter stemming with `stemmer`.
    pub fn new(stemmer: S) -> Self {
        Self { stemmer }
    }
}

impl<S: Stemmer> TokenFilter for StemmerFilter<S> {
    fn apply(&self, tokens: &mut Vec<Token>) {
        for token in tokens.iter_mut() {
            let stem = match self.stemmer.stem(&token.text) {
                Cow::Borrowed(_) => continue,
                Cow::Owned(stem) => stem,
            };
            token.text = stem;
        }
        tokens.retain(|token| !token.text.is_empty());
    }
}
