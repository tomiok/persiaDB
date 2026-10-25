//! Fuzz the tokenizers and the length guard (ROADMAP 4.1; SPEC §6.3.1): arbitrary text never panics, every token's
//! span is a non-empty, increasing, char-aligned slice equal to its text, positions count from 0, and the guard
//! keeps spans and positions while bounding text length. The first byte picks the guard's limit and policy.
#![no_main]

use std::num::NonZeroUsize;

use libfuzzer_sys::fuzz_target;
use persia_analysis::{
    KeywordTokenizer, LengthPolicy, StandardTokenizer, Token, TokenLengthGuard, Tokenizer,
    WhitespaceTokenizer,
};

fn check(text: &str, tokens: &[Token]) {
    let mut previous_end = 0;
    for (i, token) in tokens.iter().enumerate() {
        assert!(
            previous_end <= token.start && token.start < token.end,
            "bad span {token:?}"
        );
        assert_eq!(text.get(token.start..token.end), Some(token.text.as_str()));
        assert_eq!(token.position, u32::try_from(i).unwrap());
        previous_end = token.end;
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&control, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let max = NonZeroUsize::new(usize::from(control & 0x7f).max(1)).unwrap();
    let policy = if control & 0x80 == 0 {
        LengthPolicy::Truncate
    } else {
        LengthPolicy::Drop
    };
    let guard = TokenLengthGuard::new(max, policy);
    let tokenizers: [&dyn Tokenizer; 3] =
        [&StandardTokenizer, &WhitespaceTokenizer, &KeywordTokenizer];
    for tokenizer in tokenizers {
        let mut tokens = Vec::new();
        tokenizer.tokenize(text, &mut tokens);
        check(text, &tokens);
        let mut guarded = tokens.clone();
        guard.apply(&mut guarded);
        // Exactly the tokens that fit, or (truncating) whose longest char-aligned prefix is non-empty, survive,
        // in order, with their span and position, as the original or that prefix.
        let mut kept = guarded.iter();
        for original in &tokens {
            let fits = original.text.len() <= max.get();
            let prefix = &original.text[..original.text.floor_char_boundary(max.get())];
            if !fits && (policy == LengthPolicy::Drop || prefix.is_empty()) {
                continue;
            }
            let token = kept.next().expect("a surviving token was dropped");
            assert_eq!(
                (token.start, token.end, token.position),
                (original.start, original.end, original.position)
            );
            assert_eq!(
                token.text,
                if fits { original.text.as_str() } else { prefix }
            );
        }
        assert!(kept.next().is_none(), "the guard invented a token");
    }
});
