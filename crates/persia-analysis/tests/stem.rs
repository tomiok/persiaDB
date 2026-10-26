//! Stemmers against the Snowball test vocabularies, and stemming properties (ROADMAP 4.3; SPEC §6.3.2).
//!
//! `tests/fixtures/stem/<language>.tsv` holds `word<TAB>stem` lines: `voc.txt` and `output.txt` of
//! snowballstem/snowball-data (BSD-3-Clause, see `tests/fixtures/stem/README.md`), the reference output of the
//! Snowball 3 algorithms.
#![allow(clippy::unwrap_used)] // helpers outside #[test] build fixed inputs

use std::borrow::Cow;

use persia_analysis::{
    EnglishStemmer, PortugueseStemmer, SpanishStemmer, Stemmer, StemmerFilter, Token, TokenFilter,
    TokenStream, Tokenizer, WhitespaceTokenizer,
};
use proptest::prelude::*;

const ENGLISH: &str = include_str!("fixtures/stem/english.tsv");
const SPANISH: &str = include_str!("fixtures/stem/spanish.tsv");
const PORTUGUESE: &str = include_str!("fixtures/stem/portuguese.tsv");

fn vocabulary(tsv: &str) -> impl Iterator<Item = (&str, &str)> {
    tsv.lines().map(|line| line.split_once('\t').unwrap())
}

/// Every vocabulary word stems to its reference stem, by the stemmer and through `StemmerFilter`; the failures (at
/// most 20 shown) are listed at once.
fn check_vocabulary(stemmer: &dyn Stemmer, tsv: &str, expected_words: usize) {
    let mut words = 0;
    let mut wrong = Vec::new();
    for (word, expected) in vocabulary(tsv) {
        words += 1;
        let stem = stemmer.stem(word);
        if stem != expected {
            wrong.push(format!("{word} -> {stem} (expected {expected})"));
        }
        let mut tokens = TokenStream::from(vec![Token {
            text: word.to_owned(),
            start: 0,
            end: word.len(),
            position: 0,
        }]);
        StemmerFilter::new(stemmer).apply(&mut tokens);
        let filtered: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        let want: &[&str] = if expected.is_empty() {
            &[]
        } else {
            &[expected]
        };
        if filtered != want {
            wrong.push(format!(
                "filter: {word} -> {filtered:?} (expected {expected})"
            ));
        }
    }
    assert_eq!(words, expected_words, "vocabulary size");
    assert!(
        wrong.is_empty(),
        "{} of {words} words stem wrongly:\n{}",
        wrong.len(),
        wrong
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn english_matches_the_snowball_vocabulary() {
    check_vocabulary(&EnglishStemmer, ENGLISH, 42_649);
}

#[test]
fn spanish_matches_the_snowball_vocabulary() {
    check_vocabulary(&SpanishStemmer, SPANISH, 28_378);
}

#[test]
fn portuguese_matches_the_snowball_vocabulary() {
    check_vocabulary(&PortugueseStemmer, PORTUGUESE, 32_016);
}

/// Stemming is not idempotent (SPEC §6.3.2); pinning how many vocabulary stems stem further catches an algorithm
/// change the vocabulary tests would also catch, and documents the size of the effect.
#[test]
fn stems_that_stem_further_are_counted() {
    let count = |stemmer: &dyn Stemmer, tsv: &str| {
        vocabulary(tsv)
            .filter(|&(_, stem)| stemmer.stem(stem) != stem)
            .count()
    };
    let counts = [
        count(&EnglishStemmer, ENGLISH),
        count(&SpanishStemmer, SPANISH),
        count(&PortugueseStemmer, PORTUGUESE),
    ];
    assert_eq!(counts, [1_526, 2_404, 2_519]);
}

#[test]
fn the_documented_examples_hold() {
    let stems = |stemmer: &dyn Stemmer, words: &[&str]| -> Vec<String> {
        words.iter().map(|w| stemmer.stem(w).into_owned()).collect()
    };
    assert_eq!(
        stems(
            &EnglishStemmer,
            &["accelerated", "acceler", "skies", "inning", "'s"]
        ),
        ["acceler", "accel", "sky", "inning", "'s"]
    );
    assert_eq!(
        stems(&SpanishStemmer, &["comerlo", "siguen", "pingüino", "año"]),
        ["com", "sig", "pingüin", "año"]
    );
    assert_eq!(
        stems(&PortugueseStemmer, &["maçã", "ação", "voz", "brasileiras"]),
        ["maçã", "açã", "voz", "brasileir"]
    );
}

#[test]
fn unchanged_words_are_borrowed() {
    for stemmer in [
        &EnglishStemmer as &dyn Stemmer,
        &SpanishStemmer,
        &PortugueseStemmer,
    ] {
        for word in ["", "x", "zzz", "東京"] {
            assert!(
                matches!(stemmer.stem(word), Cow::Borrowed(w) if w == word),
                "{word:?}"
            );
        }
    }
}

#[test]
fn the_filter_keeps_spans_and_positions_and_drops_empty_stems() {
    let mut tokens = TokenStream::new();
    // `''s` loses its leading apostrophe and then its possessive: its stem is empty.
    WhitespaceTokenizer.tokenize("The ''s cats running", &mut tokens);
    StemmerFilter::new(EnglishStemmer).apply(&mut tokens);
    let expected = [("The", 0, 3, 0), ("cat", 8, 12, 2), ("run", 13, 20, 3)];
    let got: Vec<(&str, usize, usize, u32)> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.start, t.end, t.position))
        .collect();
    assert_eq!(got, expected);
}

