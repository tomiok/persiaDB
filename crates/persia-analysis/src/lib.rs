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

mod length;
mod standard;
mod token;
mod whitespace;

pub use length::{DEFAULT_MAX_TOKEN_LEN, LengthPolicy, TokenLengthGuard};
pub use standard::StandardTokenizer;
pub use token::{Token, Tokenizer};
pub use whitespace::{KeywordTokenizer, WhitespaceTokenizer};
