//! L1 `synonyms` through a database (SPEC §6.3.2, §7.6, ROADMAP 8.2.1): the compiler finds the runs of required words
//! that are a term of the collection's synonym groups, and the cascade's L1 step ORs each with its alternatives.
//!
//! Invariants:
//! 1. Each default field scans its own analysis of the query's required words. A match is the longest term at a token
//!    whose texts and position gaps are equal (a stopword gap lines up). It counts only over whole clauses, and never
//!    spans an excluded clause, a phrase or a `field:` clause.
//! 2. An alternative is the other term compiled as a quoted phrase over the default fields would be. Alternatives come
//!    in group order, then term order, each once, none equal to the run's own query, at most 50 per run, and at most
//!    `max_terms` in all.
//! 3. L1 is one step, `synonyms`. It requires each run or one of its alternatives, keeps L0's excluded clauses
//!    excluded, and states L0's text as its `effective`. A programmatic query, or one without a match, skips the
//!    level.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, missing_docs)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, RelaxedResults, SearchRequest, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{
    BoolQuery, Compiled, DefaultFields, PhraseQuery, Query, QueryCompiler, QueryLimits, TermQuery,
};
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, Fallback, FieldKind, Level, Levels, RelaxationConfig,
    SchemaBuilder, TextOptions,
};
use persia_engine::search::Pruning;
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

fn groups(groups: &[&[&str]]) -> Vec<Vec<String>> {
    groups
        .iter()
        .map(|g| g.iter().map(|&t| t.to_owned()).collect())
        .collect()
}

/// `title` (standard, with positions) and `body` (en, without), with `synonyms`.
fn schema(synonyms: &[&[&str]]) -> CollectionSchema {
    SchemaBuilder::new("c")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field(
            "body",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("en").unwrap(),
                ..TextOptions::default()
            }),
        )
        .synonyms(groups(synonyms))
        .build()
        .unwrap()
}

const SYNONYMS: &[&[&str]] = &[
    &["tv", "television set"],
    &["sneakers", "running shoes", "trainers"],
    &["nyc", "new york"],
    &["statue of liberty", "lady liberty"],
];

fn compile_in(schema: &CollectionSchema, text: &str, limits: &QueryLimits) -> Compiled {
    QueryCompiler::new(schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(text, &DefaultFields::all(schema), limits)
}

fn compile(text: &str) -> Compiled {
    compile_in(&schema(SYNONYMS), text, &QueryLimits::default())
}

/// Each run's clauses as text, and how many alternatives it has.
fn runs(compiled: &Compiled) -> Vec<(String, usize)> {
    compiled
        .synonyms
        .iter()
        .map(|s| {
            let text: Vec<&str> = s
                .clauses
                .clone()
                .map(|i| compiled.source(i).unwrap())
                .collect();
            (text.join(" "), s.alternatives.len())
        })
        .collect()
}

fn term(field: &str, text: &str) -> Query {
    Query::Term(TermQuery {
        field: schema(SYNONYMS).field(field).unwrap().id(),
        term: text.into(),
    })
}

#[test]
fn a_run_of_whole_clauses_matches_in_each_field_with_its_stopword_gaps() {
    // One word and two words: `tv`, and `new york` over two clauses.
    assert_eq!(runs(&compile("cheap tv")), [("tv".into(), 1)]);
    assert_eq!(runs(&compile("new york pizza")), [("new york".into(), 1)]);
    assert_eq!(
        runs(&compile("sneakers and running shoes")),
        [("sneakers".into(), 2), ("running shoes".into(), 2)]
    );
    // `of` is a stopword of `body` (en), whose term `statu liberti` has a gap of 2 there; `title` keeps `of`. Both
    // match the three clauses, and share the one alternative.
    assert_eq!(
        runs(&compile("statue of liberty")),
        [("statue of liberty".into(), 1)]
    );
    // In `body` alone, the gap must line up: `statue liberty` has none, so neither spelling matches the other.
    let body_only = SchemaBuilder::new("c")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("en").unwrap(),
                ..TextOptions::default()
            }),
        )
        .synonyms(groups(&[&["statue liberty", "monument"]]))
        .build()
        .unwrap();
    let limits = QueryLimits::default();
    assert_eq!(
        runs(&compile_in(&body_only, "statue of liberty", &limits)),
        []
    );
    assert_eq!(
        runs(&compile_in(&body_only, "statue liberty", &limits)),
        [("statue liberty".into(), 1)]
    );
    // A dropped stopword clause (`the`, no field keeps it) leaves the gap too.
    let body_only = SchemaBuilder::new("c")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("en").unwrap(),
                ..TextOptions::default()
            }),
        )
        .synonyms(groups(&[&["statue of liberty", "monument"]]))
        .build()
        .unwrap();
    let compiled = compile_in(&body_only, "statue of liberty", &limits);
    assert_eq!(compiled.clauses.len(), 2);
    assert_eq!(compiled.synonyms[0].clauses, 0..2);
}