fn stemmers() -> [&'static dyn Stemmer; 3] {
    [&EnglishStemmer, &SpanishStemmer, &PortugueseStemmer]
}

proptest! {
    /// Stemmers never panic, and the filter rewrites only token texts.
    #[test]
    fn stemming_any_text_never_panics(text in any::<String>()) {
        for stemmer in stemmers() {
            let _ = stemmer.stem(&text);
        }
    }

    /// Words made of the letters the algorithms look at, where the interesting suffix rules fire.
    #[test]
    fn stemming_letter_words_never_panics(word in "[aeiouyáéíóúâêôãõçñü'ltsnrmdgcbvx~]{0,16}") {
        for stemmer in stemmers() {
            let _ = stemmer.stem(&word);
        }
    }

    #[test]
    fn the_filter_only_rewrites_texts(text in "[a-záéíóúñ' ]{0,64}") {
        let mut original = TokenStream::new();
        WhitespaceTokenizer.tokenize(&text, &mut original);
        let mut tokens = original.clone();
        StemmerFilter::new(SpanishStemmer).apply(&mut tokens);
        prop_assert_eq!(tokens.len(), original.len());
        for (stemmed, token) in tokens.iter().zip(&original) {
            prop_assert_eq!(
                (stemmed.start, stemmed.end, stemmed.position),
                (token.start, token.end, token.position)
            );
            prop_assert_eq!(&stemmed.text, &SpanishStemmer.stem(&token.text));
        }
    }
}

/// Edge cases where the port follows Snowball on input the vocabularies lack, so a cleanup cannot drift.
#[test]
fn snowball_edge_cases_are_kept() {
    let cases: [(&dyn Stemmer, &str, &str); 6] = [
        // A leading apostrophe goes, then the possessive: nothing is left.
        (&EnglishStemmer, "''s", ""),
        // An input `Y` counts as a consonant; marked `y`s are restored to lowercase.
        (&EnglishStemmer, "Yelling", "Yell"),
        (&EnglishStemmer, "sayYing", "sayi"),
        // `skies` is a table exception, not the `-ies` rule.
        (&EnglishStemmer, "skies", "sky"),
        // Portuguese writes `ã` as `a~` while stemming, so a literal `a~` comes back as `ã`.
        (&PortugueseStemmer, "a~o", "ão"),
        (&PortugueseStemmer, "maçã", "maçã"),
    ];
    for (stemmer, word, stem) in cases {
        assert_eq!(stemmer.stem(word), stem, "{word:?}");
    }
}

#[test]
fn stemmers_can_be_picked_at_run_time() {
    let filters: Vec<StemmerFilter<Box<dyn Stemmer>>> = vec![
        StemmerFilter::new(Box::new(EnglishStemmer)),
        StemmerFilter::new(Box::new(SpanishStemmer)),
        StemmerFilter::new(Box::new(std::sync::Arc::new(PortugueseStemmer))),
    ];
    let stems: Vec<String> = filters
        .iter()
        .zip(["skies", "canciones", "brasileiras"])
        .map(|(filter, word)| {
            let mut tokens = TokenStream::new();
            WhitespaceTokenizer.tokenize(word, &mut tokens);
            filter.apply(&mut tokens);
            tokens.first().unwrap().text.clone()
        })
        .collect();
    assert_eq!(stems, ["sky", "cancion", "brasileir"]);
}
