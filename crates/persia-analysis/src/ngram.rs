//! Edge n-gram filter (SPEC §6.3.2).

use std::num::NonZeroUsize;

use crate::token::{Token, TokenFilter};

/// Replaces each token by its prefixes, for optional prefix indexing (SPEC §6.3.2).
///
/// Prefixes have `min..=max` characters (Unicode scalar values), shortest first, and are stacked on the position
/// and span of their token. A token shorter than `min` produces none. With
/// [`preserve_original`](Self::preserve_original), the whole token follows its prefixes when it is not one of them
/// (longer than `max` or shorter than `min`). Put a [`TokenLengthGuard`](crate::TokenLengthGuard) first, so the
/// prefixes of one token stay bounded.
///
/// ```
/// use std::num::NonZeroUsize;
/// use persia_analysis::{EdgeNgramFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let n = |n| NonZeroUsize::new(n).unwrap();
/// let mut tokens = Vec::new();
/// WhitespaceTokenizer.tokenize("año a", &mut tokens);
/// EdgeNgramFilter::new(n(2), n(3)).unwrap().apply(&mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["añ", "año"]);
/// assert!(tokens.iter().all(|t| (t.start, t.end, t.position) == (0, 4, 0)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EdgeNgramFilter {
    min: NonZeroUsize,
    max: NonZeroUsize,
    preserve_original: bool,
}

impl EdgeNgramFilter {
    /// A filter emitting prefixes of `min..=max` characters, or `None` when `max < min`.
    pub const fn new(min: NonZeroUsize, max: NonZeroUsize) -> Option<Self> {
        if max.get() < min.get() {
            return None;
        }
        Some(Self {
            min,
            max,
            preserve_original: false,
        })
    }

    /// Also keeps each token whole when it is not one of its own prefixes.
    #[must_use]
    pub const fn preserve_original(mut self, preserve: bool) -> Self {
        self.preserve_original = preserve;
        self
    }

    /// The shortest prefix, in characters.
    pub const fn min(&self) -> NonZeroUsize {
        self.min
    }

    /// The longest prefix, in characters.
    pub const fn max(&self) -> NonZeroUsize {
        self.max
    }

    /// Whether tokens that are not one of their prefixes are kept whole too.
    pub const fn preserves_original(&self) -> bool {
        self.preserve_original
    }
}

impl TokenFilter for EdgeNgramFilter {
    fn apply(&self, tokens: &mut Vec<Token>) {
        let input = std::mem::take(tokens);
        for token in input {
            // Byte offsets just past each character, so `&text[..end]` is a prefix of `chars` characters.
            let ends = token
                .text
                .char_indices()
                .map(|(i, c)| i.saturating_add(c.len_utf8()));
            let mut is_prefix = false;
            for (chars, end) in (1..).zip(ends) {
                if chars > self.max.get() {
                    break;
                }
                if chars >= self.min.get() {
                    let Some(prefix) = token.text.get(..end) else {
                        break;
                    };
                    is_prefix = end == token.text.len();
                    tokens.push(Token {
                        text: prefix.to_owned(),
                        start: token.start,
                        end: token.end,
                        position: token.position,
                    });
                }
            }
            if self.preserve_original && !is_prefix && !token.text.is_empty() {
                tokens.push(token);
            }
        }
    }
}