#[test]
fn excluded_phrase_and_field_clauses_are_not_expanded_and_end_a_run() {
    assert_eq!(runs(&compile("cheap -tv")), []);
    assert_eq!(runs(&compile("\"cheap tv\"")), []);
    assert_eq!(runs(&compile("title:tv")), []);
    // `york` alone is no term, and `new "york"` is a word and a phrase.
    assert_eq!(runs(&compile("new -x york")), []);
    assert_eq!(runs(&compile("new \"york\"")), []);
    // A run restarts after them.
    assert_eq!(runs(&compile("-x tv")), [("tv".into(), 1)]);
}

#[test]
fn a_match_inside_a_word_does_not_count() {
    let schema = schema(&[&["mail", "post"], &["e-mail", "email"]]);
    let limits = QueryLimits::default();
    // `e-mail` is two tokens of one clause: `e mail` is the term, `mail` alone is part of the clause.
    assert_eq!(
        runs(&compile_in(&schema, "e-mail", &limits)),
        [("e-mail".into(), 1)]
    );
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .synonyms(groups(&[&["mail", "post"]]))
        .build()
        .unwrap();
    assert_eq!(runs(&compile_in(&schema, "e-mail", &limits)), []);
    assert_eq!(
        runs(&compile_in(&schema, "mail", &limits)),
        [("mail".into(), 1)]
    );
}

#[test]
fn an_alternative_is_compiled_as_a_quoted_phrase() {
    // `title` has positions, `body` not: a phrase in `title`, both terms required in `body`, either field.
    let compiled = compile("tv");
    let [alternative] = compiled.synonyms[0].alternatives.as_slice() else {
        panic!("{:?}", compiled.synonyms);
    };
    let Query::Bool(BoolQuery { should, .. }) = alternative else {
        panic!("{alternative:?}");
    };
    // Default fields in schema order: `body`, then `title`.
    assert!(matches!(&should[1], Query::Phrase(PhraseQuery { terms, .. }) if terms.len() == 2));
    assert_eq!(
        should[0],
        Query::Bool(BoolQuery {
            must: vec![term("body", "televis"), term("body", "set")],
            ..BoolQuery::default()
        })
    );
    // A one-word alternative is a word over the fields: `nyc` for `new york`.
    let compiled = compile("new york");
    assert!(matches!(
        compiled.synonyms[0].alternatives[0],
        Query::Combined(_)
    ));
}

#[test]
fn alternatives_come_in_group_then_term_order_each_once_capped_at_50() {
    let words: Vec<String> = (0..60).map(|i| format!("w{i}")).collect();
    let mut first: Vec<&str> = vec!["x"];
    first.extend(words[..30].iter().map(String::as_str));
    // The second group repeats `w0` (once only), spells `x` as `X` (its own query: not an alternative), and goes on.
    let mut second: Vec<&str> = vec!["X", "x", "w0"];
    second.extend(words[30..].iter().map(String::as_str));
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .synonyms(groups(&[&first, &second]))
        .build()
        .unwrap();
    let compiled = compile_in(&schema, "x", &QueryLimits::default());
    // The 50-cap is the level's own bound, not a cut to fit the limits.
    assert!(!compiled.synonyms[0].cut);
    let alternatives: Vec<Query> = compiled.synonyms[0].alternatives.clone();
    let expected: Vec<Query> = words[..50]
        .iter()
        .map(|w| {
            Query::Combined(persia_engine::query::CombinedQuery {
                fields: vec![persia_engine::query::WeightedTerm {
                    field: schema.field("title").unwrap().id(),
                    term: w.clone(),
                    weight: 1.0,
                }],
            })
        })
        .collect();
    assert_eq!(alternatives, expected);
}

