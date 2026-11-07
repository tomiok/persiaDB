//! Relevance sanity (ROADMAP 7.8.2) and its nDCG@10 regression gate (7.8.4): a small judged corpus
//! (`tests/relevance/corpus.json`, 54 store items and how-to articles) and queries with graded judgments
//! (`judgments.json`: 3 perfect, 2 relevant, 1 marginal, unjudged 0), searched as user text (SPEC §7.1.1) over
//! `title` (boost 2) and `body` with the `en` analyzer, scored by BM25F (SPEC §7.2).
//!
//! Invariants:
//! 1. Every judged query returns hits, its best hit is judged (grade 1 or more), and a relevant document (grade 2
//!    or more) is in its top 3. Not every best hit is relevant: for `tent`, `Tent stakes` (grade 1) ranks first, its
//!    two-token title outweighing the tents' longer ones under length normalization (`b = 0.75`). The baseline keeps
//!    it visible.
//! 2. The regression gate (ROADMAP 7.8.4): nDCG@10 per query and its mean stay within one unit of the fourth decimal
//!    ([`TOLERANCE_UNITS`]) of the baseline recorded in `tests/relevance/baseline.txt` (four decimals, the same
//!    queries in the same order). A value below it is a regression and fails; a value above it fails too, so an
//!    improvement lands only with the baseline updated on purpose: replace the file with the table the failure
//!    prints, and say in the PR which queries moved and why (a lower mean needs a stated reason). The test runs in
//!    `cargo test --workspace`, which CI runs on Linux and macOS.
//! 3. The tolerance absorbs only rounding: the corpus, clock and segment ids are fixed, so the ranking is the same
//!    everywhere, and only `ln`, `log2` and `powf` may round differently by platform, which can flip a fourth
//!    decimal on a boundary (one unit, `0.0001`). Per query, it is less than half the smallest change one judged
//!    document makes moving by one rank, so every such move in the top 10 fails the gate (two documents of one grade
//!    trading places change nothing, and pass).
//!
//! nDCG@10 is `DCG@10 / IDCG@10`, with `DCG@10 = Σ_{i=1..10} (2^grade_i − 1) / log2(i + 1)` over the hits in rank
//! order, and `IDCG@10` the same over the judgments sorted by grade.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    missing_docs
)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{Database, DatabaseOptions, ManualClock, SearchRequest, SegmentIds};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::schema::{AnalyzerRef, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::{Projection, Pruning, SearchPlan};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use serde_json::Value as Json;

const CORPUS: &str = include_str!("relevance/corpus.json");
const JUDGMENTS: &str = include_str!("relevance/judgments.json");
const BASELINE: &str = include_str!("relevance/baseline.txt");
const K: usize = 10;

/// `DCG@K` of `grades` in rank order.
fn dcg(grades: impl IntoIterator<Item = u32>) -> f64 {
    grades
        .into_iter()
        .take(K)
        .zip(1..)
        .map(|(g, i): (u32, i32)| (2_f64.powf(f64::from(g)) - 1.0) / f64::from(i + 1).log2())
        .sum()
}

/// nDCG@K of the ranked ids under `judged`.
fn ndcg(ranked: &[String], judged: &BTreeMap<String, u32>) -> f64 {
    let mut ideal: Vec<u32> = judged.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg = dcg(ideal);
    if idcg == 0.0 {
        return 0.0;
    }
    dcg(ranked.iter().map(|id| judged.get(id).copied().unwrap_or(0))) / idcg
}

#[test]
fn ndcg_by_hand() {
    let judged: BTreeMap<String, u32> = [("a", 3), ("b", 1)]
        .into_iter()
        .map(|(id, g)| (id.to_owned(), g))
        .collect();
    let ids = |s: &[&str]| s.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
    assert!((ndcg(&ids(&["a", "b"]), &judged) - 1.0).abs() < 1e-12);
    // b, a: (1 + 7 / log2 3) / (7 + 1 / log2 3).
    let swapped = (1.0 + 7.0 / 3_f64.log2()) / (7.0 + 1.0 / 3_f64.log2());
    assert!((ndcg(&ids(&["b", "a"]), &judged) - swapped).abs() < 1e-12);
    assert!(ndcg(&ids(&["x", "y"]), &judged).abs() < 1e-12);
}

/// The ranked ids of each judged query, and its judgments.
fn run() -> Vec<(String, Vec<String>, BTreeMap<String, u32>)> {
    let en = AnalyzerRef::new("en", 1).unwrap();
    let text = |boost| {
        FieldKind::Text(TextOptions {
            analyzer: en.clone(),
            boost,
            positions: true,
            ..TextOptions::default()
        })
    };
    let schema = SchemaBuilder::new("store")
        .field("title", text(2.0))
        .field("body", text(1.0))
        .build()
        .unwrap();
    let title = schema.field("title").unwrap().id();
    let body = schema.field("body").unwrap().id();
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [7; 16], options).unwrap();
    db.create_collection(schema).unwrap();

    let corpus: Vec<Json> = serde_json::from_str(CORPUS).unwrap();
    // Two segments and the memtable: scores are collection-level, so the layout does not matter.
    for (i, doc) in corpus.iter().enumerate() {
        let d = Document::new(doc["id"].as_str().unwrap())
            .with(title, Value::String(doc["title"].as_str().unwrap().into()))
            .with(body, Value::String(doc["body"].as_str().unwrap().into()));
        db.put("store", &d, Durability::Buffered).unwrap();
        if i == corpus.len() / 3 || i == 2 * corpus.len() / 3 {
            db.commit().unwrap();
        }
    }

    let snapshot = db.snapshot();
    let c = snapshot.collection("store").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let stats = c.search_statistics(&memtable);
    let compiler = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin()).unwrap();
    let fields = DefaultFields::all(c.schema());
    let request = SearchRequest {
        limit: K,
        projection: Projection::None,
        ..SearchRequest::default()
    };
    let judgments: Vec<Json> = serde_json::from_str(JUDGMENTS).unwrap();
    judgments
        .iter()
        .map(|j| {
            let q = j["q"].as_str().unwrap();
            let text = compiler.compile(q, &fields, &QueryLimits::default());
            let plan = SearchPlan::with_terms(&text.query, c.schema(), &stats, &views).unwrap();
            let hits = views
                .search(&plan, &request, Pruning::default())
                .unwrap()
                .hits;
            let ranked = hits.iter().map(|h| h.document.id().to_owned()).collect();
            let judged = j["grades"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(id, g)| (id.clone(), u32::try_from(g.as_u64().unwrap()).unwrap()))
                .collect();
            (q.to_owned(), ranked, judged)
        })
        .collect()
}

