//! Analyzer throughput in tokens per second (ROADMAP 4.4.5; SPEC §6.3): every built-in analyzer on text in its
//! language, and the folding filter on text with and without anything to fold (ROADMAP 4.4.9).
//! Baseline numbers are recorded in `docs/benchmarks.md`.
//! Run: `cargo bench -p persia-analysis --bench analyzers`.

#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use persia_analysis::{
    BUILTIN_ANALYZERS, ENGLISH_STOPWORDS, FoldingFilter, KeywordTokenizer, PORTUGUESE_STOPWORDS,
    SPANISH_STOPWORDS, StandardTokenizer, TokenFilter, TokenStream, Tokenizer, WhitespaceTokenizer,
    builtin_analyzer,
};

const ENGLISH: &str = include_str!("../tests/fixtures/stem/english.tsv");
const SPANISH: &str = include_str!("../tests/fixtures/stem/spanish.tsv");
const PORTUGUESE: &str = include_str!("../tests/fixtures/stem/portuguese.tsv");

/// Words per field value, and values per iteration.
const WORDS_PER_VALUE: usize = 12;
const VALUES: usize = 256;

/// A fixed LCG, so every run analyzes the same text without an RNG dependency.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(self.0 >> 33).unwrap() % n
    }
}

/// Field values of `WORDS_PER_VALUE` words: vocabulary words with a stopword every third word on average, a
/// capitalized word now and then, and punctuation, like prose.
fn values(vocabulary: &str, stopwords: &[&str], seed: u64) -> Vec<String> {
    let words: Vec<&str> = vocabulary
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(word, _)| word))
        .collect();
    let mut rng = Lcg(seed);
    (0..VALUES)
        .map(|_| {
            let mut value = String::new();
            for i in 0..WORDS_PER_VALUE {
                if i > 0 {
                    value.push_str(if rng.below(8) == 0 { ", " } else { " " });
                }
                let word = if rng.below(3) == 0 {
                    stopwords.get(rng.below(stopwords.len())).unwrap()
                } else {
                    words.get(rng.below(words.len())).unwrap()
                };
                if rng.below(6) == 0 {
                    let mut chars = word.chars();
                    value.extend(chars.next().into_iter().flat_map(char::to_uppercase));
                    value.push_str(chars.as_str());
                } else {
                    value.push_str(word);
                }
            }
            value.push('.');
            value
        })
        .collect()
}

/// Text in the analyzer's language: the vocabulary of its stemmer, or English for the others.
fn corpus(analyzer: &str) -> Vec<String> {
    match analyzer {
        "es" => values(SPANISH, SPANISH_STOPWORDS, 2),
        "pt" => values(PORTUGUESE, PORTUGUESE_STOPWORDS, 3),
        _ => values(ENGLISH, ENGLISH_STOPWORDS, 1),
    }
}

/// How many tokens `tokenizer` emits for `values`: the work an analyzer does per iteration.
fn token_count(tokenizer: &dyn Tokenizer, values: &[String]) -> u64 {
    let mut tokens = TokenStream::new();
    values
        .iter()
        .map(|v| {
            tokens.clear();
            tokenizer.tokenize(v, &mut tokens);
            u64::try_from(tokens.len()).unwrap()
        })
        .sum()
}

fn analyzers(c: &mut Criterion) {
    let mut group = c.benchmark_group("analyze");
    for a in BUILTIN_ANALYZERS {
        let analyzer = builtin_analyzer(a.name, a.version).unwrap();
        let values = corpus(a.name);
        // Tokens in, per iteration: what the analyzer's tokenizer emits (one per value for `keyword`).
        let tokenizer: &dyn Tokenizer = match a.name {
            "keyword" => &KeywordTokenizer,
            "whitespace" => &WhitespaceTokenizer,
            _ => &StandardTokenizer,
        };
        let tokens = token_count(tokenizer, &values);
        group.throughput(Throughput::Elements(tokens));
        group.bench_with_input(BenchmarkId::new(a.name, a.version), &values, |b, values| {
            let mut out = TokenStream::new();
            b.iter(|| {
                for value in values {
                    analyzer.analyze(black_box(value), &mut out);
                    black_box(&out);
                }
            });
        });
    }
    group.finish();
}

/// Lowercased tokens for the folding filter: ASCII (nothing to fold), Spanish (some accents) and non-ASCII text
/// with nothing to fold, the case a pre-scan speeds up (ROADMAP 4.4.9).
fn fold_inputs() -> Vec<(&'static str, TokenStream)> {
    let tokenize = |values: Vec<String>| {
        let mut all = TokenStream::new();
        for v in &values {
            StandardTokenizer.tokenize(&v.to_lowercase(), &mut all);
        }
        all
    };
    let non_ascii_unfoldable: Vec<String> =
        ["日本語のテキスト", "привет мир", "año niño", "ελληνικη"]
            .iter()
            .cycle()
            .take(VALUES)
            .map(|s| (*s).to_owned())
            .collect();
    vec![
        ("ascii", tokenize(values(ENGLISH, ENGLISH_STOPWORDS, 4))),
        ("spanish", tokenize(values(SPANISH, SPANISH_STOPWORDS, 5))),
        ("non_ascii_unfoldable", tokenize(non_ascii_unfoldable)),
    ]
}

fn folding(c: &mut Criterion) {
    let mut group = c.benchmark_group("fold");
    let filter = FoldingFilter::preserving(['ñ']);
    for (name, tokens) in fold_inputs() {
        group.throughput(Throughput::Elements(u64::try_from(tokens.len()).unwrap()));
        group.bench_with_input(BenchmarkId::from_parameter(name), &tokens, |b, tokens| {
            // A fresh copy per iteration, made outside the timed part.
            b.iter_batched_ref(
                || tokens.clone(),
                |work| filter.apply(black_box(work)),
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, analyzers, folding);
criterion_main!(benches);
