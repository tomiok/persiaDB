//! Properties every token filter holds on arbitrary text (ROADMAP 4.2; SPEC §6.3.2).
#![allow(clippy::unwrap_used)] // helpers outside #[test] build fixed inputs

use std::num::NonZeroUsize;

use persia_analysis::{
    EdgeNgramFilter, FoldingFilter, LowercaseFilter, NfkcFilter, StandardTokenizer, StopwordFilter,
    SynonymMap, Token, TokenFilter, Tokenizer, WhitespaceTokenizer,
};
use proptest::prelude::*;
use unicode_normalization::{is_nfc, is_nfkc};

#[cfg(not(feature = "slow"))]
const CASES: u32 = 512;
#[cfg(feature = "slow")]
const CASES: u32 = 20_000;

/// Text heavy in accents, compatibility characters, case pairs and marks, plus fully arbitrary strings.
fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-ZñÑéÉüÜßẞİıIæŒøĐłþ\u{300}-\u{36f}ﬁＡ²⑴ΣΟΔάёЁ한क़ǅ \t]{0,48}",
        "(the|a|de|la|é|que|no|[a-z]{1,3})( (the|a|de|la|é|que|no|[a-z]{1,3})){0,12}",
        any::<String>(),
    ]
}

fn whitespace_tokens(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    WhitespaceTokenizer.tokenize(text, &mut tokens);
    tokens
}

fn filtered(filter: &dyn TokenFilter, mut tokens: Vec<Token>) -> Vec<Token> {
    filter.apply(&mut tokens);
    tokens
}

fn texts(tokens: &[Token]) -> Vec<&str> {
    tokens.iter().map(|t| t.text.as_str()).collect()
}

/// Every output token is non-empty and carries the span and position of an input token, in input order (several
/// outputs may stack on one input).
fn assert_from_input(input: &[Token], output: &[Token]) -> Result<(), TestCaseError> {
    let mut source = input.iter();
    let mut current: Option<&Token> = None;
    for token in output {
        prop_assert!(!token.text.is_empty(), "empty token {token:?}");
        let key = (token.start, token.end, token.position);
        while current.map(|t| (t.start, t.end, t.position)) != Some(key) {
            current = source.next();
            prop_assert!(current.is_some(), "{token:?} has no source token");
        }
    }
    Ok(())
}

/// A 1:1 filter: same tokens minus removed ones, spans and positions untouched.
fn check_one_to_one(filter: &dyn TokenFilter, text: &str) -> Result<Vec<Token>, TestCaseError> {
    let input = whitespace_tokens(text);
    let output = filtered(filter, input.clone());
    assert_from_input(&input, &output)?;
    prop_assert!(output.len() <= input.len());
    let positions: Vec<u32> = output.iter().map(|t| t.position).collect();
    prop_assert!(positions.windows(2).all(|w| matches!(w, [a, b] if a < b)));
    Ok(output)
}

