//! NFKC normalization filter (SPEC §6.3.2).

use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfkc_quick};

use crate::token::TokenFilter;
use crate::token_stream::TokenStream;

/// Replaces each token's text by its Unicode NFKC normalization (SPEC §6.3.2): compatibility characters become
/// their plain forms, so `ﬁ`, full-width `Ａ` and `²` match `fi`, `A` and `2`.
///
/// ```
/// use persia_analysis::{TokenStream, NfkcFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = TokenStream::new();
/// WhitespaceTokenizer.tokenize("ﬁle Ｗｉｆｉ x²", &mut tokens);
/// NfkcFilter.apply(&mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["file", "Wifi", "x2"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NfkcFilter;

impl TokenFilter for NfkcFilter {
    fn apply(&self, tokens: &mut TokenStream) {
        // Most text is already NFKC (all of ASCII is): nothing is written when the quick check can tell. NFKC never
        // maps a non-empty string to an empty one, but `rewrite` removes emptied tokens anyway (the filter contract).
        tokens.rewrite(|text, out| {
            if is_nfkc_quick(text.chars()) == IsNormalized::Yes {
                return false;
            }
            out.extend(text.nfkc());
            out != text
        });
    }
}
