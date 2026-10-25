//! NFKC normalization filter (SPEC §6.3.2).

use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfkc_quick};

use crate::token::{Token, TokenFilter};

/// Replaces each token's text by its Unicode NFKC normalization (SPEC §6.3.2): compatibility characters become
/// their plain forms, so `ﬁ`, full-width `Ａ` and `²` match `fi`, `A` and `2`.
///
/// ```
/// use persia_analysis::{NfkcFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = Vec::new();
/// WhitespaceTokenizer.tokenize("ﬁle Ｗｉｆｉ x²", &mut tokens);
/// NfkcFilter.apply(&mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["file", "Wifi", "x2"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NfkcFilter;

impl TokenFilter for NfkcFilter {
    fn apply(&self, tokens: &mut Vec<Token>) {
        for token in tokens.iter_mut() {
            // Most text is already NFKC (all of ASCII is): skip the allocation when the quick check can tell.
            if is_nfkc_quick(token.text.chars()) != IsNormalized::Yes {
                token.text = token.text.nfkc().collect();
            }
        }
        // NFKC never maps a non-empty string to an empty one, but the filter contract does not rely on it.
        tokens.retain(|token| !token.text.is_empty());
    }
}
