//! L3 and L4 through a database over a field whose analyzer stacks tokens (ROADMAP 8.1.23; SPEC §6.3.1, §6.3.4,
//! §7.6): the schema names a registered analyzer with an `edge_ngram` filter (ROADMAP 6.6.8). Its index already holds
//! each word's prefixes, so neither level rewrites its terms: L3 leaves them exact and L4 neither fuzzes nor corrects
//! them, while the other default field is rewritten as usual.
//!
//! Invariants, over random corpora and words:
//! 1. L3's and L4's hits are exactly what a reference finds that rewrites only the fields whose analyzer does not stack
//!    tokens: a document matches L3 when such a field holds a term the word starts (the last word is always
//!    expanded), or any field holds the word exactly; L4 likewise with a term within the word's distance (SPEC §7.6:
//!    none below 4 characters, 1 edit from 4, 2 from 8, as the optimal string alignment distance over characters).
//! 2. With the same field under an analyzer that does not stack (and analyzes queries alike), the levels do rewrite it,
//!    and the reference that rewrites every field matches: the check above sees the difference.
//! 3. With the stacking field as the only default field, neither level builds a step.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, missing_docs)] // Tests: inputs are generated.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use persia_analysis::{
    Analyzer, AnalyzerRegistry, EdgeNgramFilter, LowercaseFilter, StandardTokenizer,
};
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, RelaxedResults, SearchRequest, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{CollectionSchema, Fallback, Level, Levels, RelaxationConfig};
use persia_engine::search::{Projection, Pruning};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;

/// `grams@1` stacks the 2- to 4-character prefixes of each lowercased word, and the word itself when longer;
/// `plain@1` is the same chain without the stacking filter, so both analyze a query alike (SPEC §6.3.4).
fn registry() -> AnalyzerRegistry {
    let n = |n| NonZeroUsize::new(n).unwrap();
    let mut registry = AnalyzerRegistry::new();
    let grams = Analyzer::builder(StandardTokenizer)
        .filter(LowercaseFilter)
        .filter(
            EdgeNgramFilter::new(n(2), n(4))
                .unwrap()
                .preserve_original(true),
        )
        .build();
    registry.register("grams", 1, grams).unwrap();
    let plain = Analyzer::builder(StandardTokenizer)
        .filter(LowercaseFilter)
        .build();
    registry.register("plain", 1, plain).unwrap();
    registry.with_builtins()
}

/// `title` by `standard`, `extra` by `analyzer`, both default fields.
fn schema(analyzer: &str) -> CollectionSchema {
    let json = format!(
        r#"{{"name": "c", "fields": {{"title": {{"type": "text"}}, "extra": {{"type": "text", "analyzer": "{analyzer}"}}}}}}"#
    );
    CollectionSchema::from_json_in(&json, &registry()).unwrap()
}

/// `docs` as (title, extra) words; a commit after the first half, so the levels expand over a segment and the
/// memtable.
fn database(analyzer: &str, docs: &[(Vec<String>, Vec<String>)]) -> Database<SimFile> {
    let schema = schema(analyzer);
    let title = schema.field("title").unwrap().id();
    let extra = schema.field("extra").unwrap().id();
    let options = DatabaseOptions {
        registry: registry(),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [8; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    for (i, (t, e)) in docs.iter().enumerate() {
        let mut doc = Document::new(format!("d{i}"));
        if !t.is_empty() {
            doc = doc.with(title, Value::String(t.join(" ")));
        }
        if !e.is_empty() {
            doc = doc.with(extra, Value::String(e.join(" ")));
        }
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i + 1 == docs.len() / 2 {
            db.commit().unwrap();
        }
    }
    db
}

/// L0, then `level` alone, without a fallback, with every hit wanted.
fn plan(level: Level) -> RelaxPlan {
    let options = RelaxOptions {
        min_results: Some(*RelaxationConfig::MIN_RESULTS.end()),
        levels: Some(Levels::Custom(vec![level])),
        fallback: Some(Fallback::None),
        ..RelaxOptions::default()
    };
    RelaxPlan::new(&RelaxationConfig::default(), &options).unwrap()
}

/// `text` searched under `level`, over `fields` (every text field when `None`).
fn search(
    db: &Database<SimFile>,
    text: &str,
    level: Level,
    fields: Option<&[&str]>,
) -> RelaxedResults {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let fields = match fields {
        None => DefaultFields::all(c.schema()),
        Some(names) => DefaultFields::parse(c.schema(), names).unwrap(),
    };
    let compiled = QueryCompiler::new(c.schema(), &registry())
        .unwrap()
        .compile(text, &fields, &QueryLimits::default());
    let request = SearchRequest {
        limit: 1000,
        projection: Projection::None,
        ..SearchRequest::default()
    };
    let exec = Execution::new(Pruning::default())
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)));
    views
        .search_relaxed(&compiled, &request, &plan(level), &exec)
        .unwrap()
}

