//! Token filter tables (ROADMAP 4.2; SPEC §6.3.2).
#![allow(clippy::unwrap_used)] // helpers outside #[test] build fixed inputs

use std::num::NonZeroUsize;

use persia_analysis::{
    ENGLISH_STOPWORDS, EdgeNgramFilter, FoldingFilter, LowercaseFilter, NfkcFilter,
    PORTUGUESE_STOPWORDS, SPANISH_STOPWORDS, StandardTokenizer, StopwordFilter, Token, TokenFilter,
    Tokenizer, WhitespaceTokenizer,
};
use unicode_normalization::is_nfc;

fn tokens(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    WhitespaceTokenizer.tokenize(text, &mut tokens);
    tokens
}

fn texts(filter: &dyn TokenFilter, text: &str) -> Vec<String> {
    let mut tokens = tokens(text);
    filter.apply(&mut tokens);
    tokens.into_iter().map(|t| t.text).collect()
}

fn one(filter: &dyn TokenFilter, text: &str) -> String {
    let mut out = texts(filter, text);
    assert_eq!(out.len(), 1, "{text:?} -> {out:?}");
    out.remove(0)
}

fn n(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

#[test]
fn nfkc_table() {
    for (input, expected) in [
        ("plain", "plain"),
        ("ﬁle", "file"),
        ("Ｗｉｆｉ", "Wifi"),
        ("x²", "x2"),
        ("Å", "Å"),           // U+212B ANGSTROM SIGN → U+00C5
        ("e\u{301}", "é"),    // composed
        ("ｶﾀｶﾅ", "カタカナ"), // half-width katakana
        ("⑴", "(1)"),         // not re-tokenized
        ("½", "1⁄2"),
        ("\u{fdfa}", "صلى الله عليه وسلم"), // 18 characters from one (SPEC §6.3.2 length note)
    ] {
        assert_eq!(one(&NfkcFilter, input), expected, "{input:?}");
    }
}

#[test]
fn nfkc_keeps_spans_and_positions() {
    let mut tokens = tokens("ﬁ Ａ");
    let before: Vec<_> = tokens
        .iter()
        .map(|t| (t.start, t.end, t.position))
        .collect();
    NfkcFilter.apply(&mut tokens);
    let after: Vec<_> = tokens
        .iter()
        .map(|t| (t.start, t.end, t.position))
        .collect();
    assert_eq!(before, after);
}

#[test]
fn lowercase_table() {
    for (input, expected) in [
        ("ABC", "abc"),
        ("already", "already"),
        ("Ñandú", "ñandú"),
        ("ß", "ß"),           // lowercasing is not case folding
        ("STRAẞE", "straße"), // capital sharp s
        ("ΟΔΟΣ", "οδος"),     // Final_Sigma inside the token
        ("ΣΑΣ", "σας"),
        ("I", "i"), // no Turkish tailoring
        ("İ", "i\u{307}"),
        ("ı", "ı"),
        ("ǅ", "ǆ"), // titlecase digraph
        ("ПРИВЕТ", "привет"),
        ("Ⅻ", "ⅻ"), // roman numeral, not a letter
        ("42", "42"),
    ] {
        assert_eq!(one(&LowercaseFilter, input), expected, "{input:?}");
    }
}

#[test]
fn folding_table() {
    let fold = FoldingFilter::default();
    for (input, expected) in [
        ("café", "cafe"),
        ("pingüino", "pinguino"),
        ("año", "ano"),
        ("Ñ", "N"),
        ("français", "francais"),
        ("ǖ", "u"),        // two marks
        ("e\u{301}", "e"), // already decomposed
        ("ệ", "e"),        // Vietnamese
        ("ŠKODA", "SKODA"),
        ("Straße", "Strasse"),
        ("ẞ", "ss"),
        ("æsir", "aesir"),
        ("Œuvre", "OEuvre"),
        ("Ørsted", "Orsted"),
        ("łódź", "lodz"),
        ("Đorđe", "Dorde"),
        ("þorn", "thorn"),
        ("ı", "i"),
        ("i\u{307}", "i"),    // the lowercase of İ
        ("ά", "α"),           // Greek tonos
        ("ё", "е"),           // Cyrillic
        ("한국어", "한국어"), // Hangul recomposes
        ("क़", "क़"),           // a Devanagari nukta is not U+0300–U+036F
        ("ascii", "ascii"),
    ] {
        let folded = one(&fold, input);
        assert_eq!(folded, expected, "{input:?}");
        assert!(is_nfc(&folded), "{input:?} -> {folded:?} is not NFC");
    }
}

#[test]
fn folding_preserves_listed_characters_only() {
    let fold = FoldingFilter::preserving(['ñ', 'ñ', 'ü']);
    assert_eq!(fold.preserved(), ['ñ', 'ü']);
    assert_eq!(
        texts(&fold, "año Ñandú pingüino"),
        ["año", "Nandu", "pingüino"]
    );
}

#[test]
fn folding_preserves_listed_characters_inside_a_decomposition() {
    // `Ǣ` is `Æ` and a macron; `ü` is `u` and a diaeresis.
    assert_eq!(one(&FoldingFilter::preserving(['Æ']), "Ǣ"), "Æ");
    assert_eq!(
        one(&FoldingFilter::preserving(['\u{308}']), "ü"),
        "u\u{308}"
    );
    // A preserved character is not recomposed: U+0344 is not NFC, and NFC would split it into two marks.
    assert_eq!(
        one(&FoldingFilter::preserving(['\u{344}']), "`\u{344}"),
        "`\u{344}"
    );
    // Only composed input matches a preserved `ñ` (SPEC §6.3.2: `nfkc` goes first).
    assert_eq!(one(&FoldingFilter::preserving(['ñ']), "n\u{303}"), "n");
}

/// The folding table is part of every analyzer that uses it (SPEC §6.3.2): this fingerprint of the fold of every
/// scalar value changes with any edit of the table or of the Unicode tables, and updating it means bumping the
/// versions of those analyzers.
#[test]
fn folding_is_pinned() {
    let mut tokens: Vec<Token> = (0..=u32::from(char::MAX))
        .filter_map(char::from_u32)
        .zip(0..)
        .map(|(c, position)| Token {
            text: c.to_string(),
            start: 0,
            end: c.len_utf8(),
            position,
        })
        .collect();
    FoldingFilter::default().apply(&mut tokens);
    let fingerprint = tokens
        .iter()
        .flat_map(|t| {
            t.position
                .to_le_bytes()
                .into_iter()
                .chain(t.text.bytes())
                .chain([0xff])
        })
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    assert_eq!(
        (tokens.len(), fingerprint),
        (1_111_952, 13_066_509_306_887_631_778)
    );
}

#[test]
fn folding_drops_a_token_of_marks_only() {
    let mut tokens = tokens("a \u{301}\u{308} b");
    FoldingFilter::default().apply(&mut tokens);
    let kept: Vec<_> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.position))
        .collect();
    assert_eq!(kept, [("a", 0), ("b", 2)]);
}