#[test]
fn alternatives_are_held_to_max_terms_in_all_the_longest_lists_cut_first() {
    let a: Vec<String> = (0..10).map(|i| format!("a{i}")).collect();
    let b: Vec<String> = (0..3).map(|i| format!("b{i}")).collect();
    let mut first: Vec<&str> = vec!["x"];
    first.extend(a.iter().map(String::as_str));
    let mut second: Vec<&str> = vec!["y"];
    second.extend(b.iter().map(String::as_str));
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .synonyms(groups(&[&first, &second]))
        .build()
        .unwrap();
    let counts = |max_terms| {
        let limits = QueryLimits {
            max_terms,
            ..QueryLimits::default()
        };
        compile_in(&schema, "x y", &limits)
            .synonyms
            .iter()
            .map(|s| (s.alternatives.len(), s.cut))
            .collect::<Vec<_>>()
    };
    assert_eq!(counts(1024), [(10, false), (3, false)]);
    // L0 is 2 terms, which L1's step holds twice: at 12, 8 are left. Both lists at 3 is 6, and the 2 left go to the
    // list that was cut.
    assert_eq!(counts(12), [(5, true), (3, false)]);
    assert_eq!(counts(9), [(3, true), (2, true)]);
    // At the floor, one each. Below it, the step could never fit: one each is kept, for the cascade to list it skipped.
    assert_eq!(counts(6), [(1, true), (1, true)]);
    assert_eq!(counts(5), [(1, true), (1, true)]);
}

#[test]
fn hostile_text_compiles_no_more_alternatives_than_the_step_could_hold() {
    // 2 000 words that each match a term of 50 alternatives: within the default limits L0 keeps 1023 of them, so L1's
    // step could hold none, and the compilation holds one per run, for the cascade to list the step skipped.
    let mut group: Vec<String> = vec!["a".into()];
    group.extend((0..50).map(|i| format!("b{i} c{i}")));
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .synonyms(vec![group])
        .build()
        .unwrap();
    let text = "a ".repeat(2000);
    let compiled = compile_in(&schema, &text, &QueryLimits::default());
    assert!(compiled.truncated);
    assert_eq!(compiled.synonyms.len(), 1023);
    assert!(
        compiled
            .synonyms
            .iter()
            .all(|s| s.alternatives.len() == 1 && s.cut)
    );
    // With room for some: 100 words leave 1024 − 200 terms, over 100 runs, 8 each.
    let compiled = compile_in(&schema, &"a ".repeat(100), &QueryLimits::default());
    let counts: Vec<usize> = compiled
        .synonyms
        .iter()
        .map(|s| s.alternatives.len())
        .collect();
    assert_eq!(counts.len(), 100);
    assert_eq!(counts.iter().sum::<usize>(), 824);
    assert!(counts.iter().all(|&n| n == 8 || n == 9), "{counts:?}");
}

#[test]
fn a_collection_without_synonyms_scans_nothing() {
    let compiled = compile_in(&schema(&[]), "cheap tv", &QueryLimits::default());
    assert_eq!(compiled.synonyms, []);
}

// The cascade.

