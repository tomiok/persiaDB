//! Unicode word segmentation tokenizer (UAX#29; SPEC §6.3.1).

use unicode_segmentation::UnicodeSegmentation;

use crate::token::{Token, Tokenizer, push_spans};

/// Splits text at Unicode word boundaries (UAX #29) and keeps the words, the tokenizer of the `standard`
/// analyzer (SPEC §6.3).
///
/// A word is a segment between two boundaries that contains at least one alphanumeric character, so spaces,
/// punctuation and symbols (emoji included) produce no token; a keycap emoji such as `1️⃣` keeps its digit and is
/// one. Following UAX #29 (SPEC §6.3.1):
///
/// - numbers with inner separators stay whole: `3.14`, `1,000`, `v2`;
/// - an apostrophe between letters joins them: `don't`, `O'Neil` (typographic `’` too);
/// - emails and URLs split at `@`, `:` and `/`, while dotted names stay whole: `user@example.com` gives `user`,
///   `example.com`;
/// - `_` joins words: `snake_case`;
/// - scripts written without spaces between words (Han, Hiragana, Thai, ...) fall back to one token per
///   character, since UAX #29 has no dictionary; Katakana runs and Hangul words stay whole.
///
/// ```
/// use persia_analysis::{StandardTokenizer, Tokenizer};
///
/// let mut tokens = Vec::new();
/// StandardTokenizer.tokenize("mail ana@example.com", &mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["mail", "ana", "example.com"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StandardTokenizer;

impl Tokenizer for StandardTokenizer {
    fn tokenize(&self, text: &str, out: &mut Vec<Token>) {
        push_spans(text.unicode_word_indices(), out);
    }
}
