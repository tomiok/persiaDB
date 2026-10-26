//! Properties every tokenizer holds on arbitrary text (ROADMAP 4.1.1.3, 4.1.2, 4.1.3; SPEC §6.3.1).
#![allow(clippy::unwrap_used)] // helpers outside #[test] build fixed inputs

use std::num::NonZeroUsize;

use persia_analysis::{
    KeywordTokenizer, LengthPolicy, StandardTokenizer, Token, TokenLengthGuard, TokenStream,
    Tokenizer, WhitespaceTokenizer,
};
use proptest::prelude::*;

#[cfg(not(feature = "slow"))]
const CASES: u32 = 512;
#[cfg(feature = "slow")]
const CASES: u32 = 20_000;

/// Text mixing ASCII, punctuation, spaces, combining marks and several scripts, plus fully arbitrary strings.
fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z0-9 .,'@:/_\\-\t\n]{0,64}",
        "[a-zñéü\u{301}\u{2019} 0-9中文ひらタワภาษา한국어Привет🙂\u{200d}\u{feff}.,'@]{0,64}",
        any::<String>(),
    ]
}

fn tokens_of(tokenizer: &dyn Tokenizer, text: &str) -> Vec<Token> {
    let mut tokens = TokenStream::new();
    tokenizer.tokenize(text, &mut tokens);
    tokens.into_vec()
}

/// Spans are valid, non-empty, increasing and non-overlapping; texts equal their spans; positions count from 0.
fn assert_well_formed(text: &str, tokens: &[Token]) -> Result<(), TestCaseError> {
    let mut previous_end = 0;
    for (i, token) in tokens.iter().enumerate() {
        prop_assert!(token.start < token.end, "empty span {token:?}");
        prop_assert!(token.start >= previous_end, "overlap at {token:?}");
        prop_assert_eq!(text.get(token.start..token.end), Some(token.text.as_str()));
        prop_assert_eq!(token.position, u32::try_from(i).unwrap());
        previous_end = token.end;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn standard_is_well_formed_and_covers_alphanumerics(text in text()) {
        let tokens = tokens_of(&StandardTokenizer, &text);
        assert_well_formed(&text, &tokens)?;
        for token in &tokens {
            prop_assert!(token.text.chars().any(char::is_alphanumeric), "word without alphanumerics {token:?}");
        }
        // Every alphanumeric character lies inside a token.
        for (offset, c) in text.char_indices().filter(|(_, c)| c.is_alphanumeric()) {
            prop_assert!(
                tokens.iter().any(|t| t.start <= offset && offset < t.end),
                "{c:?} at {offset} not covered"
            );
        }
    }

    #[test]
    fn whitespace_matches_split_whitespace(text in text()) {
        let tokens = tokens_of(&WhitespaceTokenizer, &text);
        assert_well_formed(&text, &tokens)?;
        let expected: Vec<&str> = text.split_whitespace().collect();
        let got: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        prop_assert_eq!(got, expected);
    }

    #[test]
    fn keyword_is_the_whole_text(text in text()) {
        let tokens = tokens_of(&KeywordTokenizer, &text);
        assert_well_formed(&text, &tokens)?;
        if text.is_empty() {
            prop_assert!(tokens.is_empty());
        } else {
            prop_assert_eq!(tokens.len(), 1);
            prop_assert_eq!(tokens[0].end, text.len());
        }
    }

    #[test]
    fn length_guard_bounds_tokens(text in text(), max in 1usize..16, drop in any::<bool>()) {
        let original = tokens_of(&WhitespaceTokenizer, &text);
        let policy = if drop { LengthPolicy::Drop } else { LengthPolicy::Truncate };
        let guard = TokenLengthGuard::new(NonZeroUsize::new(max).unwrap(), policy);
        let mut tokens = TokenStream::from(original.clone());
        guard.apply(&mut tokens);

        let mut kept = tokens.iter().peekable();
        for before in &original {
            let fits = before.text.len() <= max;
            let truncated = before.text.get(..before.text.floor_char_boundary(max)).unwrap();
            let survives = fits || (policy == LengthPolicy::Truncate && !truncated.is_empty());
            if !survives {
                continue;
            }
            let after = kept.next().unwrap();
            // Span and position are kept; text is the original or its longest fitting prefix.
            prop_assert_eq!((after.start, after.end, after.position), (before.start, before.end, before.position));
            prop_assert_eq!(after.text.as_str(), if fits { before.text.as_str() } else { truncated });
            prop_assert!(after.text.len() <= max);
        }
        prop_assert!(kept.next().is_none());
    }
}
