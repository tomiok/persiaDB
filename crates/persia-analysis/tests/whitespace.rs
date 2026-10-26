//! Whitespace and keyword tokenizers (ROADMAP 4.1.2, SPEC §6.3.1).

use persia_analysis::{KeywordTokenizer, Token, TokenStream, Tokenizer, WhitespaceTokenizer};

fn run(tokenizer: &dyn Tokenizer, text: &str) -> Vec<(String, usize, usize, u32)> {
    let mut tokens = TokenStream::new();
    tokenizer.tokenize(text, &mut tokens);
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

fn texts(tokenizer: &dyn Tokenizer, text: &str) -> Vec<String> {
    run(tokenizer, text).into_iter().map(|t| t.0).collect()
}

#[test]
fn whitespace_splits_on_unicode_white_space_only() {
    let table: &[(&str, &[&str])] = &[
        ("", &[]),
        (" \t\r\n ", &[]),
        ("a", &["a"]),
        ("C++ and Go!", &["C++", "and", "Go!"]),
        ("ana@example.com don't", &["ana@example.com", "don't"]),
        // No-break space, ideographic space, line and paragraph separators are white space.
        (
            "a\u{a0}b\u{3000}c\u{2028}d\u{2029}e",
            &["a", "b", "c", "d", "e"],
        ),
        // Zero-width space and BOM are not.
        ("a\u{200b}b \u{feff}c", &["a\u{200b}b", "\u{feff}c"]),
        ("中文 分词", &["中文", "分词"]),
    ];
    for (input, expected) in table {
        assert_eq!(
            texts(&WhitespaceTokenizer, input),
            *expected,
            "input {input:?}"
        );
    }
}

#[test]
fn whitespace_offsets() {
    assert_eq!(
        run(&WhitespaceTokenizer, "\u{3000}año  ñu\n"),
        [("año".into(), 3, 7, 0), ("ñu".into(), 9, 12, 1)]
    );
}

#[test]
fn keyword_keeps_the_whole_text() {
    assert_eq!(run(&KeywordTokenizer, ""), []);
    assert_eq!(run(&KeywordTokenizer, " "), [(" ".into(), 0, 1, 0)]);
    assert_eq!(
        run(&KeywordTokenizer, "New York, NY"),
        [("New York, NY".into(), 0, 12, 0)]
    );
    assert_eq!(
        run(&KeywordTokenizer, "São Paulo"),
        [("São Paulo".into(), 0, 10, 0)]
    );
}
