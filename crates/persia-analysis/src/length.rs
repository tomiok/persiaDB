//! Max token length guard (SPEC §6.3.1).

use std::num::NonZeroUsize;

use crate::token::Token;

/// Default longest token, in bytes of UTF-8, that a [`TokenLengthGuard`] keeps whole.
pub const DEFAULT_MAX_TOKEN_LEN: NonZeroUsize = match NonZeroUsize::new(255) {
    Some(len) => len,
    None => NonZeroUsize::MIN,
};

/// What a [`TokenLengthGuard`] does with a token longer than its limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LengthPolicy {
    /// Keep the longest prefix of the token's text that fits the limit and ends on a character boundary; a token
    /// whose first character is already longer than the limit is dropped.
    #[default]
    Truncate,
    /// Drop the token.
    Drop,
}

/// Bounds the length of token texts, so one pathological token (a base64 blob, a run of letters without spaces)
/// cannot bloat the term dictionary (SPEC §6.3.1).
///
/// Lengths are counted in bytes of UTF-8, the unit term dictionaries key on. Kept tokens keep their span and
/// position; dropped tokens leave a gap in positions, so a phrase never matches across them.
///
/// ```
/// use std::num::NonZeroUsize;
/// use persia_analysis::{LengthPolicy, TokenLengthGuard, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = Vec::new();
/// WhitespaceTokenizer.tokenize("año extraordinario", &mut tokens);
/// let guard = TokenLengthGuard::new(NonZeroUsize::new(5).unwrap(), LengthPolicy::Truncate);
/// guard.apply(&mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["año", "extra"]);
/// assert_eq!(tokens[1].end, 19); // the span still covers the whole source word
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenLengthGuard {
    max_len: NonZeroUsize,
    policy: LengthPolicy,
}

impl Default for TokenLengthGuard {
    /// [`DEFAULT_MAX_TOKEN_LEN`] bytes, [`LengthPolicy::Truncate`].
    fn default() -> Self {
        Self::new(DEFAULT_MAX_TOKEN_LEN, LengthPolicy::default())
    }
}

impl TokenLengthGuard {
    /// A guard keeping tokens of at most `max_len` bytes whole and applying `policy` to longer ones.
    pub const fn new(max_len: NonZeroUsize, policy: LengthPolicy) -> Self {
        Self { max_len, policy }
    }

    /// The longest token text, in bytes, kept whole.
    pub const fn max_len(&self) -> NonZeroUsize {
        self.max_len
    }

    /// What happens to longer tokens.
    pub const fn policy(&self) -> LengthPolicy {
        self.policy
    }

    /// Truncates or drops, per the policy, every token of `tokens` whose text is longer than the limit.
    pub fn apply(&self, tokens: &mut Vec<Token>) {
        let max = self.max_len.get();
        match self.policy {
            LengthPolicy::Drop => tokens.retain(|token| token.text.len() <= max),
            LengthPolicy::Truncate => tokens.retain_mut(|token| {
                if token.text.len() > max {
                    token.text.truncate(token.text.floor_char_boundary(max));
                }
                !token.text.is_empty()
            }),
        }
    }
}
