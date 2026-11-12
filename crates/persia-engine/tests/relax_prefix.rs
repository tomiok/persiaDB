//! L3 `prefix` through a database (SPEC §7.6, ROADMAP 8.2.3): a compiled query's words matching the terms they start,
//! planned over the collection's terms (an FST range per segment, SPEC §7.1).
//!
//! Invariants:
//! 1. L3's step matches a document when, for every required word, the document holds a term that starts with it (the
//!    last word, and every word of at least 3 characters) or equals it (any other word), beside L0's exclusions and
//!    filters; a document L0 matches is L0's (checked against an oracle over random corpora).
//! 2. A prefix keeps at most 50 terms, those with the highest collection `df` (SPEC §7.1); over `QueryLimits`, the
//!    step's prefixes are cut (`prefix:cut`) or the step is skipped (`prefix:skipped`, SPEC §7.1.3).
//! 3. The step states L0's text as `effective`.
//!
//! A field whose analyzer stacks tokens is not expanded (SPEC §6.3.1): schemas name built-in analyzers only, none of
//! which stacks, so `db::relaxed`'s unit tests give a collection's memtable such an analyzer instead.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, missing_docs)] // Tests: inputs are fixed.

use std::collections::BTreeSet;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, RelaxedResults, SearchRequest, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    Fallback, FieldKind, Level, Levels, RelaxationConfig, SchemaBuilder, TextOptions,
};
use persia_engine::search::{Projection, Pruning};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;

