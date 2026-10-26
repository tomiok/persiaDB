//! Stemmers and the stemming filter (SPEC §6.3.2).
//!
//! The stemmers implement the Snowball algorithms (snowballstem.org) for English (Porter2), Spanish, Portuguese,
//! French, German and Italian as of Snowball v3.1.1, written from their published definitions; the Snowball test vocabularies (snowball-data
//! commit `019921c3a41c2b70063ee3feb215a00c8e594597`) pin their output.

mod english;
mod french;
mod german;
mod italian;
mod portuguese;
mod spanish;
mod word;

use std::borrow::Cow;
use std::sync::Arc;

pub use english::EnglishStemmer;
pub use french::FrenchStemmer;
pub use german::GermanStemmer;
pub use italian::ItalianStemmer;
pub use portuguese::PortugueseStemmer;
pub use spanish::SpanishStemmer;

use crate::scratch::Scratch;
use crate::token::TokenFilter;
use crate::token_stream::TokenStream;

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

    /// Writes the stem of `word` to the empty `out` and returns `true`, or returns `false` (leaving `out` empty) when
    /// `word` is its own stem. `scratch` holds reusable buffers, so the built-in stemmers allocate nothing here once
    /// they have grown (ROADMAP 4.4.12); [`StemmerFilter`] calls this. The default goes through
    /// [`stem`](Self::stem).
    fn stem_into(&self, word: &str, _scratch: &mut Scratch, out: &mut String) -> bool {
        match self.stem(word) {
            Cow::Owned(stem) if stem != word => {
                out.push_str(&stem);
                true
            }
            // A stemmer may return a borrowed stem that is not the word (a `'static` table entry).
            Cow::Borrowed(stem) if stem != word => {
                out.push_str(stem);
                true
            }
            _ => false,
        }
    }
}

macro_rules! forward_stemmer {
    ($($t:ty),*) => {$(
        impl<S: Stemmer + ?Sized> Stemmer for $t {
            fn stem<'a>(&self, word: &'a str) -> Cow<'a, str> {
                (**self).stem(word)
            }

            fn stem_into(&self, word: &str, scratch: &mut Scratch, out: &mut String) -> bool {
                (**self).stem_into(word, scratch, out)
            }
        }
    )*};
}

forward_stemmer!(&S, Box<S>, Arc<S>);

/// A built-in stemmer: it stems `word` into `scratch.chars`, using `scratch.work` for steps that rebuild the word.
pub(crate) trait CharStemmer {
    fn stem_chars(&self, word: &str, scratch: &mut Scratch);
}

/// [`Stemmer::stem`] of a built-in, with fresh buffers.
fn stem_with_fresh_buffers<'a>(stemmer: &impl CharStemmer, word: &'a str) -> Cow<'a, str> {
    let mut scratch = Scratch::new();
    let mut out = String::new();
    if stem_into_buffers(stemmer, word, &mut scratch, &mut out) {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(word)
    }
}

/// [`Stemmer::stem_into`] of a built-in.
fn stem_into_buffers(
    stemmer: &impl CharStemmer,
    word: &str,
    scratch: &mut Scratch,
    out: &mut String,
) -> bool {
    stemmer.stem_chars(word, scratch);
    let changed = !word.chars().eq(scratch.chars.iter().copied());
    if changed {
        out.extend(scratch.chars.iter());
    }
    scratch.reset();
    changed
}

/// Implements [`Stemmer`] for built-ins through [`CharStemmer`].
macro_rules! builtin_stemmer {
    ($($t:ty),*) => {$(
        impl Stemmer for $t {
            fn stem<'a>(&self, word: &'a str) -> Cow<'a, str> {
                stem_with_fresh_buffers(self, word)
            }

            fn stem_into(&self, word: &str, scratch: &mut Scratch, out: &mut String) -> bool {
                stem_into_buffers(self, word, scratch, out)
            }
        }
    )*};
}

builtin_stemmer!(
    EnglishStemmer,
    SpanishStemmer,
    PortugueseStemmer,
    FrenchStemmer,
    GermanStemmer,
    ItalianStemmer
);

/// Replaces each token's text with its stem (SPEC §6.3.2), keeping its span and position.
///
/// The stemmers expect lowercase text with its accents, so the filter runs after
/// [`LowercaseFilter`](crate::LowercaseFilter) and [`StopwordFilter`](crate::StopwordFilter) and before
/// [`FoldingFilter`](crate::FoldingFilter). A token whose stem is empty is removed.
///
/// ```
/// use persia_analysis::{TokenStream, EnglishStemmer, StemmerFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = TokenStream::new();
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
    fn apply(&self, tokens: &mut TokenStream) {
        // An empty stem empties the token, which `rewrite_with_scratch` removes.
        tokens
            .rewrite_with_scratch(|text, out, scratch| self.stemmer.stem_into(text, scratch, out));
    }
}
