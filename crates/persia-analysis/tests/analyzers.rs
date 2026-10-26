//! Analyzer pipelines, the built-in analyzers and the registry (ROADMAP 4.4.1, 4.4.2, 4.4.8; SPEC §6.3).
#![allow(clippy::unwrap_used)] // helpers outside #[test] build fixed inputs

use std::num::NonZeroUsize;

use persia_analysis::{
    Analyzer, AnalyzerRegistry, BUILTIN_ANALYZERS, DEFAULT_ANALYZER, EdgeNgramFilter,
    LowercaseFilter, RegisterError, StandardTokenizer, SynonymMap, Token, WhitespaceTokenizer,
    builtin_analyzer, current_builtin_version,
};
use proptest::prelude::*;

#[cfg(not(feature = "slow"))]
const CASES: u32 = 256;
#[cfg(feature = "slow")]
const CASES: u32 = 10_000;

fn analyze(analyzer: &Analyzer, text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    analyzer.analyze(text, &mut tokens);
    tokens
}

fn texts(tokens: &[Token]) -> Vec<&str> {
    tokens.iter().map(|t| t.text.as_str()).collect()
}

fn builtin(name: &str) -> Analyzer {
    builtin_analyzer(name, 1).unwrap()
}

fn builtin_texts(name: &str, text: &str) -> Vec<String> {
    texts(&analyze(&builtin(name), text))
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Pins every built-in analyzer's output on a sentence that exercises each filter of its chain: changing a chain
/// fails here, and must come with a new analyzer version (SPEC §6.3).
#[test]
fn builtin_chains_are_pinned() {
    let text = "The ﬁrst Ｃafé’s Años: don't STOP-running naïve ½ caña";
    let expected: &[(&str, &[&str])] = &[
        (
            "standard",
            &[
                "the",
                "first",
                "café’s",
                "años",
                "don't",
                "stop",
                "running",
                "naïve",
                "1⁄2",
                "caña",
            ],
        ),
        ("keyword", &[text]),
        (
            "whitespace",
            &[
                "The",
                "ﬁrst",
                "Ｃafé’s",
                "Años:",
                "don't",
                "STOP-running",
                "naïve",
                "½",
                "caña",
            ],
        ),
        (
            "en",
            &[
                "first", "cafe", "ano", "don't", "stop", "run", "naiv", "1⁄2", "cana",
            ],
        ),
        (
            "es",
            &[
                "the", "first", "cafe’s", "años", "don't", "stop", "running", "naiv", "1⁄2", "cañ",
            ],
        ),
        (
            "pt",
            &[
                "the", "first", "cafe’s", "anos", "don't", "stop", "running", "naiv", "1⁄2", "can",
            ],
        ),
    ];
    for (name, want) in expected {
        assert_eq!(builtin_texts(name, text), *want, "{name}");
    }
}

#[test]
fn en_treats_typographic_apostrophes_like_ascii() {
    for (typographic, ascii) in [("John’s", "John's"), ("don‘t", "don't")] {
        assert_eq!(
            builtin_texts("en", typographic),
            builtin_texts("en", ascii),
            "{typographic}"
        );
    }
    assert_eq!(builtin_texts("en", "John’s cars"), ["john", "car"]);
}

#[test]
fn es_keeps_enye_and_folds_other_accents() {
    assert_ne!(builtin_texts("es", "año"), builtin_texts("es", "ano"));
    assert_eq!(builtin_texts("es", "AÑO"), builtin_texts("es", "año"));
    assert_eq!(
        builtin_texts("es", "canción pingüino"),
        builtin_texts("es", "cancion pinguino")
    );
    // Stopwords match with their accents, before folding.
    assert_eq!(builtin_texts("es", "más está"), Vec::<String>::new());
}

#[test]
fn pt_folds_every_accent() {
    assert_eq!(builtin_texts("pt", "não ação"), ["aca"]);
    assert_eq!(builtin_texts("pt", "AÇÃO"), builtin_texts("pt", "ação"));
}

#[test]
fn every_builtin_guards_token_length() {
    let long = "é".repeat(300);
    for a in BUILTIN_ANALYZERS {
        let tokens = analyze(&builtin(a.name), &long);
        assert_eq!(tokens.len(), 1, "{}", a.name);
        let token = tokens.first().unwrap();
        assert!(token.text.len() <= 255, "{}: {}", a.name, token.text.len());
        assert!(token.text.len() > 250, "{}: truncated, not dropped", a.name);
        assert_eq!((token.start, token.end), (0, long.len()), "{}", a.name);
    }
}

#[test]
fn builtin_list_is_sorted_unique_and_constructible() {
    assert!(BUILTIN_ANALYZERS.windows(2).all(|w| w[0] < w[1]));
    for a in BUILTIN_ANALYZERS {
        assert!(a.version >= 1);
        assert!(builtin_analyzer(a.name, a.version).is_some(), "{a:?}");
        let next = a.version + 1;
        let listed = BUILTIN_ANALYZERS
            .iter()
            .any(|b| b.name == a.name && b.version == next);
        assert_eq!(builtin_analyzer(a.name, next).is_some(), listed, "{a:?}");
    }
    assert_eq!(current_builtin_version(DEFAULT_ANALYZER), Some(1));
    for missing in ["", "fr", "de", "it", "EN", "en@1"] {
        assert_eq!(current_builtin_version(missing), None, "{missing:?}");
        assert!(builtin_analyzer(missing, 1).is_none(), "{missing:?}");
    }
    assert!(builtin_analyzer("en", 0).is_none());
    assert!(builtin_analyzer("en", 2).is_none());
}

#[test]
fn registry_resolves_builtins_by_name_and_version() {
    let registry = AnalyzerRegistry::builtin();
    for a in BUILTIN_ANALYZERS {
        let analyzer = registry.get(a.name, a.version).unwrap();
        let text = "Ｃafé’s años, naïve RUNNING";
        assert_eq!(
            analyze(analyzer, text),
            analyze(&builtin(a.name), text),
            "{a:?}"
        );
        assert_eq!(
            registry.current_version(a.name),
            current_builtin_version(a.name)
        );
    }
    assert!(registry.get("en", 2).is_none());
    assert!(registry.get("fr", 1).is_none());
    assert_eq!(registry.current_version("fr"), None);
}

#[test]
fn registry_register_rules() {
    let custom = || Analyzer::builder(WhitespaceTokenizer).build();
    let mut registry = AnalyzerRegistry::new();
    assert_eq!(registry.current_version("mine"), None);
    registry.register("mine", 2, custom()).unwrap();
    registry.register("mine", 10, custom()).unwrap();
    registry.register("mine", 1, custom()).unwrap();
    assert_eq!(registry.current_version("mine"), Some(10));
    assert_eq!(registry.versions("mine").collect::<Vec<_>>(), [1, 2, 10]);
    assert_eq!(
        registry.register("mine", 2, custom()),
        Err(RegisterError::Exists {
            name: "mine".to_owned(),
            version: 2
        })
    );
    assert_eq!(
        registry.register("", 1, custom()),
        Err(RegisterError::InvalidName(String::new()))
    );
    assert_eq!(
        registry.register("a@b", 1, custom()),
        Err(RegisterError::InvalidName("a@b".to_owned()))
    );
    assert_eq!(
        registry.register("zero", 0, custom()),
        Err(RegisterError::InvalidVersion("zero".to_owned()))
    );
    assert!(registry.get("zero", 0).is_none());
    // A built-in name is reserved at every version, released or not.
    let mut builtins = AnalyzerRegistry::builtin();
    for (name, version) in [("en", 1), ("en", 2), ("standard", 7)] {
        assert_eq!(
            builtins.register(name, version, custom()),
            Err(RegisterError::Reserved(name.to_owned()))
        );
    }
    assert!(builtins.get("en", 2).is_none());
}

#[test]
fn analyze_replaces_the_output() {
    let analyzer = builtin("standard");
    let mut tokens = analyze(&analyzer, "one two three");
    analyzer.analyze("four", &mut tokens);
    assert_eq!(texts(&tokens), ["four"]);
    assert_eq!(tokens.first().map(|t| t.position), Some(0));
    analyzer.analyze_query("", &mut tokens);
    assert_eq!(tokens, []);
}

fn ngram_analyzer() -> Analyzer {
    let ngrams = EdgeNgramFilter::new(NonZeroUsize::new(2).unwrap(), NonZeroUsize::new(4).unwrap())
        .unwrap()
        .preserve_original(true);
    Analyzer::builder(StandardTokenizer)
        .filter(LowercaseFilter)
        .filter(ngrams)
        .build()
}

/// SPEC §6.3.4: queries skip the filters that stack tokens, so a query term is matched against the stacked index
/// terms as is.
#[test]
fn query_analysis_leaves_out_stacking_filters() {
    let analyzer = ngram_analyzer();
    assert!(analyzer.stacks_tokens());
    assert_eq!(
        texts(&analyze(&analyzer, "Laptop")),
        ["la", "lap", "lapt", "laptop"]
    );
    let mut query = Vec::new();
    analyzer.analyze_query("Lapt X", &mut query);
    assert_eq!(texts(&query), ["lapt", "x"]);
    assert_eq!(query.iter().map(|t| t.position).collect::<Vec<_>>(), [0, 1]);
    for a in BUILTIN_ANALYZERS {
        assert!(!builtin(a.name).stacks_tokens(), "{a:?}");
    }
}

/// ROADMAP 4.4.8: synonym terms are analyzed like queries, so a multi-token term matches a query on a field whose
/// analyzer stacks edge n-grams.
#[test]
fn synonyms_on_a_stacking_analyzer_match_queries() {
    let analyzer = ngram_analyzer();
    let map = SynonymMap::analyzed([["New York", "NYC"]], &analyzer);
    let mut query = Vec::new();
    analyzer.analyze_query("flights to new york", &mut query);
    let matches = map.find(&query);
    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches.first().map(persia_analysis::SynonymMatch::tokens),
        Some(2..4)
    );
    let alternatives = map.alternatives(matches.first().unwrap(), 10);
    assert_eq!(
        alternatives.iter().map(|t| texts(t)).collect::<Vec<_>>(),
        [["nyc"]]
    );
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-ZñÑéÉüÜßẞİıæŒøÇçãõ’‘'\u{300}-\u{36f}ﬁＡ²⑴ΣΟΔ한 .,:\t-]{0,64}",
        "((the|a|de|la|é|que|não|running|canciones|ações|[a-z]{1,8}) ){0,16}",
        any::<String>(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// Every built-in analyzer, on any text: tokens are non-empty and within the length guard, spans are on char
    /// boundaries of the input and strictly increase with positions, and query-time analysis is the same as
    /// index-time analysis (no built-in stacks tokens).
    #[test]
    fn builtins_hold_the_token_invariants(text in text()) {
        for a in BUILTIN_ANALYZERS {
            let analyzer = builtin(a.name);
            let tokens = analyze(&analyzer, &text);
            for token in &tokens {
                prop_assert!(!token.text.is_empty() && token.text.len() <= 255, "{}: {token:?}", a.name);
                prop_assert!(token.start < token.end && token.end <= text.len(), "{}: {token:?}", a.name);
                prop_assert!(text.is_char_boundary(token.start) && text.is_char_boundary(token.end));
            }
            for pair in tokens.windows(2) {
                prop_assert!(pair[0].position < pair[1].position, "{}: {pair:?}", a.name);
                prop_assert!(pair[0].end <= pair[1].start, "{}: {pair:?}", a.name);
            }
            let mut query = Vec::new();
            analyzer.analyze_query(&text, &mut query);
            prop_assert_eq!(&query, &tokens, "{}", a.name);
        }
    }
}

/// Part of the pinned definitions: these words stem differently when `fold` runs before `stem`, so reordering a
/// language chain fails here (SPEC §6.3.3).
#[test]
fn language_chains_stem_before_folding() {
    let expected: &[(&str, &str, &[&str])] = &[
        ("es", "después cafés", &["despues", "cafes"]),
        ("pt", "maçã", &["maca"]),
        ("en", "résumés naïveté", &["resume", "naivete"]),
    ];
    for (name, text, want) in expected {
        assert_eq!(builtin_texts(name, text), *want, "{name}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// SPEC §6.3.4: with `preserve_original`, every query-time token of a text is among its index-time tokens at the
    /// same position, so a query for an indexed word always finds it as is.
    #[test]
    fn query_tokens_are_among_stacked_index_tokens(text in text()) {
        let analyzer = ngram_analyzer();
        let index = analyze(&analyzer, &text);
        let mut query = Vec::new();
        analyzer.analyze_query(&text, &mut query);
        for token in &query {
            prop_assert!(
                index.iter().any(|t| t.text == token.text && t.position == token.position),
                "{token:?} not indexed"
            );
        }
    }
}