/// The table `baseline.txt` records: one line per query, then the mean.
fn table(runs: &[(String, Vec<String>, BTreeMap<String, u32>)]) -> String {
    let mut out = String::new();
    let mut sum = 0.0;
    for (q, ranked, judged) in runs {
        let n = ndcg(ranked, judged);
        sum += n;
        writeln!(out, "{n:.4}  {q}").unwrap();
    }
    writeln!(out, "{:.4}  mean nDCG@{K}", sum / runs.len() as f64).unwrap();
    out
}

#[test]
fn judged_queries_find_relevant_documents() {
    let runs = run();
    assert!(runs.len() >= 15);
    for (q, ranked, judged) in &runs {
        let grade = |id: &String| judged.get(id).copied().unwrap_or(0);
        let top = ranked.first().unwrap_or_else(|| panic!("{q:?}: no hit"));
        assert!(
            grade(top) >= 1,
            "{q:?}: best hit {top} is not judged: {ranked:?}"
        );
        assert!(
            ranked.iter().take(3).any(|id| grade(id) >= 2),
            "{q:?}: no relevant document in the top 3: {ranked:?}"
        );
    }
}

/// How far a recorded value may move before the gate fails, in units of its fourth decimal: one, the rounding of
/// the recorded value plus a flip on a rounding boundary (see the module doc, invariant 3).
const TOLERANCE_UNITS: i64 = 1;
/// [`TOLERANCE_UNITS`] as an nDCG@10 difference.
#[allow(clippy::as_conversions)] // A small integer, exact as a float.
const TOLERANCE: f64 = TOLERANCE_UNITS as f64 / 10_000.0;

/// The `(value, label)` rows of a table, the mean last, each value in units of its fourth decimal (`0.9023` is
/// 9023): the gate compares the recorded digits exactly, never two parsed floats.
fn rows(table: &str) -> Vec<(i64, String)> {
    table
        .lines()
        .map(|line| {
            let parsed = line.split_once("  ").and_then(|(value, label)| {
                let (int, frac) = value.split_once('.')?;
                let units = int.parse::<i64>().ok()? * 10_000 + frac.parse::<i64>().ok()?;
                (frac.len() == 4).then(|| (units, label.to_owned()))
            });
            parsed.unwrap_or_else(|| panic!("row {line:?} is not `d.dddd  label`"))
        })
        .collect()
}

/// `units` as the table prints it.
fn decimal(units: i64) -> String {
    format!("{}.{:04}", units / 10_000, units % 10_000)
}

