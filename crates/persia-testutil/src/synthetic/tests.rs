//! The generator's own guarantees (ROADMAP 8.4.1, SPEC §7.6 L2 `drop_terms` and L4 `fuzzy`, §7.1 OSA edits).
//!
//! Invariants, for every query of every seed tried:
//! 1. Deterministic: a seed gives the same corpus and the same queries; [`QueryGenerator::query`] with a query's
//!    `seed` gives that query again; `generate(s, n)` is a prefix of `generate(s, m)` for `n <= m`; different seeds
//!    give different corpora and suites. Every assertion message names the seeds that rebuild the failure.
//! 2. The intended words are distinct words of the target, in their first-occurrence order, and the ground truth is
//!    exactly the documents holding them all (the target among them), checked against a naive scan of the documents.
//! 3. As typed, the query matches no document with every word required.
//! 4. A typo is one optimal-string-alignment edit from the word it replaced (an oracle written from the definition, not
//!    [`edit`]), has at least [`MIN_TYPO_LEN`] letters, is held by no document, and replaces a word of more than
//!    [`MIN_TYPO_LEN`] letters.
//! 5. An added word is not intended, is not the typo, and no document holds it beside every intended word; the text is
//!    the intended words with the typo in place and the added word inserted, one space apart.
//! 6. Kind weights select the kind: `[1, 0, 0]` gives only typos, `[0, 1, 0]` only drops, `[0, 0, 1]` only both; a zero
//!    total gives [`QueryKind::TypoAndDrop`] (pinned) and saturating weights do not panic.
//! 7. Corpora of odd texts (empty, digits, non-ASCII, punctuation, one word, repeated words) never panic, the generator
//!    returns (bounded tries), and every query it returns holds 2 to 5.
//! 8. [`Rng`] is total: `below(0)`, `within` on empty or one-value ranges and `pick` on an empty slice return their
//!    documented values.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)] // Tests: inputs are fixed.

use super::*;
use proptest::prelude::{
    ProptestConfig, Strategy, any, prop, prop_assert, prop_assert_eq, proptest,
};
use proptest::test_runner::RngSeed;

/// The optimal string alignment distance over bytes (ASCII words), written from its definition: an oracle independent
/// of [`edit`].
fn osa(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

/// The ids of the documents holding every one of `words`, by scanning each document: an oracle independent of the
/// corpus's postings.
fn naive_matching<S: AsRef<str>>(corpus: &Corpus, words: &[S]) -> Vec<String> {
    corpus
        .docs()
        .iter()
        .filter(|d| {
            words
                .iter()
                .all(|w| d.words.iter().any(|x| x == w.as_ref()))
        })
        .map(|d| d.id.clone())
        .collect()
}

/// Whether some document holds `word`, by scanning.
fn naive_holds(corpus: &Corpus, word: &str) -> bool {
    corpus
        .docs()
        .iter()
        .any(|d| d.words.iter().any(|w| w == word))
}

/// Invariants 2 to 5 for `q`. `ctx` names the seeds (corpus, suite) so a failure can be rebuilt; `q` carries its own.
fn check(ctx: &str, corpus: &Corpus, q: &SyntheticQuery) {
    let target = corpus
        .docs()
        .iter()
        .find(|d| d.id == q.target)
        .unwrap_or_else(|| panic!("{ctx}: target not in the corpus: {q:?}"));
    // 2. Distinct words of the target, in first-occurrence order (strictly increasing first positions implies both).
    assert!(!q.intended.is_empty(), "{ctx}: no intended word: {q:?}");
    let firsts: Vec<usize> = q
        .intended
        .iter()
        .map(|w| {
            target
                .words
                .iter()
                .position(|t| t == w)
                .unwrap_or_else(|| panic!("{ctx}: intended {w:?} not in the target: {q:?}"))
        })
        .collect();
    assert!(
        firsts.windows(2).all(|p| p[0] < p[1]),
        "{ctx}: intended words not distinct in first-occurrence order {firsts:?}: {q:?}"
    );
    assert_eq!(q.relevant, corpus.matching(&q.intended), "{ctx}: {q:?}");
    assert_eq!(
        q.relevant,
        naive_matching(corpus, &q.intended),
        "{ctx}: {q:?}"
    );
    assert!(
        q.is_relevant(&q.target),
        "{ctx}: target not relevant: {q:?}"
    );
    // 3.
    let typed: Vec<&str> = q.text.split(' ').collect();
    assert!(
        typed.iter().all(|w| !w.is_empty()
            && w.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())),
        "{ctx}: a typed word is empty or not lowercase ASCII: {q:?}"
    );
    assert_eq!(
        naive_matching(corpus, &typed),
        Vec::<String>::new(),
        "{ctx}: matches as typed: {q:?}"
    );
    // 4.
    let mut want = q.intended.clone();
    match (&q.typo, q.kind) {
        (Some((typo, meant)), QueryKind::Typo | QueryKind::TypoAndDrop) => {
            assert_eq!(osa(typo, meant), 1, "{ctx}: typo not one edit: {q:?}");
            assert!(typo.len() >= MIN_TYPO_LEN, "{ctx}: typo too short: {q:?}");
            assert!(
                meant.len() > MIN_TYPO_LEN,
                "{ctx}: a word of {} letters misspelled: {q:?}",
                meant.len()
            );
            assert!(
                !corpus.holds(typo) && !naive_holds(corpus, typo),
                "{ctx}: typo held by a document: {q:?}"
            );
            let at = want
                .iter()
                .position(|w| w == meant)
                .unwrap_or_else(|| panic!("{ctx}: typo replaces no intended word: {q:?}"));
            want[at].clone_from(typo);
        }
        (None, QueryKind::Drop) => {}
        other => panic!("{ctx}: typo {other:?} for {q:?}"),
    }
    // 5.
    match (&q.added, q.kind) {
        (Some(added), QueryKind::Drop | QueryKind::TypoAndDrop) => {
            assert!(
                !q.intended.contains(added),
                "{ctx}: added is intended: {q:?}"
            );
            assert!(
                q.typo.as_ref().is_none_or(|(typo, _)| typo != added),
                "{ctx}: added word is the typo: {q:?}"
            );
            let mut all = q.intended.clone();
            all.push(added.clone());
            assert_eq!(
                naive_matching(corpus, &all),
                Vec::<String>::new(),
                "{ctx}: added word held beside every intended word: {q:?}"
            );
            assert_eq!(
                typed.iter().filter(|w| *w == added).count(),
                1,
                "{ctx}: added word not typed once: {q:?}"
            );
            let at = typed.iter().position(|w| w == added).unwrap();
            want.insert(at, added.clone());
        }
        (None, QueryKind::Typo) => {}
        other => panic!("{ctx}: added {other:?} for {q:?}"),
    }
    assert_eq!(q.text, want.join(" "), "{ctx}: {q:?}");
}

