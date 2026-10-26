//! Lowercasing filter (SPEC §6.3.2).

use crate::token::TokenFilter;
use crate::token_stream::TokenStream;

/// Lowercases each token's text with the Unicode default, locale-independent mapping (SPEC §6.3.2).
///
/// This is lowercasing, not case folding: `ß` stays `ß` and `ẞ` becomes `ß` ([`FoldingFilter`](crate::FoldingFilter)
/// maps both to `ss`). A final capital sigma becomes `ς` (`Final_Sigma`, judged within the token). Turkish and Azeri
/// casing is not tailored: `I` becomes `i`, `İ` becomes `i` followed by U+0307 COMBINING DOT ABOVE, and `ı` is kept.
///
/// ```
/// use persia_analysis::{TokenStream, LowercaseFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = TokenStream::new();
/// WhitespaceTokenizer.tokenize("Ñandú STRAẞE ΟΔΟΣ", &mut tokens);
/// LowercaseFilter.apply(&mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["ñandú", "straße", "οδος"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LowercaseFilter;

impl TokenFilter for LowercaseFilter {
    fn apply(&self, tokens: &mut TokenStream) {
        let mut non_ascii = false;
        for token in tokens.iter_mut() {
            if token.text.is_ascii() {
                token.text.make_ascii_lowercase();
            } else {
                non_ascii = true;
            }
        }
        if !non_ascii {
            // Nothing left to rewrite; empty tokens are still removed, as `rewrite` would.
            tokens.retain(|token| !token.text.is_empty());
            return;
        }
        tokens.rewrite(|text, out| {
            // Text made only of lowercase letters maps to itself, so it keeps its string.
            if text.is_ascii() || text.chars().all(char::is_lowercase) {
                return false;
            }
            if text.contains('Σ') {
                // `str::to_lowercase` applies Final_Sigma, which needs the context of the whole token.
                out.push_str(&text.to_lowercase());
            } else {
                // Without `Σ`, `str::to_lowercase` is exactly the mapping of each character.
                out.extend(text.chars().flat_map(char::to_lowercase));
            }
            true
        });
    }
}
