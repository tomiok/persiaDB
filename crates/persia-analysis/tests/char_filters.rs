//! Char filters and the mapping of token spans back to the original value (ROADMAP 4.4.7; SPEC §6.3, §6.3.1).
#![allow(clippy::unwrap_used)] // helpers outside #[test] build fixed inputs

use persia_analysis::{
    Analyzer, CharFilter, CharFilterOutput, LowercaseFilter, StandardTokenizer, Token, TokenStream,
    WhitespaceTokenizer,
};
use proptest::prelude::*;

#[cfg(not(feature = "slow"))]
const CASES: u32 = 512;
#[cfg(feature = "slow")]
const CASES: u32 = 20_000;

fn analyze(analyzer: &Analyzer, text: &str) -> TokenStream {
    let mut tokens = TokenStream::new();
    analyzer.analyze(text, &mut tokens);
    tokens
}

/// `(text, original span)` of every token.
fn spans<'a>(original: &'a str, tokens: &'a [Token]) -> Vec<(&'a str, &'a str)> {
    tokens
        .iter()
        .map(|t| (t.text.as_str(), original.get(t.start..t.end).unwrap()))
        .collect()
}

/// Replaces every match of `from` with `to`.
struct Replace {
    from: &'static str,
    to: &'static str,
}

impl CharFilter for Replace {
    fn filter(&self, out: &mut CharFilterOutput<'_>) {
        while let Some(at) = out.remaining().find(self.from) {
            out.keep(at);
            out.replace(self.from.len(), self.to);
        }
    }
}

/// Upper-cases each character on its own, so pieces change length (`ß` → `SS`, `ŉ` → `ʼN`).
struct UpperEachChar;

impl CharFilter for UpperEachChar {
    fn filter(&self, out: &mut CharFilterOutput<'_>) {
        let mut buf = String::new();
        while let Some(c) = out.remaining().chars().next() {
            buf.clear();
            buf.extend(c.to_uppercase());
            out.replace(c.len_utf8(), &buf);
        }
    }
}

#[test]
fn char_filters_run_in_order_before_the_tokenizer() {
    // Added after the token filter: char filters still run first, and the second sees the first one's output.
    let analyzer = Analyzer::builder(WhitespaceTokenizer)
        .filter(LowercaseFilter)
        .char_filter(Replace { from: "-", to: " " })
        .char_filter(Replace { from: " ", to: "_" })
        .build();
    let tokens = analyze(&analyzer, "well-known Fact");
    assert_eq!(
        spans("well-known Fact", &tokens),
        [("well_known_fact", "well-known Fact")]
    );
}

#[test]
fn spans_index_the_original_value() {
    let text = "Tom &amp; Jerry &amp;&amp; co";
    let analyzer = Analyzer::builder(WhitespaceTokenizer)
        .char_filter(Replace {
            from: "&amp;",
            to: "&",
        })
        .build();
    let tokens = analyze(&analyzer, text);
    assert_eq!(
        spans(text, &tokens),
        [
            ("Tom", "Tom"),
            ("&", "&amp;"),
            ("Jerry", "Jerry"),
            ("&&", "&amp;&amp;"),
            ("co", "co"),
        ]
    );
}

#[test]
fn deleted_text_is_outside_spans() {
    // `<b>` and `</b>` are deleted: `bold` spans only its letters, though the tokenizer saw `xboldy`.
    let text = "x<b>bold</b>y";
    let analyzer = Analyzer::builder(WhitespaceTokenizer)
        .char_filter(Replace {
            from: "<b>",
            to: "",
        })
        .char_filter(Replace {
            from: "</b>",
            to: "",
        })
        .build();
    let tokens = analyze(&analyzer, text);
    assert_eq!(spans(text, &tokens), [("xboldy", "x<b>bold</b>y")]);
    let analyzer = Analyzer::builder(WhitespaceTokenizer)
        .char_filter(Replace {
            from: "<b>",
            to: " ",
        })
        .char_filter(Replace {
            from: "</b>",
            to: "",
        })
        .build();
    let tokens = analyze(&analyzer, text);
    assert_eq!(spans(text, &tokens), [("x", "x"), ("boldy", "bold</b>y")]);
}

#[test]
fn words_from_one_replacement_share_its_span() {
    let text = "a½b";
    let analyzer = Analyzer::builder(StandardTokenizer)
        .char_filter(Replace {
            from: "½",
            to: " one half ",
        })
        .build();
    let tokens = analyze(&analyzer, text);
    assert_eq!(
        spans(text, &tokens),
        [("a", "a"), ("one", "½"), ("half", "½"), ("b", "b")]
    );
    assert_eq!(
        tokens.iter().map(|t| t.position).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
}

#[test]
fn chained_maps_compose() {
    // `ß` → `SS` by the first filter, then `SS` → `S S` by the second: each `S` comes from the one `ß`.
    let text = "aßb";
    let analyzer = Analyzer::builder(WhitespaceTokenizer)
        .char_filter(UpperEachChar)
        .char_filter(Replace {
            from: "SS",
            to: "S S",
        })
        .build();
    let tokens = analyze(&analyzer, text);
    assert_eq!(spans(text, &tokens), [("AS", "aß"), ("SB", "ßb")]);
}

/// Inserts `text` at byte `at`.
struct Insert {
    at: usize,
    text: &'static str,
}

impl CharFilter for Insert {
    fn filter(&self, out: &mut CharFilterOutput<'_>) {
        out.keep(self.at);
        out.replace(0, self.text);
    }
}

#[test]
fn an_empty_span_on_a_deletion_takes_the_position_before_it() {
    // `-` is deleted, then ` XY ` inserted where it was: `XY` has no original characters, and the deletion would send
    // its empty span's start past the `-` and its end before it; it is the empty span before the `-` (SPEC §6.3.1).
    let text = "a -b";
    let analyzer = Analyzer::builder(WhitespaceTokenizer)
        .char_filter(Replace { from: "-", to: "" })
        .char_filter(Insert {
            at: 2,
            text: " XY ",
        })
        .build();
    let tokens = analyze(&analyzer, text);
    let got: Vec<(&str, usize, usize)> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.start, t.end))
        .collect();
    assert_eq!(got, [("a", 0, 1), ("XY", 2, 2), ("b", 3, 4)]);
}