fn ids(out: &RelaxedResults) -> BTreeSet<String> {
    out.hits
        .iter()
        .map(|h| h.hit.document.id().to_owned())
        .collect()
}

/// The optimal string alignment distance between `a` and `b`, over characters (SPEC §7.1).
fn osa(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
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

/// SPEC §7.6 L4's distance for a term of `len` characters; `None` below 4.
fn distance(len: usize) -> Option<usize> {
    match len {
        0..4 => None,
        4..8 => Some(1),
        _ => Some(2),
    }
}

/// The reference (invariant 1): the documents L0 misses that `level`'s one step finds for the one-word query
/// `word`, rewriting only `extra` when `rewrite_extra`. With one word and no other level, the step is the last term
/// expanded (L3) or fuzzed (L4) in every rewritable field, any field matching. `None` when the level builds no step:
/// no field is rewritable, or the word is too short (L3: 1 character, L4: under 4). A field's terms are the words its
/// documents hold; `extra` under `grams` also holds the stacked prefixes, which an exact match sees.
fn reference(
    docs: &[(Vec<String>, Vec<String>)],
    word: &str,
    level: Level,
    stacking: bool,
) -> Option<BTreeSet<String>> {
    let grams = |w: &str| -> Vec<String> {
        let chars: Vec<char> = w.chars().collect();
        let mut out: Vec<String> = (2..=4.min(chars.len()))
            .map(|n| chars[..n].iter().collect())
            .collect();
        if chars.len() > 4 || chars.len() < 2 {
            out.push(w.to_owned());
        }
        out
    };
    let terms = |field: usize, d: &(Vec<String>, Vec<String>)| -> Vec<String> {
        match field {
            0 => d.0.clone(),
            _ if stacking => d.1.iter().flat_map(|w| grams(w)).collect(),
            _ => d.1.clone(),
        }
    };
    let rewritable: Vec<usize> = if stacking { vec![0] } else { vec![0, 1] };
    let dist = distance(word.chars().count());
    if level == Level::Fuzzy && dist.is_none() {
        return None;
    }
    // L3 expands a last term of 2 characters or more (ROADMAP 8.1.20, ADR-0057): a single letter stays exact.
    if level == Level::Prefix && word.chars().count() < 2 {
        return None;
    }
    let exact =
        |d: &(Vec<String>, Vec<String>)| (0..2).any(|f| terms(f, d).iter().any(|t| t == word));
    let relaxed = |t: &str| match level {
        Level::Prefix => t.starts_with(word),
        _ => dist.is_some_and(|k| osa(t, word) <= k),
    };
    let found = docs
        .iter()
        .enumerate()
        .filter(|(_, d)| !exact(d))
        .filter(|(_, d)| {
            rewritable
                .iter()
                .any(|&f| terms(f, d).iter().any(|t| relaxed(t)))
        })
        .map(|(i, _)| format!("d{i}"))
        .collect();
    Some(found)
}

/// Checks `word` under `level` on both analyzers of `extra` against the reference: the hits past L0, the step built,
/// and (L4) that a correction is never a term only `extra` holds under `grams`.
fn check(
    docs: &[(Vec<String>, Vec<String>)],
    word: &str,
    level: Level,
) -> Result<bool, TestCaseError> {
    let mut found = Vec::new();
    for (analyzer, stacking) in [("grams", true), ("plain", false)] {
        let db = database(analyzer, docs);
        let out = search(&db, word, level, None);
        let past_l0: BTreeSet<String> = out
            .hits
            .iter()
            .filter(|h| h.step > 0)
            .map(|h| h.hit.document.id().to_owned())
            .collect();
        let expected = reference(docs, word, level, stacking);
        let at = format!("{analyzer}: {word:?} {level:?} over {docs:?}");
        prop_assert_eq!(&past_l0, &expected.clone().unwrap_or_default(), "{}", at);
        let built = !out.relaxation.applied.is_empty();
        prop_assert_eq!(
            built,
            expected.is_some(),
            "{}: {:?}",
            at,
            out.relaxation.applied
        );
        if stacking {
            let extra_only: BTreeSet<String> = docs
                .iter()
                .flat_map(|d| d.1.iter().cloned())
                .filter(|w| !docs.iter().any(|d| d.0.contains(w)))
                .collect();
            for c in &out.relaxation.corrected {
                prop_assert!(!extra_only.contains(&c.corrected), "{}: {:?}", at, c);
            }
        }
        found.push(past_l0);
    }
    Ok(found[0] != found[1])
}

/// By hand: `shoest` completes `shoestring` in `extra` and `shoestrap` in `title`; `shoestrinh` is one edit from
/// `shoestring`, more than two from `shoestrap`.
#[test]
fn l3_and_l4_leave_a_stacking_field_exact() {
    let w = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    let docs = vec![
        (w("walking boots"), w("shoestring")),
        (w("shoestrap"), w("boots")),
        (w("running"), w("red")),
    ];
    let set = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>();
    // Stacking: only `title` is expanded or fuzzed.
    let db = database("grams", &docs);
    let prefix = search(&db, "shoest", Level::Prefix, None);
    assert_eq!(prefix.relaxation.applied, ["prefix"]);
    assert_eq!(ids(&prefix), set(&["d1"]));
    let fuzzy = search(&db, "shoestrinh", Level::Fuzzy, None);
    // 10 characters: the `fuzzy:1` tier first (ROADMAP 8.2.7).
    assert_eq!(fuzzy.relaxation.applied, ["fuzzy:1", "fuzzy"]);
    assert_eq!(ids(&fuzzy), set(&[]));
    assert!(
        fuzzy.relaxation.corrected.is_empty(),
        "{:?}",
        fuzzy.relaxation.corrected
    );
    // The stacking field alone: no field to rewrite, no step.
    for (word, level) in [("shoest", Level::Prefix), ("shoestrinh", Level::Fuzzy)] {
        let out = search(&db, word, level, Some(&["extra"]));
        assert!(
            out.relaxation.applied.is_empty(),
            "{word}: {:?}",
            out.relaxation.applied
        );
        assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    }
    // An exact gram still matches at L0: `shoe` is a stacked prefix of `shoestring`.
    let l0 = search(&db, "shoe", Level::Prefix, Some(&["extra"]));
    assert!(l0.hits.iter().all(|h| h.step == 0));
    assert_eq!(ids(&l0), set(&["d0"]));
    // Not stacking: `extra` is rewritten too.
    let db = database("plain", &docs);
    assert_eq!(
        ids(&search(&db, "shoest", Level::Prefix, None)),
        set(&["d0", "d1"])
    );
    let fuzzy = search(&db, "shoestrinh", Level::Fuzzy, None);
    assert_eq!(ids(&fuzzy), set(&["d0"]));
    let corrected: Vec<_> = fuzzy
        .relaxation
        .corrected
        .iter()
        .map(|c| (c.original.as_str(), c.corrected.as_str()))
        .collect();
    assert_eq!(corrected, [("shoestrinh", "shoestring")]);
}

const WORDS: [&str; 8] = [
    "shoe",
    "shoes",
    "shoestring",
    "shop",
    "short",
    "boots",
    "booth",
    "bot",
];

fn words() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop::sample::select(&WORDS[..]).prop_map(str::to_owned),
        0..3,
    )
}