fn database() -> Database<SimFile> {
    let schema = schema(SYNONYMS);
    let title = schema.field("title").unwrap().id();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [5; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    let docs = [
        ("a", "cheap television set", ""),
        ("b", "cheap set of television", ""),
        ("c", "running shoes sale", ""),
        ("d", "trainers for kids", ""),
        ("e", "", "a trip to new york"),
        ("f", "lady liberty tour", ""),
        ("g", "tv stand", ""),
        ("h", "cheap sneakers", ""),
    ];
    for (i, (id, t, b)) in docs.into_iter().enumerate() {
        let doc = Document::new(id)
            .with(title, Value::String(t.into()))
            .with(body, Value::String(b.into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i == 3 {
            db.commit().unwrap();
        }
    }
    db
}

fn exec() -> Execution {
    Execution::new(Pruning::default())
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

/// L0, L1 and L2, relaxing until `min_results` hits: L3 `prefix` (ROADMAP 8.2.3) would add a step to every query they
/// leave short.
fn plan(min_results: u32) -> RelaxPlan {
    let options = RelaxOptions {
        min_results: Some(min_results),
        levels: Some(Levels::Custom(vec![Level::Synonyms, Level::DropTerms])),
        fallback: Some(Fallback::None),
        ..RelaxOptions::default()
    };
    RelaxPlan::new(&RelaxationConfig::default(), &options).unwrap()
}

fn search<'q>(
    db: &Database<SimFile>,
    query: impl Into<persia_engine::db::RelaxedQuery<'q>>,
    relax: &RelaxPlan,
) -> RelaxedResults {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let request = SearchRequest {
        limit: 100,
        ..SearchRequest::default()
    };
    views
        .search_relaxed(query, &request, relax, &exec())
        .unwrap()
}

fn by_step(out: &RelaxedResults) -> Vec<(usize, &str)> {
    out.hits
        .iter()
        .map(|h| (h.step, h.hit.document.id()))
        .collect()
}

#[test]
fn l1_ors_each_run_with_its_alternatives_in_one_step() {
    let db = database();
    // No document holds `cheap` and `tv`; `a` holds the phrase `television set`, `b` both words apart.
    let compiled = compile("cheap tv");
    let out = search(&db, &compiled, &plan(1));
    let r = &out.relaxation;
    assert_eq!(r.level, RelaxLevel::Synonyms);
    assert_eq!(r.applied, ["synonyms"]);
    assert_eq!(r.effective.as_deref(), Some("cheap tv"));
    assert_eq!(by_step(&out), [(1, "a")]);

    // One run of two words, with two alternatives (`sneakers`, `trainers`): L0 finds `c`; L1 adds the documents of
    // the alternatives.
    let compiled = compile("running shoes");
    let out = search(&db, &compiled, &plan(3));
    assert_eq!(out.relaxation.applied[0], "synonyms");
    let hits = by_step(&out);
    assert_eq!(hits[0], (0, "c"));
    let l1: Vec<&str> = hits.iter().filter(|h| h.0 == 1).map(|h| h.1).collect();
    assert_eq!(l1.len(), 2, "{hits:?}");
    assert!(l1.contains(&"d") && l1.contains(&"h"), "{hits:?}");

    // Over both fields, with the stopword gap: `statue of liberty` finds `lady liberty`.
    let compiled = compile("statue of liberty");
    let out = search(&db, &compiled, &plan(1));
    assert_eq!(by_step(&out), [(1, "f")]);
    // `new york` and `nyc`, the alternative a word over both fields.
    let compiled = compile("nyc");
    let out = search(&db, &compiled, &plan(1));
    assert_eq!(by_step(&out), [(1, "e")]);
}

#[test]
fn excluded_clauses_stay_excluded_and_l2_follows() {
    let db = database();
    // `-stand` excludes `g` from every step; L2 then drops a term of `cheap tv` (any one of them).
    let compiled = compile("cheap tv -stand");
    let out = search(&db, &compiled, &plan(100));
    let r = &out.relaxation;
    assert_eq!(r.applied, ["synonyms", "drop_terms:75%"]);
    assert_eq!(by_step(&out)[0], (1, "a"));
    assert!(out.hits.iter().all(|h| h.hit.document.id() != "g"));
    assert!(
        out.hits
            .iter()
            .any(|h| h.step == 2 && h.hit.document.id() == "b")
    );
}

#[test]
fn a_query_without_a_match_or_text_skips_the_level() {
    let db = database();
    let compiled = compile("cheap stand");
    let out = search(&db, &compiled, &plan(1));
    assert_eq!(out.relaxation.applied, ["drop_terms:75%"]);
    // A programmatic query has no text to scan.
    let query = term("title", "tv");
    let out = search(&db, &query, &plan(100));
    assert!(
        out.relaxation
            .applied
            .iter()
            .all(|a| !a.starts_with("synonyms"))
    );
    assert_eq!(by_step(&out), [(0, "g")]);
}

#[test]
fn over_the_limits_l1_keeps_each_runs_first_alternatives_or_is_skipped() {
    let db = database();
    // `kids sneakers`: L0 is 4 terms (a word over 2 fields is 2). L1 is 10 of its own (`kids` 2, `sneakers` 2, the
    // phrase `running shoes` 4, `trainers` 2) beside L0's 4, which it excludes: 14.
    let compiled = compile("kids sneakers");
    let limited = |max_terms| {
        plan(1).with_limits(QueryLimits {
            max_terms,
            ..QueryLimits::default()
        })
    };
    let out = search(&db, &compiled, &limited(14));
    assert_eq!(out.relaxation.applied, ["synonyms"]);
    assert_eq!(by_step(&out), [(1, "d")]);
    // At 13, `sneakers` keeps its first alternative only, `running shoes`, which no document with `kids` holds. L2's
    // step (its own 4, L0's 4, L1's 8) does not fit either.
    let out = search(&db, &compiled, &limited(13));
    let r = &out.relaxation;
    assert_eq!(r.applied, ["synonyms:cut", "drop_terms:75%:skipped"]);
    assert_eq!(r.level, RelaxLevel::Synonyms);
    assert_eq!(r.effective.as_deref(), Some("kids sneakers"));
    assert_eq!(by_step(&out), []);
    // At 11, not even one alternative fits: L1 is skipped, so L2 does not exclude it, and fits.
    let out = search(&db, &compiled, &limited(11));
    assert_eq!(
        out.relaxation.applied,
        ["synonyms:skipped", "drop_terms:75%"]
    );
    let mut ids: Vec<&str> = by_step(&out).into_iter().map(|(_, id)| id).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["d", "h"]);
}

#[test]
fn alternatives_the_compilation_left_out_list_the_step_as_cut_even_when_it_fits() {
    // One default field, words of one term: `tv` is 1 term, and each alternative 1. At 4, the compilation keeps 2 of
    // the 3 (its own 1 and L0's 1 beside them), and the step fits exactly: still cut, `box` was left out.
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .synonyms(groups(&[&["tv", "telly", "set", "box"]]))
        .build()
        .unwrap();
    let title = schema.field("title").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [6; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema.clone()).unwrap();
    for (id, t) in [("a", "telly show"), ("b", "box office")] {
        let doc = Document::new(id).with(title, Value::String(t.into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
    }
    let run = |max_terms| {
        let limits = QueryLimits {
            max_terms,
            ..QueryLimits::default()
        };
        let compiled = compile_in(&schema, "tv", &limits);
        let out = search(&db, &compiled, &plan(1).with_limits(limits));
        let ids: Vec<String> = by_step(&out)
            .into_iter()
            .map(|(_, id)| id.to_owned())
            .collect();
        (out.relaxation.applied, ids)
    };
    assert_eq!(
        run(5),
        (
            vec!["synonyms".to_owned()],
            vec!["a".to_owned(), "b".to_owned()]
        )
    );
    assert_eq!(
        run(4),
        (vec!["synonyms:cut".to_owned()], vec!["a".to_owned()])
    );
    // At 2 the step holds L0 twice and no alternative: one is kept, and the step is skipped.
    assert_eq!(run(2).0, ["synonyms:skipped"]);
}

#[test]
fn alternatives_keep_their_place_in_the_group_they_expand_from() {
    // `zz` was first given in group 0, but `tv` expands from group 1, where `telly` comes first.
    let schema = schema(&[&["q", "zz"], &["tv", "telly", "zz"]]);
    let order = |max_terms| {
        let limits = QueryLimits {
            max_terms,
            ..QueryLimits::default()
        };
        compile_in(&schema, "tv", &limits).synonyms[0]
            .alternatives
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
    };
    let both = order(1024);
    assert_eq!(both.len(), 2);
    assert!(
        both[0].contains("telly") && both[1].contains("zz"),
        "{both:?}"
    );
    // L0's 2 terms twice, and room for one: `telly` is kept.
    let one = order(5);
    assert_eq!(one.len(), 1);
    assert!(one[0].contains("telly"), "{one:?}");
}

#[test]
fn a_term_ending_inside_a_word_does_not_hide_a_shorter_one() {
    // `tv e` would end inside `e-mail`; `tv` alone ends with a word, and matches.
    let schema = schema(&[&["tv", "telly"], &["tv e", "x"]]);
    let limits = QueryLimits::default();
    assert_eq!(
        runs(&compile_in(&schema, "tv e-mail", &limits)),
        [("tv".into(), 1)]
    );
    // Nor one starting inside a word: `mail tv` starts in `e-mail`.
    let schema = self::schema(&[&["tv", "telly"], &["mail tv", "x"]]);
    assert_eq!(
        runs(&compile_in(&schema, "e-mail tv", &limits)),
        [("tv".into(), 1)]
    );
}

/// `body` (en) alone, with `synonyms`.
fn english(synonyms: &[&[&str]]) -> CollectionSchema {
    SchemaBuilder::new("c")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("en").unwrap(),
                ..TextOptions::default()
            }),
        )
        .synonyms(groups(synonyms))
        .build()
        .unwrap()
}

#[test]
fn every_removed_token_keeps_its_position() {
    let limits = QueryLimits::default();
    // `the` at the end of `tv-the` is removed, and still takes a position: `tv show` has no gap there.
    let schema = english(&[&["tv show", "series"]]);
    assert_eq!(runs(&compile_in(&schema, "tv-the show", &limits)), []);
    assert_eq!(runs(&compile_in(&schema, "tv the show", &limits)), []);
    assert_eq!(
        runs(&compile_in(&schema, "tv show", &limits)),
        [("tv show".into(), 1)]
    );
    // A dropped clause of two stopwords is a gap of two.
    let schema = english(&[&["statue of-the liberty", "monument"]]);
    for text in ["statue of-the liberty", "statue of the liberty"] {
        assert_eq!(
            runs(&compile_in(&schema, text, &limits)).len(),
            1,
            "{text:?}"
        );
    }
    assert_eq!(runs(&compile_in(&schema, "statue of liberty", &limits)), []);
}

#[test]
fn fields_that_match_the_same_clauses_share_alternatives_and_an_overlap_is_dropped() {
    let limits = QueryLimits::default();
    // `body` (en) stems `shoes` to `shoe`, so it expands `shoe` from both groups; `title` (standard) from the first
    // only. The run gets both, in group order.
    let schema = schema(&[&["shoe", "boot"], &["shoes", "sneaker"]]);
    let compiled = compile_in(&schema, "shoe", &limits);
    let alternatives: Vec<String> = compiled.synonyms[0]
        .alternatives
        .iter()
        .map(|a| format!("{a:?}"))
        .collect();
    assert_eq!(alternatives.len(), 2);
    assert!(
        alternatives[0].contains("boot") && alternatives[1].contains("sneaker"),
        "{alternatives:?}"
    );
    // `body` (en, the first default field) matches `runs shoes` over `running shoes` (both stem to `run shoe`);
    // `title` (standard) matches `shoes store` over the last two words, which overlaps: it is dropped.
    let schema = self::schema(&[&["runs shoes", "sneakers"], &["shoes store", "shop"]]);
    assert_eq!(
        runs(&compile_in(&schema, "running shoes store", &limits)),
        [("running shoes".into(), 1)]
    );
}

#[test]
fn pages_after_a_cursor_continue_l1_whether_it_was_cut_or_not() {
    let db = database();
    // L0 finds nothing; L1 is 10 terms of its own beside L0's 4. Cut to 12, `sneakers` keeps `running shoes`, which
    // `c` holds beside `sale`.
    let compiled = compile("sale sneakers");
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    for (max_terms, applied) in [(1024, "synonyms"), (12, "synonyms:cut")] {
        let relax = plan(10).with_limits(QueryLimits {
            max_terms,
            ..QueryLimits::default()
        });
        let all = SearchRequest {
            limit: 100,
            ..SearchRequest::default()
        };
        let whole = views
            .search_relaxed(&compiled, &all, &relax, &exec())
            .unwrap();
        assert_eq!(whole.relaxation.applied[0], applied);
        let mut paged = Vec::new();
        let mut after = None;
        loop {
            let page = SearchRequest {
                limit: 1,
                search_after: after.take(),
                ..SearchRequest::default()
            };
            let out = views
                .search_relaxed(&compiled, &page, &relax, &exec())
                .unwrap();
            assert_eq!(out.relaxation.applied, whole.relaxation.applied);
            let Some(hit) = out.hits.last() else { break };
            paged.push((hit.step, hit.hit.document.id().to_owned()));
            after = Some(hit.cursor());
        }
        let whole: Vec<(usize, String)> = whole
            .hits
            .iter()
            .map(|h| (h.step, h.hit.document.id().to_owned()))
            .collect();
        assert_ne!(whole.len(), 0);
        assert_eq!(paged, whole, "max_terms {max_terms}");
    }
}
