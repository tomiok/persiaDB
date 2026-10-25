//! Standard (UAX #29) tokenizer: a table of tricky inputs (ROADMAP 4.1.1.3, SPEC §6.3.1).

use persia_analysis::{StandardTokenizer, Token, Tokenizer};

fn words(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    StandardTokenizer.tokenize(text, &mut tokens);
    tokens.into_iter().map(|t| t.text).collect()
}

#[test]
fn tricky_inputs() {
    let table: &[(&str, &[&str])] = &[
        // Empty and non-word input.
        ("", &[]),
        ("   \t\n", &[]),
        ("!?.,;:-()[]{}", &[]),
        ("🙂 👍🏽 ★", &[]),
        // A keycap emoji keeps its digit, so it is a word.
        (
            "1\u{fe0f}\u{20e3} 👨\u{200d}👩\u{200d}👧",
            &["1\u{fe0f}\u{20e3}"],
        ),
        // Plain words, punctuation and mixed case.
        ("Hello, world!", &["Hello", "world"]),
        ("  leading and trailing  ", &["leading", "and", "trailing"]),
        ("well-known", &["well", "known"]),
        // Numbers: inner separators keep them whole, signs and units do not.
        ("3.14 1,000 42", &["3.14", "1,000", "42"]),
        ("-5 +7", &["5", "7"]),
        ("v2 x86_64 2nd", &["v2", "x86_64", "2nd"]),
        ("10% $20", &["10", "20"]),
        ("2026-10-25", &["2026", "10", "25"]),
        ("end. 3", &["end", "3"]),
        // Apostrophes between letters join; leading and trailing ones do not.
        ("don't O'Neil l'été", &["don't", "O'Neil", "l'été"]),
        ("don\u{2019}t", &["don\u{2019}t"]),
        ("'quoted' dogs'", &["quoted", "dogs"]),
        // Emails and URLs split at `@`, `:` and `/`; dotted names stay whole.
        ("ana@example.com", &["ana", "example.com"]),
        (
            "first.last@mail.example.org",
            &["first.last", "mail.example.org"],
        ),
        (
            "https://example.com/a/b?q=1",
            &["https", "example.com", "a", "b", "q", "1"],
        ),
        ("www.example.com.", &["www.example.com"]),
        // Underscores join.
        ("snake_case _private", &["snake_case", "_private"]),
        // Accents, composed and decomposed (combining marks stay with their letter).
        ("café naïve", &["café", "naïve"]),
        ("cafe\u{301}", &["cafe\u{301}"]),
        ("niño año", &["niño", "año"]),
        // Non-Latin scripts written with spaces stay whole.
        ("Привет мир", &["Привет", "мир"]),
        ("γειά σου", &["γειά", "σου"]),
        ("مرحبا بالعالم", &["مرحبا", "بالعالم"]),
        ("שלום עולם", &["שלום", "עולם"]),
        ("안녕하세요 세계", &["안녕하세요", "세계"]),
        // Scripts without spaces fall back to one token per character (Han, Hiragana, Thai).
        ("中文分词", &["中", "文", "分", "词"]),
        ("東京タワーへ", &["東", "京", "タワー", "へ"]),
        ("ひらがな", &["ひ", "ら", "が", "な"]),
        ("ภาษาไทย", &["ภ", "า", "ษ", "า", "ไ", "ท", "ย"]),
        // Mixed scripts.
        ("iPhone15中国", &["iPhone15", "中", "国"]),
        // Full-width digits and letters are alphanumeric.
        ("ＡＢＣ１２３", &["ＡＢＣ１２３"]),
        // Control characters and NUL separate words.
        ("a\u{0}b\u{7}c", &["a", "b", "c"]),
        // Zero-width joiner and BOM inside a word do not split it; a leading BOM is not part of the word.
        ("a\u{200d}b wo\u{feff}rd", &["a\u{200d}b", "wo\u{feff}rd"]),
        ("\u{feff}word", &["word"]),
    ];
    for (input, expected) in table {
        assert_eq!(words(input), *expected, "input {input:?}");
    }
}

#[test]
fn offsets_and_positions() {
    let text = "¿Qué tal, señor?";
    let mut tokens = Vec::new();
    StandardTokenizer.tokenize(text, &mut tokens);
    let expected = [
        Token {
            text: "Qué".into(),
            start: 2,
            end: 6,
            position: 0,
        },
        Token {
            text: "tal".into(),
            start: 7,
            end: 10,
            position: 1,
        },
        Token {
            text: "señor".into(),
            start: 12,
            end: 18,
            position: 2,
        },
    ];
    assert_eq!(tokens, expected);
    for token in &tokens {
        assert_eq!(text.get(token.start..token.end), Some(token.text.as_str()));
    }
}

#[test]
fn appends_and_restarts_positions() {
    let mut tokens = Vec::new();
    StandardTokenizer.tokenize("one two", &mut tokens);
    StandardTokenizer.tokenize("three", &mut tokens);
    let got: Vec<(&str, u32)> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.position))
        .collect();
    assert_eq!(got, [("one", 0), ("two", 1), ("three", 0)]);
}
