//! Fuzz the analyzers (ROADMAP 4.4.5, 4.4.7; SPEC §6.3): arbitrary UTF-8 never panics in any built-in analyzer, at
//! index or query time; tokens are non-empty, within the length guard, on char boundaries of the value, with strictly
//! increasing positions and disjoint, increasing spans. The same text through a char filter driven by the input
//! (the first byte: which characters it replaces, and with what) keeps spans on char boundaries of the original
//! value, with starts and ends that never decrease (§6.3.1).
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_analysis::{
    Analyzer, BUILTIN_ANALYZERS, CharFilter, CharFilterOutput, LowercaseFilter, StandardTokenizer,
    Token, builtin_analyzer,
};

/// Replaces each character whose code point has `mask` bits in common with `control` by one of a few strings,
/// chosen by the character: deletion, expansion to several words, multi-byte text.
struct Driven {
    control: u8,
}

impl CharFilter for Driven {
    fn filter(&self, out: &mut CharFilterOutput<'_>) {
        const REPLACEMENTS: [&str; 4] = ["", " x y ", "é\u{301}中", "ab"];
        let mask = self.control & 0x3f;
        while let Some(c) = out.remaining().chars().next() {
            let code = u32::from(c);
            if code & u32::from(mask) != 0 {
                let pick = usize::try_from(code % 4).unwrap();
                // A length of 1 byte also checks that a piece is widened to a whole character.
                let len = if self.control & 0x40 == 0 {
                    c.len_utf8()
                } else {
                    1
                };
                out.replace(len, REPLACEMENTS[pick]);
            } else {
                out.keep(c.len_utf8());
            }
            if self.control & 0x80 != 0 && code % 7 == 0 {
                // Insertions between characters.
                out.replace(0, "-");
            }
        }
    }
}

fn check_builtin(name: &str, text: &str, tokens: &[Token]) {
    for token in tokens {
        assert!(
            !token.text.is_empty() && token.text.len() <= 255,
            "{name}: {token:?}"
        );
        assert!(
            token.start < token.end && text.get(token.start..token.end).is_some(),
            "{name}: {token:?}"
        );
    }
    for pair in tokens.windows(2) {
        assert!(
            pair[0].position < pair[1].position && pair[0].end <= pair[1].start,
            "{name}: {pair:?}"
        );
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&control, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let mut tokens = Vec::new();
    let mut query = Vec::new();
    for a in BUILTIN_ANALYZERS {
        let analyzer = builtin_analyzer(a.name, a.version).expect("listed built-ins exist");
        analyzer.analyze(text, &mut tokens);
        check_builtin(a.name, text, &tokens);
        // No built-in stacks tokens: query-time analysis is the same (SPEC §6.3.4).
        analyzer.analyze_query(text, &mut query);
        assert_eq!(query, tokens, "{}", a.name);
    }
    let analyzer = Analyzer::builder(StandardTokenizer)
        .char_filter(Driven { control })
        .char_filter(Driven {
            control: control.rotate_left(3),
        })
        .filter(LowercaseFilter)
        .build();
    analyzer.analyze(text, &mut tokens);
    for token in &tokens {
        assert!(token.start <= token.end, "{token:?}");
        assert!(
            text.get(token.start..token.end).is_some(),
            "off a char boundary: {token:?}"
        );
    }
    for pair in tokens.windows(2) {
        assert!(
            pair[0].start <= pair[1].start && pair[0].end <= pair[1].end,
            "{pair:?}"
        );
        assert!(pair[0].position < pair[1].position, "{pair:?}");
    }
});
