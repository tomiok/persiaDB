//! Fuzz the token filters and the synonym lookup (ROADMAP 4.2; SPEC §6.3.2): arbitrary text never panics, every
//! output token is non-empty and keeps the span and position of an input token in order, NFKC, lowercase and
//! folding are idempotent, and synonym matches are ordered and disjoint. The first byte picks the edge n-gram bounds
//! and whether the original is preserved; the text before the first `|` holds the synonym groups (`;` between
//! groups, `,` between terms).
#![no_main]

use std::num::NonZeroUsize;

use libfuzzer_sys::fuzz_target;
use persia_analysis::{
    EdgeNgramFilter, FoldingFilter, LowercaseFilter, NfkcFilter, StandardTokenizer, StopwordFilter,
    SynonymMap, Token, TokenFilter, Tokenizer, WhitespaceTokenizer,
};

fn check_from_input(input: &[Token], output: &[Token]) {
    let mut source = input.iter();
    let mut current: Option<&Token> = None;
    for token in output {
        assert!(!token.text.is_empty(), "empty token {token:?}");
        let key = (token.start, token.end, token.position);
        // Several output tokens may stack on one source token.
        while current.map(|t| (t.start, t.end, t.position)) != Some(key) {
            current = source.next();
            assert!(current.is_some(), "{token:?} has no source token");
        }
    }
}

fn analyze(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    StandardTokenizer.tokenize(text, &mut tokens);
    LowercaseFilter.apply(&mut tokens);
    tokens
}

fuzz_target!(|data: &[u8]| {
    let Some((&control, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let min = usize::from(control & 0x07) + 1;
    let max = min + usize::from((control >> 3) & 0x07);
    let ngrams = EdgeNgramFilter::new(
        NonZeroUsize::new(min).unwrap(),
        NonZeroUsize::new(max).unwrap(),
    )
    .unwrap()
    .preserve_original(control & 0x80 != 0);
    let preserved: Vec<char> = text.chars().take(2).collect();
    let fold = FoldingFilter::default();
    let fold_preserving = FoldingFilter::preserving(preserved);
    let idempotent: [&dyn TokenFilter; 4] =
        [&NfkcFilter, &LowercaseFilter, &fold, &fold_preserving];
    let others: [&dyn TokenFilter; 4] = [
        &StopwordFilter::english(),
        &StopwordFilter::spanish(),
        &StopwordFilter::portuguese(),
        &ngrams,
    ];
    let tokenizers: [&dyn Tokenizer; 2] = [&StandardTokenizer, &WhitespaceTokenizer];
    for tokenizer in tokenizers {
        let mut input = Vec::new();
        tokenizer.tokenize(text, &mut input);
        for filter in idempotent {
            let mut once = input.clone();
            filter.apply(&mut once);
            check_from_input(&input, &once);
            let mut twice = once.clone();
            filter.apply(&mut twice);
            assert_eq!(once, twice, "not idempotent");
        }
        for filter in others {
            let mut output = input.clone();
            filter.apply(&mut output);
            check_from_input(&input, &output);
        }
    }

    let (synonyms, query) = text.split_once('|').unwrap_or(("", text));
    let map = SynonymMap::new(
        synonyms
            .split(';')
            .map(|group| group.split(',').map(analyze).collect::<Vec<_>>()),
    );
    let tokens = analyze(query);
    let mut previous_end = 0;
    for m in map.find(&tokens) {
        let range = m.tokens();
        assert!(
            previous_end <= range.start && range.start < range.end && range.end <= tokens.len()
        );
        previous_end = range.end;
        for alternative in map.alternatives(&m) {
            assert!(!alternative.is_empty());
        }
    }
});
