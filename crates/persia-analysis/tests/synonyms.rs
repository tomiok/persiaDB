//! Query-time synonym lookup (ROADMAP 4.2.5; SPEC §6.3.2, §7.6 L1).

use persia_analysis::{
    DEFAULT_SYNONYM_EXPANSIONS, LowercaseFilter, StandardTokenizer, StopwordFilter, SynonymMap,
    Token, TokenFilter, TokenStream, Tokenizer,
};

/// A small analyzer: standard words, lowercase, English stopwords.
fn analyze(text: &str) -> Vec<Token> {
    let mut tokens = TokenStream::new();
    StandardTokenizer.tokenize(text, &mut tokens);
    LowercaseFilter.apply(&mut tokens);
    StopwordFilter::english().apply(&mut tokens);
    tokens.into_vec()
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
                .alternatives(m, DEFAULT_SYNONYM_EXPANSIONS)
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
    // "bang" is left alone in its group, so it has nothing to expand to and does not match.
    let map = map(&[&["the", "of", "a"], &["!!!", "bang"], &["boom", "!"]]);
    assert_eq!(expand(&map, "the bang boom"), []);
    assert_eq!(SynonymMap::default().find(&analyze("anything")), []);
}

#[test]
fn a_group_that_collapses_to_one_term_does_not_hide_shorter_terms() {
    // Both spellings of "New York" lowercase to one term: it must not match, or "york" would never expand.
    let collapsed = map(&[&["New York", "new york"], &["york", "yorkshire"]]);
    assert_eq!(
        expand(&collapsed, "new york"),
        [(1..2, vec!["yorkshire".into()])]
    );
    // The same term with a real synonym in another group still matches.
    let with_nyc = map(&[&["New York", "new york"], &["new york", "nyc"]]);
    assert_eq!(expand(&with_nyc, "NEW YORK"), [(0..2, vec!["nyc".into()])]);
}

#[test]
fn a_decreasing_position_keeps_the_match_found_so_far() {
    let map = map(&[&["p", "y"], &["p q", "w"]]);
    let token = |text: &str, position| Token {
        text: text.into(),
        start: 0,
        end: 1,
        position,
    };
    let tokens = [token("p", 5), token("q", 0)];
    let matches = map.find(&tokens);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].tokens(), 0..1);
}

#[test]
fn alternatives_are_capped_in_group_then_term_order() {
    let words: Vec<String> = (0..120).map(|i| format!("w{i}")).collect();
    let mut first: Vec<&str> = vec!["x"];
    first.extend(words[..60].iter().map(String::as_str));
    let mut second: Vec<&str> = vec!["x"];
    second.extend(words[60..].iter().map(String::as_str));
    let map = map(&[&first, &second]);
    let tokens = analyze("x");
    let matches = map.find(&tokens);
    let texts = |limit| -> Vec<String> {
        map.alternatives(&matches[0], limit)
            .iter()
            .map(|t| t[0].text.clone())
            .collect()
    };
    assert_eq!(texts(DEFAULT_SYNONYM_EXPANSIONS), words[..50]);
    assert_eq!(texts(70), words[..70]);
    assert_eq!(texts(1000), words);
    assert_eq!(texts(0), Vec::<String>::new());
}
