//! Stemmers and the stemming filter (SPEC §6.3.2).
//!
//! The stemmers implement the Snowball algorithms (snowballstem.org) for English (Porter2), Spanish and Portuguese
//! as of Snowball v3.1.1, written from their published definitions; the Snowball test vocabularies (snowball-data
//! commit `019921c3a41c2b70063ee3feb215a00c8e594597`) pin their output.

mod english;
mod portuguese;
mod spanish;
mod word;

use std::borrow::Cow;
use std::sync::Arc;

pub use english::EnglishStemmer;
pub use portuguese::PortugueseStemmer;
pub use spanish::SpanishStemmer;

use crate::token::{Token, TokenFilter};

/// Reduces a word to its stem (SPEC §6.3.2).
///
/// A stemmer never fails or panics, on any input. Its output is not a dictionary word, only a key that the
/// inflections of one word share. Stemming is not idempotent: a stem may stem further
/// (`accelerated` → `acceler` → `accel` in English), so query terms and indexed terms must go through the same
/// analyzer. Object safe, and implemented for references, `Box` and `Arc`, so an analyzer can pick a stemmer at run
/// time (`StemmerFilter<Box<dyn Stemmer>>`).
pub trait Stemmer: Send + Sync {
    /// The stem of `word`, borrowing `word` when it is its own stem.
    fn stem<'a>(&self, word: &'a str) -> Cow<'a, str>;
}

impl<S: Stemmer + ?Sized> Stemmer for &S {
    fn stem<'a>(&self, word: &'a str) -> Cow<'a, str> {
        (**self).stem(word)
    }
}

impl<S: Stemmer + ?Sized> Stemmer for Box<S> {
    fn stem<'a>(&self, word: &'a str) -> Cow<'a, str> {
        (**self).stem(word)
    }
}

impl<S: Stemmer + ?Sized> Stemmer for Arc<S> {
    fn stem<'a>(&self, word: &'a str) -> Cow<'a, str> {
        (**self).stem(word)
    }
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
            // Compared rather than matched on `Cow::Borrowed`: a stemmer may return a borrowed stem that is not the
            // word (a `'static` table entry).
            let stem = self.stemmer.stem(&token.text);
            if stem != token.text.as_str() {
                let stem = stem.into_owned();
                token.text = stem;
            }
        }
        tokens.retain(|token| !token.text.is_empty());
    }
}