fn idempotent(filter: &dyn TokenFilter, once: &[Token]) -> Result<(), TestCaseError> {
    let twice = filtered(filter, once.to_vec());
    prop_assert_eq!(texts(&twice), texts(once));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn nfkc_normalizes_and_is_idempotent(text in text()) {
        let once = check_one_to_one(&NfkcFilter, &text)?;
        for token in &once {
            prop_assert!(is_nfkc(&token.text), "{token:?}");
        }
        idempotent(&NfkcFilter, &once)?;
    }

    #[test]
    fn lowercase_matches_std_and_is_idempotent(text in text()) {
        let once = check_one_to_one(&LowercaseFilter, &text)?;
        let expected: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
        prop_assert_eq!(texts(&once), expected.iter().map(String::as_str).collect::<Vec<_>>());
        idempotent(&LowercaseFilter, &once)?;
    }

    #[test]
    fn folding_strips_accents_and_is_idempotent(text in text(), preserve in prop::collection::vec(prop_oneof![
        Just('ñ'), Just('Ñ'), Just('ü'), Just('ß'), Just('\u{301}'), Just('\u{308}'), any::<char>()
    ], 0..4)) {
        for filter in [FoldingFilter::default(), FoldingFilter::preserving(preserve.iter().copied())] {
            let once = check_one_to_one(&filter, &text)?;
            for token in &once {
                // Preserved characters are not recomposed, so only text without them is NFC.
                prop_assert!(!filter.preserved().is_empty() || is_nfc(&token.text), "{token:?}");
                for c in token.text.chars() {
                    prop_assert!(
                        !matches!(c, '\u{300}'..='\u{36f}') || filter.preserved().contains(&c),
                        "unfolded mark in {token:?}"
                    );
                }
            }
            idempotent(&filter, &once)?;
        }
    }

    #[test]
    fn stopwords_remove_exactly_the_listed_words(text in text()) {
        for filter in [StopwordFilter::english(), StopwordFilter::spanish(), StopwordFilter::portuguese()] {
            let input = whitespace_tokens(&text);
            let output = check_one_to_one(&filter, &text)?;
            let expected: Vec<&Token> = input.iter().filter(|t| !filter.is_stopword(&t.text)).collect();
            prop_assert_eq!(output.iter().collect::<Vec<_>>(), expected);
        }
    }

    #[test]
    fn edge_ngrams_are_bounded_prefixes(text in text(), min in 1_usize..5, extra in 0_usize..4, preserve: bool) {
        let max = min + extra;
        let filter = EdgeNgramFilter::new(NonZeroUsize::new(min).unwrap(), NonZeroUsize::new(max).unwrap())
            .unwrap()
            .preserve_original(preserve);
        let input = whitespace_tokens(&text);
        let output = filtered(&filter, input.clone());
        assert_from_input(&input, &output)?;
        for source in &input {
            let stacked: Vec<&str> = output
                .iter()
                .filter(|t| t.position == source.position)
                .map(|t| t.text.as_str())
                .collect();
            let chars = source.text.chars().count();
            let mut expected: Vec<String> = (min..=max.min(chars))
                .map(|n| source.text.chars().take(n).collect())
                .collect();
            if preserve && !(min..=max).contains(&chars) {
                expected.push(source.text.clone());
            }
            prop_assert_eq!(stacked, expected.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }

    #[test]
    fn synonym_matches_are_ordered_disjoint_terms(
        groups in prop::collection::vec(prop::collection::vec("[a-c]{1,2}( [a-c]{1,2}){0,2}", 1..4), 0..6),
        query in "[a-c]{1,2}( [a-c]{1,2}){0,10}",
    ) {
        let analyze = |text: &str| {
            let mut tokens = Vec::new();
            StandardTokenizer.tokenize(text, &mut tokens);
            tokens
        };
        let map = SynonymMap::new(groups.iter().map(|g| g.iter().map(|t| analyze(t)).collect::<Vec<_>>()));
        let tokens = analyze(&query);
        let mut previous_end = 0;
        for m in map.find(&tokens) {
            let range = m.tokens();
            prop_assert!(previous_end <= range.start && range.start < range.end && range.end <= tokens.len());
            previous_end = range.end;
            let matched: Vec<&str> = texts(&tokens[range]);
            // The matched text is a term of some group, and no alternative repeats it.
            let joined = matched.join(" ");
            prop_assert!(groups.iter().flatten().any(|term| *term == joined));
            let alternatives: Vec<String> =
                map.alternatives(&m).iter().map(|t| texts(t).join(" ")).collect();
            prop_assert!(!alternatives.contains(&joined));
            let mut unique = alternatives.clone();
            unique.sort();
            unique.dedup();
            prop_assert_eq!(unique.len(), alternatives.len());
            for alternative in &alternatives {
                prop_assert!(groups.iter().any(|g| g.contains(alternative) && g.contains(&joined)));
            }
        }
    }
}
