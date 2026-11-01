//! Relevance sanity (ROADMAP 7.8.2): a small judged corpus (`tests/relevance/corpus.json`, 54 store items and
//! how-to articles) and queries with graded judgments (`judgments.json`: 3 perfect, 2 relevant, 1 marginal, unjudged
//! 0), searched as user text (SPEC §7.1.1) over `title` (boost 2) and `body` with the `en` analyzer, scored by
//! BM25F (SPEC §7.2).
//!
//! Invariants:
//! 1. Every judged query returns hits, its best hit is judged (grade 1 or more), and a relevant document (grade 2
//!    or more) is in its top 3. Not every best hit is relevant: for `tent`, `Tent stakes` (grade 1) ranks first, its
//!    two-token title outweighing the tents' longer ones under length normalization (`b = 0.75`). The baseline keeps
//!    it visible.
//! 2. nDCG@10 per query and its mean are the baseline recorded in `tests/relevance/baseline.txt`, to four decimals.
//!    A change to analysis or scoring that moves any of them fails here: if it is intended, replace the file with
//!    the table the failure prints, and say in the PR which queries moved and why. The regression gate with a
//!    tolerance is ROADMAP 7.8.4.
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

#[test]
fn ndcg_matches_the_recorded_baseline() {
    let got = table(&run());
    assert!(
        got == BASELINE,
        "nDCG@{K} moved from tests/relevance/baseline.txt; if intended, replace it with:\n{got}"
    );
}