/// The gate's verdict on `got` against `baseline`: `Ok` when every row stays within [`TOLERANCE_UNITS`], else the
/// moved rows, regressions first.
fn gate(baseline: &str, got: &str) -> Result<(), String> {
    let (old, new) = (rows(baseline), rows(got));
    let labels = |rows: &[(i64, String)]| rows.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>();
    if labels(&old) != labels(&new) {
        return Err(format!(
            "the judged queries changed: baseline {:?}, now {:?}",
            labels(&old),
            labels(&new)
        ));
    }
    let moved = |worse: bool| {
        old.iter()
            .zip(&new)
            .filter(|((a, _), (b, _))| (b - a).abs() > TOLERANCE_UNITS && (b < a) == worse)
            .map(|((a, l), (b, _))| format!("  {l}: {} -> {}", decimal(*a), decimal(*b)))
            .collect::<Vec<_>>()
    };
    let (dropped, rose) = (moved(true), moved(false));
    if dropped.is_empty() && rose.is_empty() {
        return Ok(());
    }
    let mut out = String::new();
    if !dropped.is_empty() {
        writeln!(out, "regression, nDCG@{K} dropped:\n{}", dropped.join("\n")).unwrap();
    }
    if !rose.is_empty() {
        writeln!(
            out,
            "nDCG@{K} rose (update the baseline):\n{}",
            rose.join("\n")
        )
        .unwrap();
    }
    Err(out)
}

#[test]
fn ndcg_stays_within_tolerance_of_the_baseline() {
    let got = table(&run());
    if let Err(moved) = gate(BASELINE, &got) {
        panic!(
            "{moved}\nIf intended, replace tests/relevance/baseline.txt with the table below and say in the PR \
             which queries moved and why:\n{got}"
        );
    }
}

#[test]
fn gate_fails_beyond_tolerance_either_way() {
    let base = "0.5000  a\n0.8000  b\n0.6500  mean nDCG@10\n";
    assert_eq!(gate(base, base), Ok(()));
    // A flip on a rounding boundary passes.
    assert_eq!(
        gate(base, "0.4999  a\n0.8001  b\n0.6500  mean nDCG@10\n"),
        Ok(())
    );
    let drop = gate(base, "0.4998  a\n0.8000  b\n0.6499  mean nDCG@10\n").unwrap_err();
    assert!(
        drop.starts_with("regression") && drop.contains("a: 0.5000 -> 0.4998"),
        "{drop}"
    );
    assert!(!drop.contains("mean"), "{drop}");
    let rise = gate(base, "0.5000  a\n0.8200  b\n0.6600  mean nDCG@10\n").unwrap_err();
    assert!(
        rise.starts_with("nDCG@10 rose") && rise.contains("mean nDCG@10: 0.6500 -> 0.6600"),
        "{rise}"
    );
    let both = gate(base, "0.4000  a\n0.9000  b\n0.6500  mean nDCG@10\n").unwrap_err();
    assert!(
        both.contains("a: 0.5000 -> 0.4000") && both.contains("b: 0.8000 -> 0.9000"),
        "{both}"
    );
    // A query added, removed, reordered or renamed without the baseline, even at the same values.
    for changed in [
        "0.5000  a\n0.6500  mean nDCG@10\n",
        "0.8000  b\n0.5000  a\n0.6500  mean nDCG@10\n",
        "0.5000  a\n0.8000  c\n0.6500  mean nDCG@10\n",
    ] {
        let err = gate(base, changed).unwrap_err();
        assert!(err.starts_with("the judged queries changed"), "{err}");
    }
}

/// Invariant 3: per query, the smallest change of nDCG@10 one judged document makes moving by one rank (ranks 9 and
/// 10, the smallest discount step, swapped with the closest other grade, unjudged included) is more than twice the
/// tolerance, so the gate sees it whatever the rounding.
#[test]
fn tolerance_is_below_one_rank_move() {
    let judgments: Vec<Json> = serde_json::from_str(JUDGMENTS).unwrap();
    let step = 1.0 / 10_f64.log2() - 1.0 / 11_f64.log2();
    for j in &judgments {
        let mut ideal: Vec<u32> = j["grades"]
            .as_object()
            .unwrap()
            .values()
            .map(|g| u32::try_from(g.as_u64().unwrap()).unwrap())
            .collect();
        ideal.sort_unstable_by(|a, b| b.cmp(a));
        let idcg = dcg(ideal.iter().copied());
        // The gains of the grades present, and 0; the closest two differ least.
        let mut gains: Vec<f64> = ideal
            .iter()
            .map(|&g| 2_f64.powf(f64::from(g)) - 1.0)
            .collect();
        gains.push(0.0);
        gains.sort_by(f64::total_cmp);
        gains.dedup();
        let least = gains
            .windows(2)
            .map(|w| w[1] - w[0])
            .fold(f64::INFINITY, f64::min);
        let smallest = least * step / idcg;
        assert!(
            smallest > 2.0 * TOLERANCE,
            "{:?}: a one-rank move changes nDCG@{K} by {smallest:.6}, within the tolerance",
            j["q"]
        );
    }
}
