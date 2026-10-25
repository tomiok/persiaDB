//! Text analysis for Persia DB: tokenizers, normalizers, stemmers, analyzers (SPEC §6.3).
//!
//! A tokenizer splits text into [`Token`]s that carry their byte span in the source text and their position in
//! the token stream (SPEC §6.3.1):
//!
//! ```
//! use persia_analysis::{StandardTokenizer, Tokenizer};
//!
//! let mut tokens = Vec::new();
//! StandardTokenizer.tokenize("Don't panic, 42!", &mut tokens);
//! let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
//! assert_eq!(texts, ["Don't", "panic", "42"]);
//! assert_eq!((tokens[1].start, tokens[1].end, tokens[1].position), (6, 11, 1));
//! ```
#![forbid(unsafe_code)]
// Offsets come from user text: arithmetic must be checked (ROADMAP 1.1.5).
#![deny(clippy::arithmetic_side_effects)]
#![cfg_attr(test, allow(clippy::arithmetic_side_effects))]

mod fold;
mod length;
mod lowercase;
mod nfkc;
mod ngram;
mod standard;
mod stem;
mod stop;
mod synonyms;
mod token;
mod whitespace;

pub use fold::FoldingFilter;
pub use length::{DEFAULT_MAX_TOKEN_LEN, LengthPolicy, TokenLengthGuard};
pub use lowercase::LowercaseFilter;
pub use nfkc::NfkcFilter;
pub use ngram::EdgeNgramFilter;
pub use standard::StandardTokenizer;
pub use stem::{EnglishStemmer, PortugueseStemmer, SpanishStemmer, Stemmer, StemmerFilter};
pub use stop::{ENGLISH_STOPWORDS, PORTUGUESE_STOPWORDS, SPANISH_STOPWORDS, StopwordFilter};
pub use synonyms::{DEFAULT_SYNONYM_EXPANSIONS, SynonymMap, SynonymMatch};
pub use token::{Token, TokenFilter, Tokenizer};
pub use whitespace::{KeywordTokenizer, WhitespaceTokenizer};

/// The Unicode version whose tables define tokenization and token filters (SPEC §6.3.1, §6.3.2): the UAX #29 word
/// boundaries of `unicode-segmentation`, the normalization tables of `unicode-normalization` and the `char`
/// properties and case mappings of the standard library must all be at this version, so a dependency or toolchain
/// update cannot silently change what an analyzer version produces.
pub const UNICODE_VERSION: (u8, u8, u8) = (17, 0, 0);

#[cfg(test)]
mod tests {
    use super::UNICODE_VERSION;

    /// Fails on a `unicode-segmentation` or toolchain update that moves to another Unicode version: bump
    /// `UNICODE_VERSION` deliberately, together with the versions of the analyzers it changes (SPEC §6.3.1).
    #[test]
    fn unicode_tables_are_at_the_pinned_version() {
        let (major, minor, update) = UNICODE_VERSION;
        let pinned = (u64::from(major), u64::from(minor), u64::from(update));
        assert_eq!(
            unicode_segmentation::UNICODE_VERSION,
            pinned,
            "unicode-segmentation tables"
        );
        assert_eq!(
            unicode_normalization::UNICODE_VERSION,
            UNICODE_VERSION,
            "unicode-normalization tables"
        );
        assert_eq!(
            char::UNICODE_VERSION,
            UNICODE_VERSION,
            "standard library char tables"
        );
    }
}