#[test]
fn stopwords_leave_position_gaps() {
    let mut tokens = tokens("the cat and the hat");
    StopwordFilter::english().apply(&mut tokens);
    let kept: Vec<_> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.position))
        .collect();
    assert_eq!(kept, [("cat", 1), ("hat", 4)]);
}

#[test]
fn stopwords_match_exactly() {
    // Case and accents matter: the embedded lists expect lowercased, unfolded text.
    assert_eq!(texts(&StopwordFilter::english(), "The the"), ["The"]);
    assert_eq!(
        texts(&StopwordFilter::spanish(), "él el ella"),
        Vec::<String>::new()
    );
    assert_eq!(texts(&StopwordFilter::spanish(), "mas más"), ["mas"]);
    assert_eq!(
        texts(&StopwordFilter::portuguese(), "não é você"),
        Vec::<String>::new()
    );
    // Words as often used as content words stay.
    assert_eq!(
        texts(&StopwordFilter::spanish(), "bajo ante era"),
        ["bajo", "ante", "era"]
    );
    assert_eq!(texts(&StopwordFilter::portuguese(), "era"), ["era"]);
    assert_eq!(
        texts(&StopwordFilter::new(["foo"]), "foo bar Foo"),
        ["bar", "Foo"]
    );
    assert_eq!(texts(&StopwordFilter::default(), "a the"), ["a", "the"]);
}

#[test]
fn embedded_stopword_lists_are_sorted_unique_lowercase_nfc() {
    for list in [ENGLISH_STOPWORDS, SPANISH_STOPWORDS, PORTUGUESE_STOPWORDS] {
        assert!(
            list.windows(2).all(|w| w[0] < w[1]),
            "not sorted and unique: {list:?}"
        );
        for word in list {
            assert!(!word.is_empty() && is_nfc(word), "{word:?}");
            assert_eq!(word.to_lowercase(), *word);
            // One token of the standard tokenizer, so a stopword can match a standard token.
            let mut tokens = Vec::new();
            StandardTokenizer.tokenize(word, &mut tokens);
            assert_eq!(tokens.len(), 1, "{word:?}");
        }
    }
}

/// The lists are part of every analyzer that uses them (SPEC §6.3.2): this fingerprint changes with any edit, and
/// updating it means bumping the versions of those analyzers.
#[test]
fn embedded_stopword_lists_are_pinned() {
    fn fnv1a(list: &[&str]) -> u64 {
        list.iter()
            .flat_map(|w| w.bytes().chain([0]))
            .fold(0xcbf2_9ce4_8422_2325, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            })
    }
    let fingerprints =
        [ENGLISH_STOPWORDS, SPANISH_STOPWORDS, PORTUGUESE_STOPWORDS].map(|l| (l.len(), fnv1a(l)));
    assert_eq!(
        fingerprints,
        [
            (33, 14_029_973_542_677_213_976),
            (88, 15_764_952_924_425_316_783),
            (82, 7_466_230_536_459_025_178)
        ]
    );
}

#[test]
fn edge_ngrams_stack_on_the_token() {
    let filter = EdgeNgramFilter::new(n(2), n(4)).unwrap();
    let mut tokens = tokens("a añora xyz");
    filter.apply(&mut tokens);
    let out: Vec<_> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.start, t.end, t.position))
        .collect();
    assert_eq!(
        out,
        [
            ("añ", 2, 8, 1),
            ("año", 2, 8, 1),
            ("añor", 2, 8, 1),
            ("xy", 9, 12, 2),
            ("xyz", 9, 12, 2),
        ]
    );
}

#[test]
fn edge_ngrams_preserve_original_only_when_not_a_prefix() {
    let filter = EdgeNgramFilter::new(n(2), n(3))
        .unwrap()
        .preserve_original(true);
    assert!(filter.preserves_original());
    assert_eq!(
        texts(&filter, "a ab abc abcd"),
        ["a", "ab", "ab", "abc", "ab", "abc", "abcd"]
    );
}

#[test]
fn edge_ngram_bounds() {
    assert!(EdgeNgramFilter::new(n(3), n(2)).is_none());
    let filter = EdgeNgramFilter::new(n(1), n(1)).unwrap();
    assert_eq!(
        (filter.min(), filter.max(), filter.preserves_original()),
        (n(1), n(1), false)
    );
    assert_eq!(texts(&filter, "中文 b"), ["中", "b"]);
}