/// A query word: a document's word, cut short at times (L3), or with one or two edits (L4).
fn query_word() -> impl Strategy<Value = String> {
    let word = prop::sample::select(&WORDS[..]).prop_map(str::to_owned);
    prop_oneof![
        1 => word.clone(),
        2 => (word.clone(), 1..6_usize).prop_map(|(w, n)| w.chars().take(n).collect()),
        2 => (word, 0..10_usize, prop::sample::select(&['x', 'o', 's'][..])).prop_map(|(w, at, c)| {
            let mut chars: Vec<char> = w.chars().collect();
            let at = at % chars.len();
            chars[at] = c;
            chars.into_iter().collect()
        }),
    ]
}

/// Invariants 1 and 2 over random corpora and words. The levels must rewrite `extra` (under `plain`) differently
/// from leaving it exact (under `grams`) in enough cases, of both levels, for the check to mean something.
#[test]
fn l3_and_l4_rewrite_only_fields_that_do_not_stack() {
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

    let config = Config {
        cases: 64,
        ..Config::default()
    };
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &[0x23; 32]);
    let mut runner = TestRunner::new_with_rng(config, rng);
    let differ = std::cell::Cell::new([0usize; 2]);
    let strategy = (
        prop::collection::vec((words(), words()), 1..6),
        query_word(),
        any::<bool>(),
    );
    runner
        .run(&strategy, |(docs, word, fuzzy)| {
            let level = if fuzzy { Level::Fuzzy } else { Level::Prefix };
            if check(&docs, &word, level)? {
                let mut d = differ.get();
                d[usize::from(fuzzy)] += 1;
                differ.set(d);
            }
            Ok(())
        })
        .unwrap();
    let [prefix, fuzzy] = differ.get();
    assert!(
        prefix >= 3 && fuzzy >= 3,
        "cases where stacking changed the hits: L3 {prefix}, L4 {fuzzy}"
    );
}
