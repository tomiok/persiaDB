//! L4 `fuzzy` through a database (SPEC §7.6, ROADMAP 8.2.4): a compiled query's words matching the terms within the
//! edit distance their length allows, planned over the collection's terms (each segment's FST intersected with the
//! automaton, and the memtable's terms, SPEC §7.1).
//!
//! Invariants:
//! 1. L4's step matches a document when, for every required word, the document holds a term within the word's
//!    distance (optimal string alignment over characters: 1 for 4 to 7 characters, 2 from 8) or, for a shorter word,
//!    the word itself, beside L0's exclusions and filters; a document L0 matches is L0's (checked against an oracle
//!    over random corpora split between a segment and the memtable).
//! 2. A `fuzzy` keeps at most 50 terms, those of highest collection `df` (SPEC §7.1); over `QueryLimits`, the step's
//!    `fuzzy` queries are cut (`fuzzy:cut`) or the step is skipped (`fuzzy:skipped`, SPEC §7.1.3).
//! 3. An exact term outranks a correction at the same frequency and length (the edit-distance weight, SPEC §7.1).
//! 4. Pages by cursor are slices of the one ranking across L0 and L4.
//! 5. Typo tiers (ROADMAP 8.2.7): when a word may take two edits, L4 searches every word within one edit first (`fuzzy:1`),
//!    so its hits rank above those that need two, whatever their scores (checked against the oracle of 1).
//!
//! `tests/differential.rs` checks L4 against its reference model too (ROADMAP 8.1.21), but after L3, which takes the
//! completions first. The oracle of invariant 1 stays: L4 alone after L0, over two letters, where every word has many
//! neighbours within one and two edits.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, missing_docs)] // Tests: inputs are fixed.

use std::collections::BTreeSet;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, RelaxedResults, SearchRequest, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, Query, QueryCompiler, QueryLimits, TermQuery};
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    Fallback, FieldKind, Level, Levels, RelaxationConfig, SchemaBuilder, TextOptions,
};
use persia_engine::search::{Projection, Pruning};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;

/// One `body` field (`standard`) with `docs`; a commit after the first half, so the `fuzzy` queries expand over a
/// segment and the memtable.
fn database(docs: &[&str]) -> Database<SimFile> {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [6; 16], DatabaseOptions::default()).unwrap();
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

