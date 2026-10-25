//! Query-time synonym lookup (ROADMAP 4.2.5; SPEC §6.3.2, §7.6 L1).

use persia_analysis::{
    LowercaseFilter, StandardTokenizer, StopwordFilter, SynonymMap, Token, TokenFilter, Tokenizer,
};

/// A small analyzer: standard words, lowercase, English stopwords.
fn analyze(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    StandardTokenizer.tokenize(text, &mut tokens);
    LowercaseFilter.apply(&mut tokens);
    StopwordFilter::english().apply(&mut tokens);
    tokens
}

fn map(groups: &[&[&str]]) -> SynonymMap {
    SynonymMap::new(
        groups
            .iter()
            .map(|g| g.iter().map(|t| analyze(t)).collect::<Vec<_>>()),
    )
}

/// Each match as (token range, alternatives as space-joined texts).
fn expand(map: &SynonymMap, query: &str) -> Vec<(std::ops::Range<usize>, Vec<String>)> {
    let tokens = analyze(query);
    map.find(&tokens)
        .iter()
        .map(|m| {
            let alternatives = map
                .alternatives(m)
                .iter()
                .map(|term| {
                    term.iter()
                        .map(|t| t.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect();
            (m.tokens(), alternatives)
        })
        .collect()
}

#[test]
fn single_token_terms_expand_both_ways() {
    let map = map(&[&["tv", "television"]]);
    assert_eq!(
        expand(&map, "cheap TV"),
        [(1..2, vec!["television".into()])]
    );
    assert_eq!(expand(&map, "Television"), [(0..1, vec!["tv".into()])]);
    assert_eq!(expand(&map, "radio"), []);
}

#[test]
fn the_longest_term_wins_and_matches_do_not_overlap() {
    let map = map(&[
        &["new york", "nyc"],
        &["new", "fresh"],
        &["york", "yorkshire"],
    ]);
    assert_eq!(
        expand(&map, "new york new"),
        [(0..2, vec!["nyc".into()]), (2..3, vec!["fresh".into()])]
    );
    assert_eq!(expand(&map, "york"), [(0..1, vec!["yorkshire".into()])]);
}

#[test]
fn a_shorter_term_matches_when_the_longer_one_breaks_off() {
    let map = map(&[&["p q r", "x"], &["p", "y"]]);
    // "p q" is a trie prefix of "p q r" but no term: the match falls back to "p".
    assert_eq!(expand(&map, "p q s"), [(0..1, vec!["y".into()])]);
}

#[test]
fn position_gaps_must_line_up() {
    // "the" is a stopword: the term "lord of rings" has a gap where "of" was removed.
    let map = map(&[&["lord of rings", "lotr"]]);
    assert_eq!(expand(&map, "lord of the rings"), []);
    assert_eq!(expand(&map, "Lord of Rings"), [(0..2, vec!["lotr".into()])]);
    assert_eq!(expand(&map, "lord rings"), []);
}

#[test]
fn a_term_in_several_groups_expands_to_all_of_them_once() {
    let map = map(&[
        &["car", "auto"],
        &["car", "vehicle", "auto"],
        &["CAR", "car"],
    ]);
    assert_eq!(
        expand(&map, "car"),
        [(0..1, vec!["auto".into(), "vehicle".into()])]
    );
    assert_eq!(
        expand(&map, "vehicle"),
        [(0..1, vec!["car".into(), "auto".into()])]
    );
}

#[test]
fn terms_without_tokens_are_ignored() {
    let map = map(&[&["the", "of", "a"], &["!!!", "bang"]]);
    // Ranges index the analyzed tokens, where "the" is already gone.
    assert_eq!(expand(&map, "the bang"), [(0..1, vec![])]);
    assert_eq!(SynonymMap::default().find(&analyze("anything")), []);
}
