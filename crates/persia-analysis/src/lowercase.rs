//! Lowercasing filter (SPEC §6.3.2).

use crate::token::{Token, TokenFilter};

/// Lowercases each token's text with the Unicode default, locale-independent mapping (SPEC §6.3.2).
///
/// This is lowercasing, not case folding: `ß` stays `ß` and `ẞ` becomes `ß` ([`FoldingFilter`](crate::FoldingFilter)
/// maps both to `ss`). A final capital sigma becomes `ς` (`Final_Sigma`, judged within the token). Turkish and Azeri
/// casing is not tailored: `I` becomes `i`, `İ` becomes `i` followed by U+0307 COMBINING DOT ABOVE, and `ı` is kept.
///
/// ```
/// use persia_analysis::{LowercaseFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = Vec::new();
/// WhitespaceTokenizer.tokenize("Ñandú STRAẞE ΟΔΟΣ", &mut tokens);
/// LowercaseFilter.apply(&mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["ñandú", "straße", "οδος"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LowercaseFilter;

impl TokenFilter for LowercaseFilter {
    fn apply(&self, tokens: &mut Vec<Token>) {
        for token in tokens.iter_mut() {
            if token.text.is_ascii() {
                token.text.make_ascii_lowercase();
            } else if token.text.chars().any(|c| !c.is_lowercase()) {
                // Text made only of lowercase letters maps to itself, so it keeps its allocation.
                token.text = token.text.to_lowercase();
            }
        }
        // Lowercasing never empties a token; kept for the filter contract.
        tokens.retain(|token| !token.text.is_empty());
    }
}