/// A char filter driven by generated edits: per character, keep it, delete it, or replace it with a string.
#[derive(Debug, Clone)]
struct Scripted {
    /// Per character class (`c as u32 % len`), what replaces it; `None` keeps it.
    rules: Vec<Option<String>>,
}

impl CharFilter for Scripted {
    fn filter(&self, out: &mut CharFilterOutput<'_>) {
        while let Some(c) = out.remaining().chars().next() {
            let rule = self.rules.get(c as usize % self.rules.len()).unwrap();
            match rule {
                Some(replacement) => out.replace(c.len_utf8(), replacement),
                None => out.keep(c.len_utf8()),
            }
        }
    }
}

fn scripted() -> impl Strategy<Value = Scripted> {
    let rule = prop_oneof![
        Just(None),
        Just(Some(String::new())),
        "[a-z é\u{301}中]{1,4}".prop_map(Some),
    ];
    prop::collection::vec(rule, 1..6).prop_map(|rules| Scripted { rules })
}

/// A char filter that consumes arbitrary byte lengths, mid-character ones and ones past the end included.
#[derive(Debug, Clone)]
struct Lengths {
    steps: Vec<(bool, usize, String)>,
}

impl CharFilter for Lengths {
    fn filter(&self, out: &mut CharFilterOutput<'_>) {
        for (keep, len, replacement) in &self.steps {
            if *keep {
                out.keep(*len);
            } else {
                out.replace(*len, replacement);
            }
        }
    }
}

fn lengths() -> impl Strategy<Value = Lengths> {
    prop::collection::vec((any::<bool>(), 0usize..8, "[a-z ñ]{0,3}"), 0..12)
        .prop_map(|steps| Lengths { steps })
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof!["[a-zA-Zé\u{301}ß中 .,-]{0,48}", any::<String>(),]
}

/// Spans lie on char boundaries of `text`, with `start <= end`, and both starts and ends never decrease.
fn assert_spans_well_formed(text: &str, tokens: &[Token]) -> Result<(), TestCaseError> {
    for token in tokens {
        prop_assert!(
            token.start <= token.end && token.end <= text.len(),
            "{token:?}"
        );
        prop_assert!(
            text.is_char_boundary(token.start) && text.is_char_boundary(token.end),
            "{token:?}"
        );
    }
    for (a, b) in tokens.iter().zip(tokens.iter().skip(1)) {
        prop_assert!(a.start <= b.start && a.end <= b.end, "{a:?} {b:?}");
        prop_assert!(a.position < b.position, "{a:?} {b:?}");
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// SPEC §6.3.1: whatever char filters do, spans index the original value on its character boundaries and
    /// never go backwards.
    #[test]
    fn mapped_spans_are_well_formed(text in text(), first in scripted(), second in lengths()) {
        let analyzer = Analyzer::builder(StandardTokenizer)
            .char_filter(first.clone())
            .char_filter(second.clone())
            .build();
        assert_spans_well_formed(&text, &analyze(&analyzer, &text))?;
        let analyzer = Analyzer::builder(WhitespaceTokenizer).char_filter(second).char_filter(first).build();
        assert_spans_well_formed(&text, &analyze(&analyzer, &text))?;
    }

    /// When the filtered text keeps the original's words (each character replaced by its upper case, which adds no
    /// whitespace), each token spans exactly its original word, strictly after the previous one.
    #[test]
    fn spans_cover_exactly_the_original_words(text in "[a-zß\u{149}éΣ中 ]{0,48}") {
        let analyzer = Analyzer::builder(WhitespaceTokenizer).char_filter(UpperEachChar).build();
        let tokens = analyze(&analyzer, &text);
        let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        let originals: Vec<&str> = tokens.iter().map(|t| text.get(t.start..t.end).unwrap()).collect();
        prop_assert_eq!(originals, words);
        for pair in tokens.windows(2) {
            prop_assert!(pair[0].end < pair[1].start, "{pair:?}");
        }
    }

    /// A filter that keeps everything changes nothing: spans equal the tokenizer's.
    #[test]
    fn keeping_filters_change_nothing(text in text()) {
        let plain = Analyzer::builder(StandardTokenizer).build();
        let kept = Analyzer::builder(StandardTokenizer)
            .char_filter(Scripted { rules: vec![None] })
            .char_filter(Lengths { steps: vec![(true, 3, String::new()), (false, 0, String::new())] })
            .build();
        prop_assert_eq!(analyze(&kept, &text), analyze(&plain, &text));
    }
}
