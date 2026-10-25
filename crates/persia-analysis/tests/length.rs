//! Max token length guard (ROADMAP 4.1.3, SPEC §6.3.1).
#![allow(clippy::unwrap_used)] // helpers outside #[test] build fixed inputs

use std::num::NonZeroUsize;

use persia_analysis::{
    DEFAULT_MAX_TOKEN_LEN, LengthPolicy, Token, TokenLengthGuard, Tokenizer, WhitespaceTokenizer,
};

fn guarded(text: &str, max: usize, policy: LengthPolicy) -> Vec<(String, usize, usize, u32)> {
    let mut tokens = Vec::new();
    WhitespaceTokenizer.tokenize(text, &mut tokens);
    TokenLengthGuard::new(NonZeroUsize::new(max).unwrap(), policy).apply(&mut tokens);
    tokens
        .into_iter()
        .map(
            |Token {
                 text,
                 start,
                 end,
                 position,
             }| (text, start, end, position),
        )
        .collect()
}

#[test]
fn tokens_at_the_limit_are_kept_whole() {
    for policy in [LengthPolicy::Truncate, LengthPolicy::Drop] {
        assert_eq!(
            guarded("abc de", 3, policy),
            [("abc".into(), 0, 3, 0), ("de".into(), 4, 6, 1)]
        );
    }
}

#[test]
fn drop_leaves_a_position_gap() {
    assert_eq!(
        guarded("a looooong b", 4, LengthPolicy::Drop),
        [("a".into(), 0, 1, 0), ("b".into(), 11, 12, 2)]
    );
}

#[test]
fn truncate_keeps_the_span_and_cuts_on_a_char_boundary() {
    assert_eq!(
        guarded("abcdef", 4, LengthPolicy::Truncate),
        [("abcd".into(), 0, 6, 0)]
    );
    // "ñ" is two bytes: a limit of 2 cannot keep "añ", only "a".
    assert_eq!(
        guarded("año", 2, LengthPolicy::Truncate),
        [("a".into(), 0, 4, 0)]
    );
    assert_eq!(
        guarded("año", 3, LengthPolicy::Truncate),
        [("añ".into(), 0, 4, 0)]
    );
}

#[test]
fn truncate_drops_a_token_whose_first_char_does_not_fit() {
    // "中" is three bytes.
    assert_eq!(
        guarded("中文 a", 2, LengthPolicy::Truncate),
        [("a".into(), 7, 8, 1)]
    );
}

#[test]
fn default_guard() {
    let guard = TokenLengthGuard::default();
    assert_eq!(guard.max_len(), DEFAULT_MAX_TOKEN_LEN);
    assert_eq!(DEFAULT_MAX_TOKEN_LEN.get(), 255);
    assert_eq!(guard.policy(), LengthPolicy::Truncate);
    let long = "x".repeat(300);
    let mut tokens = Vec::new();
    WhitespaceTokenizer.tokenize(&long, &mut tokens);
    guard.apply(&mut tokens);
    assert_eq!(tokens[0].text.len(), 255);
    assert_eq!(tokens[0].end, 300);
}
