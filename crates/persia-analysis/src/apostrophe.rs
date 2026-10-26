//! Apostrophe normalization filter (SPEC §6.3.2).

use crate::token::TokenFilter;
use crate::token_stream::TokenStream;

/// Replaces the typographic apostrophes `‘` (U+2018) and `’` (U+2019) with `'` (U+0027), the only apostrophe the
/// English stemmer knows (SPEC §6.3.2).
///
/// UAX #29 joins letters across all three (`don’t` is one token), so without this filter `don’t` and `don't` would
/// be different terms. Spans and positions are kept.
///
/// ```
/// use persia_analysis::{TokenStream, ApostropheFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = TokenStream::new();
/// WhitespaceTokenizer.tokenize("don’t", &mut tokens);
/// ApostropheFilter.apply(&mut tokens);
/// assert_eq!(tokens[0].text, "don't");
/// assert_eq!(tokens[0].end, 7); // `’` is 3 bytes in the source
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApostropheFilter;

const fn is_typographic_apostrophe(c: char) -> bool {
    matches!(c, '\u{2018}' | '\u{2019}')
}

impl TokenFilter for ApostropheFilter {
    fn apply(&self, tokens: &mut TokenStream) {
        tokens.rewrite(|text, out| {
            if !text.contains(is_typographic_apostrophe) {
                return false;
            }
            out.extend(text.chars().map(|c| {
                if is_typographic_apostrophe(c) {
                    '\''
                } else {
                    c
                }
            }));
            true
        });
    }
}