/// One `body` field (`standard`) with `docs`; a commit after the first half, so the prefixes expand over a segment and
/// the memtable.
fn database(docs: &[&str]) -> Database<SimFile> {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [5; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    for (i, text) in docs.iter().enumerate() {
        let doc = Document::new(format!("d{i}")).with(body, Value::String((*text).into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i + 1 == docs.len() / 2 {
            db.commit().unwrap();
        }
    }
    db
}

/// L0, then L3 alone: no other level runs first.
fn plan() -> RelaxPlan {
    let options = RelaxOptions {
        min_results: Some(*RelaxationConfig::MIN_RESULTS.end()),
        levels: Some(Levels::Custom(vec![Level::Prefix])),
        fallback: Some(Fallback::None),
        ..RelaxOptions::default()
    };
    RelaxPlan::new(&RelaxationConfig::default(), &options).unwrap()
}

fn exec() -> Execution {
    Execution::new(Pruning::default())
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

fn search(
    db: &Database<SimFile>,
    registry: &AnalyzerRegistry,
    text: &str,
    limits: Option<QueryLimits>,
) -> RelaxedResults {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let compiled = QueryCompiler::new(c.schema(), registry).unwrap().compile(
        text,
        &DefaultFields::all(c.schema()),
        &QueryLimits::default(),
    );
    let relax = limits.map_or_else(plan, |l| plan().with_limits(l));
    let request = SearchRequest {
        limit: 1000,
        projection: Projection::None,
        ..SearchRequest::default()
    };
    views
        .search_relaxed(&compiled, &request, &relax, &exec())
        .unwrap()
}

/// Each hit's step and id, in page order.
fn hits(out: &RelaxedResults) -> Vec<(usize, String)> {
    out.hits
        .iter()
        .map(|h| (h.step, h.hit.document.id().to_owned()))
        .collect()
}

#[test]
fn the_words_match_the_terms_they_start_and_the_step_states_l0s_text() {
    let docs = [
        "running shoes",
        "running shorts",
        "red shirt",
        "runner shoes",
        "walking boots",
        "run sh",
    ];
    let db = database(&docs);
    let registry = AnalyzerRegistry::builtin();
    // `sho` is in no document: L0 has no hit. `run` (3 characters) and `sho` (last) are expanded.
    let out = search(&db, &registry, "run  sho", None);
    let r = &out.relaxation;
    assert_eq!(r.level, RelaxLevel::Prefix);
    assert_eq!(r.applied, ["prefix"]);
    assert_eq!(r.effective.as_deref(), Some("run sho"));
    let mut ids: Vec<String> = hits(&out).into_iter().map(|h| h.1).collect();
    ids.sort();
    // `running shoes`, `running shorts` and `runner shoes`; not `run sh` (`sh` does not start with `sho`).
    assert_eq!(ids, ["d0", "d1", "d3"]);
    // `sh` is not last and has 2 characters: exact. `run sh` (d5) holds both, at L0; the step adds the documents
    // whose `ru` (last, expanded) word starts one of their terms and that hold `sh` exactly: none more.
    let out = search(&db, &registry, "sh ru", None);
    assert_eq!(hits(&out), [(1, "d5".to_owned())]);
    let out = search(&db, &registry, "sh run", None);
    assert_eq!(hits(&out), [(0, "d5".to_owned())]);
}

#[test]
fn a_prefix_keeps_the_fifty_terms_of_highest_df() {
    // `ab00`..`ab49` in two documents each, `ab50`..`ab59` in one: the 50 kept are the first ones.
    let mut docs = Vec::new();
    for i in 0..60 {
        let copies = if i < 50 { 2 } else { 1 };
        for _ in 0..copies {
            docs.push(format!("ab{i:02}"));
        }
    }
    let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
    let db = database(&refs);
    let out = search(&db, &AnalyzerRegistry::builtin(), "ab", None);
    assert_eq!(out.relaxation.applied, ["prefix"]);
    let found: BTreeSet<String> = hits(&out).into_iter().map(|h| h.1).collect();
    let want: BTreeSet<String> = (0..100).map(|i| format!("d{i}")).collect();
    assert_eq!(found, want);
}

#[test]
fn over_the_limits_the_prefixes_are_cut_or_the_step_skipped() {
    // Two words, each a prefix of 50: 100 expansions.
    let docs = ["abc1 abd1", "abc2 abd2", "abc3 abd3"];
    let db = database(&docs);
    let registry = AnalyzerRegistry::builtin();
    let limits = |max_expansions| QueryLimits {
        max_expansions,
        ..QueryLimits::default()
    };
    let out = search(&db, &registry, "abc abd", Some(limits(100)));
    assert_eq!(out.relaxation.applied, ["prefix"]);
    assert_eq!(hits(&out).len(), 3);
    // 2 left: each prefix keeps its most frequent term, ties by bytes (`abc1`, `abd1`).
    let out = search(&db, &registry, "abc abd", Some(limits(2)));
    assert_eq!(out.relaxation.applied, ["prefix:cut"]);
    assert_eq!(hits(&out), [(1, "d0".to_owned())]);
    // 1 left for two prefixes: no cut fits.
    let out = search(&db, &registry, "abc abd", Some(limits(1)));
    assert_eq!(out.relaxation.applied, ["prefix:skipped"]);
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    assert_eq!(hits(&out), []);
}

/// Words over `a` and `b` of 1 to 4 letters: 30 in all, so no prefix reaches the 50-term cap.
fn word() -> impl Strategy<Value = String> {
    prop::collection::vec(prop_oneof![Just('a'), Just('b')], 1..5)
        .prop_map(|w| w.into_iter().collect())
}

/// Whether `doc` holds a term `w` matches at L3: one starting with it when `open`, else `w` itself.
fn holds(doc: &[String], w: &str, open: bool) -> bool {
    doc.iter()
        .any(|t| if open { t.starts_with(w) } else { t == w })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 512 } else { 64 }))]

    #[test]
    fn the_step_matches_what_the_oracle_says(
        docs in prop::collection::vec(prop::collection::vec(word(), 1..5), 1..20),
        query in prop::collection::vec(word(), 1..4),
    ) {
        let texts: Vec<String> = docs.iter().map(|d| d.join(" ")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let db = database(&refs);
        let out = search(&db, &AnalyzerRegistry::builtin(), &query.join(" "), None);
        let last = query.len() - 1;
        let mut want = Vec::new();
        for (i, doc) in docs.iter().enumerate() {
            let l0 = query.iter().all(|w| holds(doc, w, false));
            let l3 = query
                .iter()
                .enumerate()
                .all(|(j, w)| holds(doc, w, j == last || w.chars().count() >= 3));
            if l0 {
                want.push((0, format!("d{i}")));
            } else if l3 {
                want.push((1, format!("d{i}")));
            }
        }
        let mut got = hits(&out);
        got.sort();
        want.sort();
        prop_assert_eq!(got, want);
    }
}