/// L0, then L4 alone: no other level runs first.
fn plan() -> RelaxPlan {
    let options = RelaxOptions {
        min_results: Some(*RelaxationConfig::MIN_RESULTS.end()),
        levels: Some(Levels::Custom(vec![Level::Fuzzy])),
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

fn request(limit: usize) -> SearchRequest {
    SearchRequest {
        limit,
        projection: Projection::None,
        ..SearchRequest::default()
    }
}

fn search_with(
    db: &Database<SimFile>,
    text: &str,
    limits: Option<QueryLimits>,
    request: &SearchRequest,
) -> RelaxedResults {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let compiled = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(
            text,
            &DefaultFields::all(c.schema()),
            &QueryLimits::default(),
        );
    let relax = limits.map_or_else(plan, |l| plan().with_limits(l));
    views
        .search_relaxed(&compiled, request, &relax, &exec())
        .unwrap()
}

fn search(db: &Database<SimFile>, text: &str, limits: Option<QueryLimits>) -> RelaxedResults {
    search_with(db, text, limits, &request(1000))
}

/// Each hit's step and id, in page order.
fn hits(out: &RelaxedResults) -> Vec<(usize, String)> {
    out.hits
        .iter()
        .map(|h| (h.step, h.hit.document.id().to_owned()))
        .collect()
}

fn ids(out: &RelaxedResults) -> BTreeSet<String> {
    hits(out).into_iter().map(|h| h.1).collect()
}

#[test]
fn misspelled_words_match_the_terms_within_their_distance() {
    let docs = [
        "zapatillas rojas",
        "zapatillas azules",
        "botas rojas",
        "zapatos rojos",
    ];
    let db = database(&docs);
    // `zapatilas` (9 characters, distance 2) is one edit from `zapatillas` and more than two from `zapatos`. `rojs` (4, distance 1) is one insertion from `rojas` and from `rojos`.
    let out = search(&db, "zapatilas rojs", None);
    let r = &out.relaxation;
    assert_eq!(r.level, RelaxLevel::Fuzzy);
    // `zapatilas` may take two edits, so the tier step within one edit comes first (ROADMAP 8.2.7): it finds `d0`, and
    // the step at two edits adds nothing.
    assert_eq!(r.applied, ["fuzzy:1", "fuzzy"]);
    assert_eq!(hits(&out), [(1, "d0".to_owned())]);
    // A short word stays exact: `red` (3) matches only itself.
    let db = database(&["red shoes", "rad shoes", "red shoez"]);
    let out = search(&db, "red shoex", None);
    assert_eq!(
        ids(&out),
        BTreeSet::from(["d0".to_owned(), "d2".to_owned()])
    );
}

#[test]
fn an_exact_term_outranks_a_correction() {
    // `shoes` is a word of both queries; `redd` is in no document, so L0 has no hit. `shoes` matches `shoes` exactly
    // (weight 1) and `shoe` within one edit (weight 1/2): at the same frequency and length, `d0` ranks first.
    let db = database(&["shoe red", "shoes red"]);
    let out = search(&db, "shoes redd", None);
    assert_eq!(hits(&out), [(1, "d1".to_owned()), (1, "d0".to_owned())]);
    assert!(out.hits[0].hit.score > out.hits[1].hit.score);
}

/// 76 terms one edit from `abcde`, none of them `abcde`: `abcd?e` (26), `?bcde` (25) and `a?cde` (25).
fn neighbours() -> Vec<String> {
    let letters = || ('a'..='z').map(String::from);
    let mut terms: Vec<String> = letters().map(|c| format!("abcd{c}e")).collect();
    terms.extend(letters().filter(|c| c != "a").map(|c| format!("{c}bcde")));
    terms.extend(letters().filter(|c| c != "b").map(|c| format!("a{c}cde")));
    terms
}

#[test]
fn a_fuzzy_keeps_the_fifty_terms_of_highest_df() {
    // The first 50 terms in two documents each, the other 26 in one: the 50 kept are the first ones, whichever view
    // holds their documents (half are in the segment, half in the memtable).
    let terms = neighbours();
    let mut docs = Vec::new();
    let mut want = BTreeSet::new();
    for (i, term) in terms.iter().enumerate() {
        let copies = if i < 50 { 2 } else { 1 };
        for _ in 0..copies {
            if i < 50 {
                want.insert(format!("d{}", docs.len()));
            }
            docs.push(term.clone());
        }
    }
    let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
    let db = database(&refs);
    let out = search(&db, "abcde", None);
    assert_eq!(out.relaxation.applied, ["fuzzy"]);
    assert_eq!(ids(&out), want);
}

#[test]
fn over_the_limits_the_fuzzy_queries_are_cut_or_the_step_skipped() {
    // Two words, each a `fuzzy` of 50: 100 expansions.
    let db = database(&["abcd1 wxyz1", "abcd2 wxyz2", "abcd2 wxyz2", "abcd3 wxyz3"]);
    let limits = |max_expansions| QueryLimits {
        max_expansions,
        ..QueryLimits::default()
    };
    let out = search(&db, "abcd wxyz", Some(limits(100)));
    assert_eq!(out.relaxation.applied, ["fuzzy"]);
    assert_eq!(hits(&out).len(), 4);
    // 2 left: each keeps its most frequent term (`abcd2`, `wxyz2`).
    let out = search(&db, "abcd wxyz", Some(limits(2)));
    assert_eq!(out.relaxation.applied, ["fuzzy:cut"]);
    assert_eq!(
        ids(&out),
        BTreeSet::from(["d1".to_owned(), "d2".to_owned()])
    );
    // The corrections are the terms each cut `fuzzy` kept: the step searched them.
    assert_eq!(corrected(&out), [("abcd", "abcd2"), ("wxyz", "wxyz2")]);
    assert_eq!(out.relaxation.effective.as_deref(), Some("abcd2 wxyz2"));
    // 1 left for two queries: no cut fits.
    let out = search(&db, "abcd wxyz", Some(limits(1)));
    assert_eq!(out.relaxation.applied, ["fuzzy:skipped"]);
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    assert_eq!(hits(&out), []);
    // A skipped step states nothing: L0's text, no correction.
    assert_eq!(corrected(&out), []);
    assert_eq!(out.relaxation.effective.as_deref(), Some("abcd wxyz"));
}

#[test]
fn pages_by_cursor_are_slices_of_one_ranking() {
    let db = database(&[
        "boots red",
        "boot red",
        "boats red",
        "boost red",
        "bots red",
        "boots blue",
    ]);
    let all = search(&db, "boots red", None);
    // L0 holds `d0`; L4 the documents whose words are within one edit of both words.
    assert_eq!(all.hits[0].step, 0);
    assert!(all.hits.len() > 2);
    let mut paged = Vec::new();
    let mut page = request(1);
    loop {
        let out = search_with(&db, "boots red", None, &page);
        let Some(last) = out.hits.last() else {
            break;
        };
        assert_eq!(out.relaxation.level, RelaxLevel::Fuzzy);
        page.search_after = Some(last.cursor());
        paged.extend(hits(&out));
    }
    assert_eq!(paged, hits(&all));
}

#[test]
fn a_programmatic_term_relaxes_too_without_text() {
    let db = database(&["sandals", "sandal", "scandals"]);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let body = c.schema().field("body").unwrap().id();
    let query = Query::Term(TermQuery {
        field: body,
        term: "sandels".into(),
    });
    let out = views
        .search_relaxed(&query, &request(10), &plan(), &exec())
        .unwrap();
    assert_eq!(out.relaxation.level, RelaxLevel::Fuzzy);
    assert_eq!(out.relaxation.effective, None);
    assert_eq!(out.relaxation.corrected, []);
    // `sandals` (one substitution) and `sandal` (a substitution and a deletion is two: out); `scandals` is two away.
    assert_eq!(ids(&out), BTreeSet::from(["d0".to_owned()]));
}

/// The textbook optimal string alignment distance, over characters.
#[allow(clippy::needless_range_loop, clippy::many_single_char_names)] // The textbook form.
fn osa(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0_usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=m {
        d[0][j] = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[n][m]
}

/// The distance SPEC §7.6 gives a word of L4, by its length in characters.
fn allowed(w: &str) -> usize {
    match w.chars().count() {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// Words over `a` and `b` of 1 to 8 letters, so every distance is reached. A corpus holds at most 36 distinct words
/// (12 documents of 3), so no `fuzzy` reaches the 50-term cap. At least 2 documents, so a commit splits every corpus
/// between a segment and the memtable.
fn word() -> impl Strategy<Value = String> {
    prop::collection::vec(prop_oneof![Just('a'), Just('b')], 1..9)
        .prop_map(|w| w.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 512 } else { 64 }))]

    #[test]
    fn the_step_matches_what_the_oracle_says(
        docs in prop::collection::vec(prop::collection::vec(word(), 1..4), 2..13),
        query in prop::collection::vec(word(), 1..4),
    ) {
        let texts: Vec<String> = docs.iter().map(|d| d.join(" ")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let db = database(&refs);
        let out = search(&db, &query.join(" "), None);
        // A word of 8 letters or more may take two edits: L4 then searches within one edit first, its tier step
        // (ROADMAP 8.2.7), so every document within one edit of each word ranks above every other L4 hit.
        let tiered = query.iter().any(|w| allowed(w) == 2);
        let within = |doc: &Vec<String>, cap: usize| {
            query.iter().all(|w| doc.iter().any(|t| osa(w, t) <= allowed(w).min(cap)))
        };
        let mut want = Vec::new();
        for (i, doc) in docs.iter().enumerate() {
            let l0 = query.iter().all(|w| doc.contains(w));
            let step = if l0 {
                Some(0)
            } else if tiered && within(doc, 1) {
                Some(1)
            } else if within(doc, 2) {
                Some(if tiered { 2 } else { 1 })
            } else {
                None
            };
            if let Some(step) = step {
                want.push((step, format!("d{i}")));
            }
        }
        // The page ranks by step first: the tier step's hits lead, whatever their BM25F scores.
        prop_assert!(out.hits.windows(2).all(|w| w[0].step <= w[1].step));
        let mut got = hits(&out);
        got.sort();
        want.sort();
        prop_assert_eq!(got, want);
    }
}

/// The corrections of `out`, as `(original, corrected)` pairs.
fn corrected(out: &RelaxedResults) -> Vec<(&str, &str)> {
    out.relaxation
        .corrected
        .iter()
        .map(|c| (c.original.as_str(), c.corrected.as_str()))
        .collect()
}

#[test]
fn words_no_document_holds_are_corrected_and_effective_reads_the_corrections() {
    let db = database(&[
        "zapatillas rojas",
        "zapatillas azules",
        "botas rojas",
        "zapatos rojos",
    ]);
    // `rojas` (df 2) wins over `rojos` (df 1); the word keeps how the user wrote it, the correction is the index's term.
    let out = search(&db, "Zapatilas  rojs", None);
    assert_eq!(
        corrected(&out),
        [("Zapatilas", "zapatillas"), ("rojs", "rojas")]
    );
    assert_eq!(
        out.relaxation.effective.as_deref(),
        Some("zapatillas rojas")
    );
    assert_eq!(out.relaxation.original.as_deref(), Some("Zapatilas  rojs"));
}

#[test]
fn a_word_some_document_holds_is_not_corrected() {
    // `shoes` is held (and widened to `shoe`), `redd` is not: only `redd` is reported, as `red`.
    let db = database(&["shoe red", "shoes red"]);
    let out = search(&db, "shoes redd", None);
    assert_eq!(corrected(&out), [("redd", "red")]);
    assert_eq!(out.relaxation.effective.as_deref(), Some("shoes red"));
}

#[test]
fn equal_df_corrections_go_to_the_lower_bytes_and_far_words_stay_as_written() {
    // `boots` and `boats` both hold df 1 and are one edit from `boxts`: `boats` sorts first. `zzzzz` is near no term,
    // so it is not corrected, and the step matches nothing (it requires a term within one edit of it).
    let db = database(&["boots", "boats", "boots zzzzy"]);
    let out = search(&db, "boxts", None);
    assert_eq!(corrected(&out), [("boxts", "boots")]);
    let db = database(&["boots", "boats"]);
    let out = search(&db, "boxts", None);
    assert_eq!(corrected(&out), [("boxts", "boats")]);
    // L4 corrects `boxts` but finds no hit, so no step did: the response states L0's text and no correction (SPEC
    // §7.6: the hit step, ROADMAP 8.1.22), though L4 was built.
    let out = search(&db, "boxts qqqqq", None);
    assert_eq!(out.relaxation.level, RelaxLevel::Fuzzy);
    assert_eq!(corrected(&out), []);
    assert_eq!(out.relaxation.effective.as_deref(), Some("boxts qqqqq"));
    assert_eq!(hits(&out), []);
}

#[test]
fn exclusions_stay_in_effective_and_split_or_short_words_are_not_corrected() {
    let db = database(&["red shoes", "blue shoes", "mail red"]);
    // The excluded clause is kept as written; `shoez` is corrected.
    let out = search(&db, "-blue shoez", None);
    assert_eq!(corrected(&out), [("shoez", "shoes")]);
    assert_eq!(out.relaxation.effective.as_deref(), Some("-blue shoes"));
    assert_eq!(ids(&out), BTreeSet::from(["d0".to_owned()]));
    // `e-mial` is two tokens (`e`, `mial`): searched fuzzily per token, but a word of several parts is not reported.
    // `e` is short and stays exact, so a document must hold it for the step to find the hit it states.
    let mail = database(&["e mail red"]);
    let out = search(&mail, "e-mial redd", None);
    assert_eq!(corrected(&out), [("redd", "red")]);
    assert_eq!(out.relaxation.effective.as_deref(), Some("e-mial red"));
    // `rde` has 3 characters: no `fuzzy`, no correction, no step.
    let out = search(&db, "rde", None);
    assert_eq!(out.relaxation.level, RelaxLevel::Exact);
    assert_eq!(corrected(&out), []);
}

#[test]
fn every_page_states_the_same_corrections() {
    let db = database(&["boots red", "boot red", "boats red", "bots red"]);
    let all = search(&db, "bootz red", None);
    // `boot` (a deletion), `boots` (a substitution) are within one edit, all df 1: `boot` sorts first.
    assert_eq!(corrected(&all), [("bootz", "boot")]);
    let mut page = request(1);
    let mut pages = 0;
    loop {
        let out = search_with(&db, "bootz red", None, &page);
        let Some(last) = out.hits.last() else {
            break;
        };
        assert_eq!(out.relaxation.corrected, all.relaxation.corrected);
        assert_eq!(out.relaxation.effective, all.relaxation.effective);
        page.search_after = Some(last.cursor());
        pages += 1;
    }
    assert_eq!(pages, all.hits.len());
}

#[test]
fn over_fields_the_correction_of_highest_df_wins() {
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let title = schema.field("title").unwrap().id();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [6; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    // `shoez`: `shoes` in one title, `shoed` in three bodies.
    for (id, t, b) in [
        ("a", "shoes", "x"),
        ("b", "x", "shoed"),
        ("c", "x", "shoed"),
        ("d", "x", "shoed"),
    ] {
        let doc = Document::new(id)
            .with(title, Value::String(t.into()))
            .with(body, Value::String(b.into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
    }
    let out = search(&db, "shoez", None);
    assert_eq!(corrected(&out), [("shoez", "shoed")]);
    assert_eq!(hits(&out).len(), 4);
}

/// The collection's levels (L1, L2, L3, L4), `min_results` 10 and no fallback: every step is built.
fn every_level() -> RelaxPlan {
    RelaxPlan::new(
        &RelaxationConfig::default(),
        &RelaxOptions {
            min_results: Some(10),
            fallback: Some(Fallback::None),
            ..RelaxOptions::default()
        },
    )
    .unwrap()
}

fn search_every_level(
    db: &Database<SimFile>,
    text: &str,
    request: &SearchRequest,
) -> RelaxedResults {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let compiled = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(
            text,
            &DefaultFields::all(c.schema()),
            &QueryLimits::default(),
        );
    views
        .search_relaxed(&compiled, request, &every_level(), &exec())
        .unwrap()
}

/// SPEC §7.6, ROADMAP 8.1.22: the response states the *hit step*'s `effective` and `corrected`, the last step that
/// found a hit, not the last one built. With the collection's levels, `red shoez` relaxes through L2
/// (`drop_terms:75%`: any one word, so every `red` document), L3 and L4 (`red` and a term near `shoez`), whose matches
/// L2 already took: L4 corrects `shoez` to `shoes` but finds no hit, so the response states L2's `red` and no
/// correction, on every page.
#[test]
fn a_last_step_that_adds_no_hit_states_the_step_that_found_one() {
    let db = database(&["red shoes", "red hat"]);
    let out = search_every_level(&db, "red shoez", &request(10));
    assert_eq!(
        out.relaxation.applied,
        ["drop_terms:75%", "prefix", "fuzzy"]
    );
    assert!(out.hits.iter().all(|h| h.level == RelaxLevel::DropTerms));
    assert_eq!(out.relaxation.level, RelaxLevel::Fuzzy);
    assert_eq!(corrected(&out), []);
    assert_eq!(out.relaxation.effective.as_deref(), Some("red"));
    // Page by page: the same text, from a cursor whose page searches L2 only.
    let mut page = request(1);
    let mut pages = 0;
    loop {
        let out = search_every_level(&db, "red shoez", &page);
        let Some(last) = out.hits.last() else {
            break;
        };
        assert_eq!(out.relaxation.effective.as_deref(), Some("red"));
        assert_eq!(corrected(&out), []);
        page.search_after = Some(last.cursor());
        pages += 1;
    }
    assert_eq!(pages, 2);
}

/// The hit step reaches a page that does not search it (ROADMAP 8.1.22): `boots shoez` is held by no document whole.
/// L2's one-word rung finds `d0` and `d1` (`boots`), and L4 finds `d2` (`boot` and `shoes`, one edit from each word),
/// so L4 is the hit step. A page of one hit after `d0` is filled by `d1` without searching L4, and still states L4's
/// correction, from the cursor.
#[test]
fn every_page_states_the_hit_step_though_it_does_not_search_it() {
    let db = database(&["boots", "boots x", "boot shoes"]);
    let all = search_every_level(&db, "boots shoez", &request(10));
    assert_eq!(
        hits(&all),
        [
            (1, "d0".to_owned()),
            (1, "d1".to_owned()),
            (3, "d2".to_owned())
        ]
    );
    assert_eq!(corrected(&all), [("shoez", "shoes")]);
    assert_eq!(all.relaxation.effective.as_deref(), Some("boots shoes"));
    let mut page = request(1);
    let mut seen = Vec::new();
    loop {
        let out = search_every_level(&db, "boots shoez", &page);
        let Some(last) = out.hits.last() else {
            break;
        };
        assert_eq!(out.relaxation.corrected, all.relaxation.corrected);
        assert_eq!(out.relaxation.effective, all.relaxation.effective);
        assert_eq!(out.relaxation.applied, all.relaxation.applied);
        let cursor = last.cursor();
        assert_eq!(cursor.relaxed.map(|s| s.hit), Some(3));
        seen.push(last.hit.document.id().to_owned());
        page.search_after = Some(cursor);
    }
    assert_eq!(seen, ["d0", "d1", "d2"]);
}

/// ROADMAP 8.2.7: a word of 8 letters or more is searched within one edit first (`fuzzy:1`), then within two, and pages
/// by cursor are slices of that one ranking: every hit of the tier step, whatever its score, before the next step's.
#[test]
fn tier_steps_page_by_cursor_as_one_ranking() {
    // `sandalls` is one edit from `sandals` and two from `scandals`; the far ones repeat their word, so they would
    // outscore the near ones in one step.
    let db = database(&[
        "sandals with a long description of leather straps",
        "sandals and more words about summer beaches",
        "scandals scandals scandals",
        "scandal scandal",
        "boots",
    ]);
    let all = search(&db, "sandalls", None);
    assert_eq!(all.relaxation.applied, ["fuzzy:1", "fuzzy"]);
    // `scandal` is three edits away.
    let steps: Vec<usize> = all.hits.iter().map(|h| h.step).collect();
    assert_eq!(steps, [1, 1, 2]);
    assert_eq!(all.hits[2].hit.document.id(), "d2");
    // Within one step the far ones keep their own scores; across steps the near ones lead whatever they score.
    assert!(all.hits[2].hit.score > all.hits[0].hit.score);
    let mut paged = Vec::new();
    let mut page = request(1);
    loop {
        let out = search_with(&db, "sandalls", None, &page);
        let Some(last) = out.hits.last() else {
            break;
        };
        assert_eq!(out.relaxation.applied, ["fuzzy:1", "fuzzy"]);
        page.search_after = Some(last.cursor());
        paged.extend(hits(&out));
    }
    assert_eq!(paged, hits(&all));
    // The tier step's corrections are within one edit: `sandalls` reads `sandals`, the hit step's.
    assert_eq!(corrected(&all), [("sandalls", "sandals")]);
    assert_eq!(all.relaxation.effective.as_deref(), Some("sandals"));
    // Even where a term two edits away is more frequent, a cascade that stops at the tier step (`min_results` 1)
    // corrects within one edit, the distance its hits were searched at.
    let db = database(&["sandals", "scandals", "scandals", "scandals", "boots"]);
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let compiled = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(
            "sandalls",
            &DefaultFields::all(c.schema()),
            &QueryLimits::default(),
        );
    let first = RelaxOptions {
        levels: Some(Levels::Custom(vec![Level::Fuzzy])),
        fallback: Some(Fallback::None),
        ..RelaxOptions::default()
    };
    let relax = RelaxPlan::new(&RelaxationConfig::default(), &first).unwrap();
    let out = views
        .search_relaxed(&compiled, &request(10), &relax, &exec())
        .unwrap();
    assert_eq!(hits(&out), [(1, "d0".to_owned())]);
    assert_eq!(out.relaxation.applied, ["fuzzy:1"]);
    assert_eq!(corrected(&out), [("sandalls", "sandals")]);
    // Searched to the end, the hit step is `fuzzy`, which corrects to the most frequent term within two edits.
    assert_eq!(
        corrected(&search(&db, "sandalls", None)),
        [("sandalls", "scandals")]
    );
}

/// ROADMAP 8.2.7, SPEC §7.1.3: a tier step counts against the limits as any step, and once searched the next step
/// excludes it in full, its expansions counted there and not cut. Two words of 8 characters or more: 100 expansions a
/// step.
#[test]
fn tier_steps_are_held_to_the_limits_and_counted_where_excluded() {
    let db = database(&["zapatillas bolsitas", "zapatos bolsas", "botas"]);
    let limits = |max_expansions| QueryLimits {
        max_expansions,
        ..QueryLimits::default()
    };
    for (max, applied) in [
        (1024, ["fuzzy:1", "fuzzy"]),
        // `fuzzy:1` fits; `fuzzy` has 50 left beside the 100 it excludes, so it is cut.
        (150, ["fuzzy:1", "fuzzy:cut"]),
        // Nothing left beside `fuzzy:1`: `fuzzy` is skipped.
        (100, ["fuzzy:1", "fuzzy:skipped"]),
        // `fuzzy:1` is cut to 60, which `fuzzy` then excludes: nothing left.
        (60, ["fuzzy:1:cut", "fuzzy:skipped"]),
        // One expansion for two `fuzzy` queries: both skipped, and a skipped step is not excluded.
        (1, ["fuzzy:1:skipped", "fuzzy:skipped"]),
    ] {
        let out = search(&db, "zapatilas bolsitass", Some(limits(max)));
        assert_eq!(out.relaxation.applied, applied, "max_expansions {max}");
        let ids: Vec<String> = hits(&out).into_iter().map(|h| h.1).collect();
        let distinct: BTreeSet<&String> = ids.iter().collect();
        assert_eq!(
            distinct.len(),
            ids.len(),
            "max_expansions {max}: a hit twice"
        );
        // At 1 both steps are skipped, and this plan has no L5: the page is empty.
        let expected = if max > 1 {
            vec![(1, "d0".to_owned())]
        } else {
            Vec::new()
        };
        assert_eq!(hits(&out), expected, "max_expansions {max}");
    }
}