/// `generate(suite, n)` over `corpus`, every query checked and rebuilt from its seed; the queries.
fn generate_checked(
    ctx: &str,
    corpus: &Corpus,
    options: QueryOptions,
    suite: u64,
    n: usize,
) -> Vec<SyntheticQuery> {
    let generator = QueryGenerator::new(corpus, options);
    let queries = generator.generate(suite, n);
    assert!(queries.len() <= n, "{ctx}: {} > {n} queries", queries.len());
    for q in &queries {
        check(ctx, corpus, q);
        assert_eq!(
            generator.query(q.seed).as_ref(),
            Some(q),
            "{ctx}: query seed {} does not rebuild it",
            q.seed
        );
    }
    queries
}

// ---------------------------------------------------------------------------------------------------------------
// The oracles

#[test]
fn the_osa_oracle_agrees_with_known_distances() {
    for (a, b, want) in [
        ("", "", 0),
        ("", "abc", 3),
        ("abc", "abc", 0),
        ("abc", "abd", 1),
        ("abc", "ab", 1),
        ("abc", "abxc", 1),
        ("abc", "acb", 1),
        ("ca", "ac", 1),
        // OSA, not Damerau-Levenshtein: a swapped pair is not edited again (DL gives 2).
        ("ca", "abc", 3),
        ("kitten", "sitting", 3),
    ] {
        assert_eq!(osa(a, b), want, "osa({a:?}, {b:?})");
        assert_eq!(osa(b, a), want, "osa({b:?}, {a:?})");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 8. Rng

#[test]
fn rng_matches_the_splitmix64_reference() {
    // Reference outputs of Vigna's splitmix64.c, so the stream (and every recorded suite) is the published one.
    let mut rng = Rng::new(0);
    let got: Vec<u64> = (0..3).map(|_| rng.next_u64()).collect();
    assert_eq!(
        got,
        [
            0xe220_a839_7b1d_cdaf,
            0x6e78_9e6a_a1b9_65f4,
            0x06c4_5d18_8009_454f
        ]
    );
    let mut rng = Rng::new(1_234_567);
    let got: Vec<u64> = (0..5).map(|_| rng.next_u64()).collect();
    assert_eq!(
        got,
        [
            6_457_827_717_110_365_317,
            3_203_168_211_198_807_973,
            9_817_491_932_198_370_423,
            4_593_380_528_125_082_431,
            16_408_922_859_458_223_821,
        ]
    );
    // A fork is the next draw: one stream's use does not shift another's.
    let (mut a, mut b) = (Rng::new(9), Rng::new(9));
    assert_eq!(a.fork(), b.next_u64());
}

#[test]
fn rng_edges_return_their_documented_values() {
    let mut rng = Rng::new(42);
    let fresh = rng.clone().next_u64();
    // below(0) is 0 and draws nothing (so a zero bound does not move the stream).
    assert_eq!(rng.below(0), 0);
    assert_eq!(rng.clone().next_u64(), fresh, "below(0) consumed a draw");
    for _ in 0..100 {
        assert_eq!(rng.below(1), 0);
    }
    for n in [2, 3, 7, 100, 1 << 40, usize::MAX - 1, usize::MAX] {
        for _ in 0..100 {
            assert!(rng.below(n) < n, "below({n})");
        }
    }
    // within: a one-value range gives it; an empty (inverted) range gives its start; never outside a range.
    assert_eq!(rng.within(&(5..=5)), 5);
    assert_eq!(rng.within(&(0..=0)), 0);
    assert_eq!(rng.within(&(usize::MAX..=usize::MAX)), usize::MAX);
    #[allow(clippy::reversed_empty_ranges)]
    let inverted = 9..=3;
    assert_eq!(rng.within(&inverted), 9);
    for range in [0..=1, 3..=7, 0..=usize::MAX, usize::MAX - 2..=usize::MAX] {
        for _ in 0..100 {
            let v = rng.within(&range);
            assert!(range.contains(&v), "within({range:?}) gave {v}");
        }
    }
    let mut seen = BTreeSet::new();
    for _ in 0..200 {
        seen.insert(rng.within(&(3..=5)));
    }
    assert_eq!(
        seen,
        BTreeSet::from([3, 4, 5]),
        "within misses an end of its range"
    );
    // pick: None on an empty slice, the element on one of one.
    assert_eq!(rng.pick::<u8>(&[]), None);
    assert_eq!(rng.pick(&[7]), Some(&7));
    // chance: 0 never, 100 and above always.
    assert!((0..200).all(|_| !rng.chance(0)));
    assert!((0..200).all(|_| rng.chance(100)));
    assert!((0..200).all(|_| rng.chance(usize::MAX)));
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Determinism

#[test]
fn every_query_holds_its_ground_truth_on_every_seed() {
    for seed in 0..8u64 {
        let corpus = Corpus::synthetic(seed, &CorpusOptions::default());
        let suite = seed ^ 0xabcd;
        let ctx = format!("corpus seed {seed}, suite seed {suite}");
        let queries = generate_checked(&ctx, &corpus, QueryOptions::default(), suite, 200);
        assert_eq!(queries.len(), 200, "{ctx}");
        let kinds: BTreeSet<QueryKind> = queries.iter().map(|q| q.kind).collect();
        assert_eq!(kinds.len(), 3, "{ctx}");
    }
}

#[test]
fn a_seed_names_its_corpus_and_queries() {
    let options = CorpusOptions::default();
    let a = Corpus::synthetic(7, &options);
    let b = Corpus::synthetic(7, &options);
    assert_eq!(a.docs(), b.docs());
    // Every seed of a range gives its own corpus and suite.
    let corpora: BTreeSet<Vec<String>> = (0..16u64)
        .map(|s| {
            Corpus::synthetic(s, &options)
                .docs()
                .iter()
                .map(CorpusDoc::text)
                .collect()
        })
        .collect();
    assert_eq!(
        corpora.len(),
        16,
        "two corpus seeds of 0..16 give one corpus"
    );
    let generator = QueryGenerator::new(&a, QueryOptions::default());
    let queries = generator.generate(1, 50);
    assert_eq!(queries, generator.generate(1, 50));
    let suites: BTreeSet<Vec<String>> = (0..16u64)
        .map(|s| {
            generator
                .generate(s, 20)
                .into_iter()
                .map(|q| q.text)
                .collect()
        })
        .collect();
    assert_eq!(suites.len(), 16, "two suite seeds of 0..16 give one suite");
    let seeds: BTreeSet<u64> = queries.iter().map(|q| q.seed).collect();
    assert_eq!(seeds.len(), queries.len(), "two queries share a seed");
    for q in &queries {
        assert_eq!(
            generator.query(q.seed).as_ref(),
            Some(q),
            "query seed {}",
            q.seed
        );
    }
}

#[test]
fn a_shorter_suite_is_a_prefix_of_a_longer_one() {
    let corpus = Corpus::synthetic(11, &CorpusOptions::default());
    let generator = QueryGenerator::new(&corpus, QueryOptions::default());
    for suite in 0..4u64 {
        let long = generator.generate(suite, 60);
        for n in [0, 1, 7, 59] {
            assert_eq!(
                generator.generate(suite, n),
                long[..n],
                "suite seed {suite}, n {n}"
            );
        }
    }
}

#[test]
fn a_pinned_seed_gives_a_pinned_corpus_and_query() {
    // A change to the generator moves every recorded suite: it must be seen here (and the baselines re-recorded).
    let corpus = Corpus::synthetic(7, &CorpusOptions::default());
    assert_eq!(
        corpus.docs()[0].text(),
        "nobi mugenidi vine guhebu gune culo fidisi romukoviro jufi kafipi vovafa casisu dohehu"
    );
    let generator = QueryGenerator::new(&corpus, QueryOptions::default());
    let q = &generator.generate(1, 1)[0];
    assert_eq!(
        (
            q.kind,
            q.text.as_str(),
            q.target.as_str(),
            q.intended.join(" ")
        ),
        (
            QueryKind::Drop,
            "disafoj bisobuz vuzufil",
            "d46",
            "disafoj vuzufil".to_owned()
        )
    );
}

// ---------------------------------------------------------------------------------------------------------------
// The corpus

#[test]
fn the_corpus_holds_what_its_options_ask() {
    let options = CorpusOptions {
        docs: 50,
        vocabulary: 100,
        words: 3..=6,
    };
    let corpus = Corpus::synthetic(3, &options);
    assert_eq!(corpus.docs().len(), 50);
    let mut vocabulary = BTreeSet::new();
    for (i, doc) in corpus.docs().iter().enumerate() {
        assert_eq!(doc.id, format!("d{i}"));
        assert!((3..=6).contains(&doc.words.len()));
        for w in &doc.words {
            assert!((3..=10).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_lowercase()));
            vocabulary.insert(w.clone());
            assert!(corpus.holds(w));
            assert!(corpus.matching(&[w]).contains(&doc.id));
        }
        assert_eq!(doc.text(), doc.words.join(" "));
    }
    assert!(vocabulary.len() <= 100);
    // No word: every document.
    assert_eq!(corpus.matching::<&str>(&[]).len(), 50);
}

#[test]
fn degenerate_corpus_options_give_no_query_and_no_panic() {
    for (docs, vocabulary, words) in [(0, 10, 2..=3), (5, 0, 2..=3), (5, 10, 0..=0), (1, 1, 1..=1)]
    {
        let ctx = format!("docs {docs}, vocabulary {vocabulary}, words {words:?}");
        let corpus = Corpus::synthetic(
            1,
            &CorpusOptions {
                docs,
                vocabulary,
                words,
            },
        );
        assert_eq!(corpus.docs().len(), docs, "{ctx}");
        let queries = generate_checked(&ctx, &corpus, QueryOptions::default(), 1, 4);
        assert_eq!(queries, [], "{ctx}");
    }
}

#[test]
fn texts_split_into_lowercase_runs_of_letters_and_digits() {
    let corpus = Corpus::from_texts([("a", "Red-Shoes, size 42!"), ("b", "  red  hat ")]).unwrap();
    assert_eq!(corpus.docs()[0].words, ["red", "shoes", "size", "42"]);
    assert_eq!(corpus.docs()[1].words, ["red", "hat"]);
    assert_eq!(corpus.matching(&["red"]), ["a", "b"]);
    assert_eq!(corpus.matching(&["red", "hat"]), ["b"]);
    assert_eq!(corpus.matching(&["blue"]).len(), 0);
    // Non-ASCII letters split a word; nothing but ASCII letters and digits is kept.
    let corpus = Corpus::from_texts([("c", "Café naïve ÉTÉ 日本 x_y"), ("d", "!?.,")]).unwrap();
    assert_eq!(corpus.docs()[0].words, ["caf", "na", "ve", "t", "x", "y"]);
    assert_eq!(corpus.docs()[1].words, Vec::<String>::new());
    // A repeated word in a query does not change what matches; an unknown one empties it.
    assert_eq!(corpus.matching(&["x", "x"]), ["c"]);
    assert_eq!(corpus.matching(&["x", "nope"]), Vec::<String>::new());
}

#[test]
fn duplicate_ids_are_rejected() {
    // Two documents of one id would make the ground truth ambiguous (and an engine keeps only the last).
    let err =
        Corpus::from_texts([("a", "red hat"), ("b", "blue hat"), ("a", "green hat")]).unwrap_err();
    assert_eq!(err, DuplicateId("a".to_owned()));
    assert!(err.to_string().contains("\"a\""), "{err}");
    // Ids differing only by case are distinct ids.
    let corpus = Corpus::from_texts([("a", "red"), ("A", "red")]).unwrap();
    assert_eq!(corpus.matching(&["red"]), ["a", "A"]);
}

#[test]
fn the_made_up_word_gives_up_after_bounded_draws() {
    // A corpus that held every word would make the added word's fallback loop forever: it stops after `TRIES` draws.
    let mut rng = Rng::new(7);
    assert_eq!(made_up(&mut rng, |_| true), None);
    // One held draw is skipped, the next kept.
    let mut rng = Rng::new(7);
    let first = pronounceable(&mut rng.clone(), 5..=8);
    let word = made_up(&mut rng, |w| w == first).unwrap();
    assert_ne!(word, first);
    assert!((5..=8).contains(&word.len()), "{word}");
}

#[test]
fn the_added_word_and_the_typo_go_anywhere_in_the_text() {
    // A relaxation that only ever drops the last word, or only corrects the first, must not pass the suite: the added
    // word lands at every position of a 2-word query (3 as typed), and when both words can be misspelled, so does the
    // typo.
    let corpus = Corpus::synthetic(9, &CorpusOptions::default());
    let options = QueryOptions {
        words: 2..=2,
        weights: [1, 1, 0],
    };
    let ctx = "corpus seed 9, suite seed 6, words 2..=2";
    let queries = generate_checked(ctx, &corpus, options, 6, 200);
    let mut added_at = BTreeSet::new();
    let mut typo_at = BTreeSet::new();
    for q in &queries {
        let typed: Vec<&str> = q.text.split(' ').collect();
        if let Some(added) = &q.added {
            added_at.insert(typed.iter().position(|w| w == added).unwrap());
        }
        let both_long = q.intended.iter().all(|w| w.len() > MIN_TYPO_LEN);
        if let (Some((typo, _)), true) = (&q.typo, both_long) {
            typo_at.insert(typed.iter().position(|w| w == typo).unwrap());
        }
    }
    assert_eq!(
        added_at,
        BTreeSet::from([0, 1, 2]),
        "{ctx}: added word positions"
    );
    assert_eq!(typo_at, BTreeSet::from([0, 1]), "{ctx}: typo positions");
}

// ---------------------------------------------------------------------------------------------------------------
// 6. Kinds and options

#[test]
fn kind_weights_select_only_their_kind() {
    let corpus = Corpus::synthetic(5, &CorpusOptions::default());
    for (weights, want) in [
        ([1, 0, 0], QueryKind::Typo),
        ([0, 1, 0], QueryKind::Drop),
        ([0, 0, 1], QueryKind::TypoAndDrop),
        ([0, 7, 0], QueryKind::Drop),
    ] {
        let ctx = format!("corpus seed 5, suite seed 3, weights {weights:?}");
        let options = QueryOptions {
            weights,
            ..QueryOptions::default()
        };
        let queries = generate_checked(&ctx, &corpus, options, 3, 100);
        assert_eq!(queries.len(), 100, "{ctx}");
        assert!(queries.iter().all(|q| q.kind == want), "{ctx}");
    }
}

#[test]
fn a_zero_total_weight_gives_typo_and_drop() {
    // Pinned: `below(0)` is 0, which no zero weight takes, so the last kind is drawn.
    let corpus = Corpus::synthetic(5, &CorpusOptions::default());
    let ctx = "corpus seed 5, suite seed 3, weights [0, 0, 0]";
    let options = QueryOptions {
        weights: [0, 0, 0],
        ..QueryOptions::default()
    };
    let queries = generate_checked(ctx, &corpus, options, 3, 50);
    assert_eq!(queries.len(), 50, "{ctx}");
    assert!(
        queries.iter().all(|q| q.kind == QueryKind::TypoAndDrop),
        "{ctx}"
    );
}

#[test]
fn saturating_weights_do_not_panic() {
    let corpus = Corpus::synthetic(5, &CorpusOptions::default());
    for weights in [
        [usize::MAX; 3],
        [usize::MAX, 0, 0],
        [0, usize::MAX, 1],
        [1, 1, usize::MAX],
    ] {
        let ctx = format!("corpus seed 5, suite seed 3, weights {weights:?}");
        let options = QueryOptions {
            weights,
            ..QueryOptions::default()
        };
        let queries = generate_checked(&ctx, &corpus, options, 3, 30);
        assert_eq!(queries.len(), 30, "{ctx}");
    }
}

#[test]
fn default_weights_mix_kinds_in_proportion() {
    // Weights [2, 1, 1]: about half typos, a quarter of each other kind (loose bounds; the seed is fixed).
    let corpus = Corpus::synthetic(2, &CorpusOptions::default());
    let queries = QueryGenerator::new(&corpus, QueryOptions::default()).generate(4, 2000);
    assert_eq!(queries.len(), 2000);
    for (kind, lo, hi) in [
        (QueryKind::Typo, 850, 1150),
        (QueryKind::Drop, 380, 620),
        (QueryKind::TypoAndDrop, 380, 620),
    ] {
        let n = queries.iter().filter(|q| q.kind == kind).count();
        assert!(
            (lo..=hi).contains(&n),
            "corpus seed 2, suite seed 4: {n} {kind} of 2000"
        );
    }
}

#[test]
fn query_word_counts_follow_the_options() {
    let corpus = Corpus::synthetic(6, &CorpusOptions::default());
    #[allow(clippy::reversed_empty_ranges)]
    let inverted = 3..=2;
    // (words, the counts allowed): an inverted range draws its start, as `Rng::within` does.
    for (words, allowed) in [
        (1..=1, 1..=1),
        (4..=4, 4..=4),
        (1..=5, 1..=5),
        (inverted, 3..=3),
    ] {
        let ctx = format!("corpus seed 6, suite seed 8, words {words:?}");
        let options = QueryOptions {
            words,
            ..QueryOptions::default()
        };
        let queries = generate_checked(&ctx, &corpus, options, 8, 60);
        assert_eq!(queries.len(), 60, "{ctx}");
        for q in &queries {
            assert!(allowed.contains(&q.intended.len()), "{ctx}: {q:?}");
        }
    }
    // No intended word: no query, and the generator gives up.
    let options = QueryOptions {
        words: 0..=0,
        ..QueryOptions::default()
    };
    assert_eq!(QueryGenerator::new(&corpus, options).generate(8, 5), []);
}

// ---------------------------------------------------------------------------------------------------------------
// 4. and 5. Typos and added words on small corpora

#[test]
fn a_corpus_that_cannot_give_a_query_gives_fewer() {
    // One word per document: no query of 2 words.
    let corpus = Corpus::from_texts([("a", "alpha"), ("b", "beta")]).unwrap();
    let generator = QueryGenerator::new(&corpus, QueryOptions::default());
    assert_eq!(generator.generate(1, 5).len(), 0);
    // Short words only: drops, never typos.
    let corpus = Corpus::from_texts([("a", "ab cd ef"), ("b", "cd ef gh")]).unwrap();
    let queries = generate_checked(
        "short words, suite seed 1",
        &corpus,
        QueryOptions::default(),
        1,
        20,
    );
    assert_ne!(queries.len(), 0);
    assert!(queries.iter().all(|q| q.kind == QueryKind::Drop));
    // Typos only over words of at most MIN_TYPO_LEN letters: none can be built.
    let corpus = Corpus::from_texts([("a", "abcd efgh ijkl"), ("b", "abcd mnop")]).unwrap();
    let options = QueryOptions {
        weights: [1, 0, 0],
        ..QueryOptions::default()
    };
    assert_eq!(QueryGenerator::new(&corpus, options).generate(1, 5), []);
    let empty = Corpus::from_texts::<_, &str, &str>([]).unwrap();
    assert_eq!(
        QueryGenerator::new(&empty, QueryOptions::default()).generate(1, 3),
        []
    );
}

#[test]
fn a_typo_never_replaces_a_short_word() {
    // Only `bridge` has more than MIN_TYPO_LEN letters: every typo is of it.
    let corpus = Corpus::from_texts([
        ("a", "blue bridge over lake"),
        ("b", "red bridge"),
        ("c", "over the lake"),
    ])
    .unwrap();
    let options = QueryOptions {
        weights: [1, 0, 1],
        words: 2..=4,
    };
    let queries = generate_checked("bridge corpus, suite seed 2", &corpus, options, 2, 40);
    assert_eq!(queries.len(), 40);
    for q in &queries {
        assert_eq!(q.typo.as_ref().unwrap().1, "bridge", "suite seed 2: {q:?}");
    }
}

#[test]
fn one_document_needs_a_made_up_added_word() {
    // Every word of the corpus is in its one document, so no corpus word can be added: the added word is held by none.
    let corpus = Corpus::from_texts([("only", "seven silver swans swim south")]).unwrap();
    let options = QueryOptions {
        weights: [0, 1, 1],
        words: 1..=3,
    };
    let queries = generate_checked("one document, suite seed 3", &corpus, options, 3, 30);
    assert_eq!(queries.len(), 30);
    for q in &queries {
        let added = q.added.as_ref().unwrap();
        assert!(!corpus.holds(added), "suite seed 3: {q:?}");
        assert!(
            (5..=8).contains(&added.len()) && added.bytes().all(|b| b.is_ascii_lowercase()),
            "suite seed 3: {q:?}"
        );
    }
}

#[test]
fn odd_texts_never_panic_and_hold_the_invariants() {
    let corpora: &[(&str, &[&str])] = &[
        ("empty texts", &["", "", "   "]),
        ("punctuation only", &["!!! ... ,,,", "-- // ??"]),
        (
            "digits",
            &["12345 67890 24680", "12345 13579", "000000 11111"],
        ),
        (
            "mixed digits",
            &[
                "abc123 x9y8z7 route66 plain",
                "route66 abc123",
                "plain words here",
            ],
        ),
        (
            "non-ASCII",
            &[
                "Café crème brûlée naïve",
                "日本語 テキスト ok",
                "straße größe maße",
            ],
        ),
        ("single-word docs", &["alpha", "bravo", "charlie", "delta"]),
        (
            "repeated words",
            &[
                "echo echo echo foxtrot",
                "echo foxtrot foxtrot",
                "golf golf",
            ],
        ),
        ("one repeated word", &["hotel hotel hotel hotel"]),
        (
            "exact lengths",
            &["abcd abcde abcdef", "abcd abcde", "abcdef"],
        ),
        ("upper and lower", &["Kilo KILO kilo Lima", "LIMA mike"]),
        (
            "identical docs",
            &["november oscar papa", "november oscar papa"],
        ),
    ];
    let option_sets = [
        QueryOptions::default(),
        QueryOptions {
            words: 1..=1,
            weights: [1, 1, 1],
        },
        QueryOptions {
            words: 1..=4,
            weights: [0, 0, 1],
        },
        QueryOptions {
            words: 2..=2,
            weights: [1, 0, 0],
        },
    ];
    for (name, texts) in corpora {
        let corpus =
            Corpus::from_texts(texts.iter().enumerate().map(|(i, t)| (format!("t{i}"), *t)))
                .unwrap();
        for (o, options) in option_sets.iter().enumerate() {
            for suite in 0..3u64 {
                let ctx = format!("{name}, options #{o}, suite seed {suite}");
                generate_checked(&ctx, &corpus, options.clone(), suite, 6);
            }
        }
    }
}

#[test]
fn an_edit_is_one_osa_step() {
    let mut rng = Rng::new(5);
    for word in ["kalimo", "tesa", "barunemi", "aa", "a", ""] {
        let mut kinds = BTreeSet::new();
        for _ in 0..400 {
            let e = edit(&mut rng, word);
            assert!(osa(word, &e) <= 1, "seed 5: {word} → {e}");
            kinds.insert(e.len().cmp(&word.len()));
        }
        // Insertions, deletions and same-length edits are all drawn (the empty word: no deletion, no substitution).
        let want = if word.is_empty() { 2 } else { 3 };
        assert_eq!(kinds.len(), want, "seed 5: {word} edits {kinds:?}");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Properties

#[cfg(not(feature = "slow"))]
const SCALE: u32 = 1;
#[cfg(feature = "slow")]
const SCALE: u32 = 64;

const PROPTEST_SEED: u64 = 0x5045_5253_4941_0841; // "PERSIA", 8.4.1

/// `cases` (times [`SCALE`]) from a fixed seed, unless `PROPTEST_CASES` / `PROPTEST_RNG_SEED` say otherwise.
fn config(cases: u32) -> ProptestConfig {
    let env = ProptestConfig::default();
    ProptestConfig {
        cases: if std::env::var_os("PROPTEST_CASES").is_some() {
            env.cases
        } else {
            cases * SCALE
        },
        rng_seed: if std::env::var_os("PROPTEST_RNG_SEED").is_some() {
            env.rng_seed
        } else {
            RngSeed::Fixed(PROPTEST_SEED)
        },
        ..env
    }
}

/// Small corpus options, including degenerate ones (no documents, no vocabulary, inverted word ranges).
fn corpus_options() -> impl Strategy<Value = CorpusOptions> {
    (0usize..40, 0usize..60, 0usize..12, 0usize..12).prop_map(|(docs, vocabulary, lo, hi)| {
        CorpusOptions {
            docs,
            vocabulary,
            words: lo..=hi,
        }
    })
}

/// Query options, including empty and inverted word ranges and zero weights.
fn query_options() -> impl Strategy<Value = QueryOptions> {
    (0usize..5, 0usize..5, prop::array::uniform3(0usize..4)).prop_map(|(lo, hi, weights)| {
        QueryOptions {
            words: lo..=hi,
            weights,
        }
    })
}

/// A text of words a test cares about (and some it does not): mixed case, digits, non-ASCII, punctuation, empty.
fn odd_text() -> impl Strategy<Value = String> {
    let token = prop::sample::select(vec![
        "alpha",
        "Alpha",
        "BRAVO",
        "charlie",
        "delta",
        "echo",
        "kappa",
        "omega",
        "x",
        "yy",
        "abcd",
        "abcde",
        "abcdef",
        "12345",
        "a1b2c3",
        "naïve",
        "café",
        "Ünïcödé",
        "日本語",
        "straße",
        "--",
        "!?",
        ".",
        "",
        "ab_cd",
        "e\u{301}clair",
    ]);
    let sep = prop::sample::select(vec![" ", ",", "\t", "-", "é", "\n", "/", "  "]);
    prop::collection::vec((token, sep), 0..10)
        .prop_map(|parts| parts.into_iter().flat_map(|(t, s)| [t, s]).collect())
}

/// Odd texts and their corpus (ids `t0`, `t1`...).
fn odd_corpus() -> impl Strategy<Value = (Vec<String>, Corpus)> {
    prop::collection::vec(odd_text(), 0..8).prop_map(|texts| {
        let corpus = Corpus::from_texts(
            texts
                .iter()
                .enumerate()
                .map(|(i, t)| (format!("t{i}"), t.as_str())),
        )
        .unwrap();
        (texts, corpus)
    })
}

proptest! {
    #![proptest_config(config(48))]

    /// Invariants 1 to 5 on synthetic corpora of any small shape and any query options.
    #[test]
    fn synthetic_queries_hold_the_invariants(
        corpus_seed in any::<u64>(),
        suite in any::<u64>(),
        corpus_options in corpus_options(),
        options in query_options(),
    ) {
        let corpus = Corpus::synthetic(corpus_seed, &corpus_options);
        let again = Corpus::synthetic(corpus_seed, &corpus_options);
        prop_assert_eq!(corpus.docs(), again.docs());
        let ctx = format!("corpus seed {corpus_seed}, suite seed {suite}, {corpus_options:?}, {options:?}");
        let queries = generate_checked(&ctx, &corpus, options.clone(), suite, 4);
        prop_assert_eq!(&queries, &QueryGenerator::new(&corpus, options).generate(suite, 4), "{}", ctx);
    }

    /// Invariant 7: corpora of odd texts never panic, the generator returns, and its queries hold 2 to 5.
    #[test]
    fn odd_text_queries_hold_the_invariants(
        (texts, corpus) in odd_corpus(),
        suite in any::<u64>(),
        options in query_options(),
    ) {
        for doc in corpus.docs() {
            prop_assert!(
                doc.words.iter().all(|w| !w.is_empty()
                    && w.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())),
                "{:?} from {:?}", doc, texts
            );
        }
        let ctx = format!("texts {texts:?}, suite seed {suite}, {options:?}");
        generate_checked(&ctx, &corpus, options, suite, 4);
    }
}

proptest! {
    #![proptest_config(config(256))]

    /// The ground truth's `matching` is a scan of the documents, whatever the words (repeated, unknown, none).
    #[test]
    fn matching_is_a_scan_of_the_documents(
        (texts, corpus) in odd_corpus(),
        words in prop::collection::vec(
            prop::sample::select(vec!["alpha", "bravo", "charlie", "abcd", "x", "12345", "caf", "nope", ""]),
            0..4,
        ),
    ) {
        prop_assert_eq!(corpus.matching(&words), naive_matching(&corpus, &words), "texts {:?}", texts);
        for w in &words {
            prop_assert_eq!(corpus.holds(w), naive_holds(&corpus, w), "texts {:?}", texts);
        }
    }

    /// An edit is one OSA step, or none (a substitution by the same letter, a swap of equal letters); its length moves
    /// by at most one and it stays lowercase ASCII.
    #[test]
    fn an_edit_is_one_osa_step_or_none(seed in any::<u64>(), word in "[a-z]{0,12}") {
        let mut rng = Rng::new(seed);
        let e = edit(&mut rng, &word);
        let d = osa(&word, &e);
        prop_assert!(d == usize::from(e != word), "seed {}: {:?} -> {:?} at {}", seed, word, e, d);
        prop_assert!(e.len().abs_diff(word.len()) <= 1, "seed {}: {:?} -> {:?}", seed, word, e);
        prop_assert!(e.bytes().all(|b| b.is_ascii_lowercase()), "seed {}: {:?} -> {:?}", seed, word, e);
    }

    /// `query(seed)` is a function of the seed: it builds the same query (or none) twice, carrying the seed.
    #[test]
    fn a_query_seed_rebuilds_its_query(seed in any::<u64>()) {
        let corpus = Corpus::synthetic(1, &CorpusOptions { docs: 30, vocabulary: 60, words: 2..=8 });
        let generator = QueryGenerator::new(&corpus, QueryOptions::default());
        let q = generator.query(seed);
        prop_assert_eq!(&q, &generator.query(seed), "seed {}", seed);
        if let Some(q) = &q {
            prop_assert_eq!(q.seed, seed);
            check(&format!("corpus seed 1, query seed {seed}"), &corpus, q);
        }
    }
}

/// Many more seeds over the default corpus and suites (nightly).
#[cfg(feature = "slow")]
#[test]
fn every_query_holds_its_ground_truth_on_many_seeds() {
    for seed in 0..256u64 {
        let corpus = Corpus::synthetic(seed, &CorpusOptions::default());
        let suite = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let ctx = format!("corpus seed {seed}, suite seed {suite}");
        let queries = generate_checked(&ctx, &corpus, QueryOptions::default(), suite, 500);
        assert_eq!(queries.len(), 500, "{ctx}");
    }
}
