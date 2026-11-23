//! Relaxation quality over a synthetic suite (SPEC §7.6; ROADMAP 8.4.1, 8.4.2): a corpus and typo and term-drop
//! queries with known ground truth (`persia_testutil::synthetic`), searched through the cascade, measured as:
//! - the **zero-result rate before relaxation**: the share of queries with no hit at L0 alone (`relax: false`);
//! - the **zero-result rate after relaxation**: the share with no hit under the relaxation settings measured;
//! - the **precision of the first relaxed hit**: of the queries whose first hit was found by a step after L0, the
//!   share whose first hit is a document the query meant (holds every intended word).
//!
//! Invariants:
//! 1. The metrics are what their definitions count, on outcomes built by hand.
//! 2. The suite's queries match nothing as typed (the generator's ground truth), so the rate before relaxation is 1,
//!    and every first hit of a relaxed search is a relaxed one.
//! 3. The engine's own counters (`RelaxationStats`, ROADMAP 8.3.3) agree with the measure: queries, still empty,
//!    relaxed.
//!
//! Every run prints its seed and the measure, so a failure can be reproduced and read.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    missing_docs
)] // Tests: inputs are generated from fixed seeds.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, SearchRequest, SegmentIds, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::Projection;
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use persia_testutil::synthetic::{
    Corpus, CorpusOptions, QueryGenerator, QueryKind, QueryOptions, SyntheticQuery,
};

/// What one query did: whether L0 alone found a hit, and the first hit of the relaxed search (its id, and whether a
/// step after L0 found it).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    exact_hit: bool,
    first: Option<(String, bool)>,
}

/// The counts the metrics are ratios of, for a set of queries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    queries: u64,
    /// No hit at L0 alone.
    zero_before: u64,
    /// No hit under the relaxation settings.
    zero_after: u64,
    /// The first hit found by a step after L0.
    relaxed_first: u64,
    /// Of those, a document the query meant.
    relaxed_first_relevant: u64,
}

impl Counts {
    fn add(&mut self, query: &SyntheticQuery, outcome: &Outcome) {
        self.queries += 1;
        self.zero_before += u64::from(!outcome.exact_hit);
        match &outcome.first {
            None => self.zero_after += 1,
            Some((id, true)) => {
                self.relaxed_first += 1;
                self.relaxed_first_relevant += u64::from(query.is_relevant(id));
            }
            Some((_, false)) => {}
        }
    }

    fn ratio(part: u64, whole: u64) -> f64 {
        if whole == 0 {
            0.0
        } else {
            part as f64 / whole as f64
        }
    }

    /// The share of queries with no hit at L0 alone.
    fn zero_rate_before(&self) -> f64 {
        Self::ratio(self.zero_before, self.queries)
    }

    /// The share of queries with no hit after relaxation.
    fn zero_rate_after(&self) -> f64 {
        Self::ratio(self.zero_after, self.queries)
    }

    /// The share of first relaxed hits that are relevant; `None` when no first hit was relaxed.
    fn first_relaxed_precision(&self) -> Option<f64> {
        (self.relaxed_first > 0)
            .then(|| Self::ratio(self.relaxed_first_relevant, self.relaxed_first))
    }
}

/// The metrics of a suite: over every query, and per kind of query.
#[derive(Debug, Clone, Default, PartialEq)]
struct Quality {
    all: Counts,
    by_kind: BTreeMap<QueryKind, Counts>,
}

impl Quality {
    fn add(&mut self, query: &SyntheticQuery, outcome: &Outcome) {
        self.all.add(query, outcome);
        self.by_kind
            .entry(query.kind)
            .or_default()
            .add(query, outcome);
    }

    /// One row per kind, then `all`: the counts and the three metrics, as percentages to two decimals.
    fn rows(&self) -> Vec<(String, Counts)> {
        let mut rows: Vec<(String, Counts)> = QueryKind::ALL
            .iter()
            .filter_map(|k| self.by_kind.get(k).map(|c| (k.name().to_owned(), *c)))
            .collect();
        rows.push(("all".to_owned(), self.all));
        rows
    }
}

impl fmt::Display for Quality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{:<10} {:>7} {:>12} {:>11} {:>16}",
            "kind", "queries", "zero before", "zero after", "first relaxed ok"
        )?;
        for (name, c) in self.rows() {
            let precision = c
                .first_relaxed_precision()
                .map_or_else(|| "-".to_owned(), |p| format!("{:.2}%", 100.0 * p));
            writeln!(
                f,
                "{name:<10} {:>7} {:>11.2}% {:>10.2}% {precision:>16}",
                c.queries,
                100.0 * c.zero_rate_before(),
                100.0 * c.zero_rate_after(),
            )?;
        }
        Ok(())
    }
}

/// A suite: a synthetic corpus indexed in a database, and queries drawn from it, both from `seed`.
struct Suite {
    seed: u64,
    db: Database<SimFile>,
    queries: Vec<SyntheticQuery>,
}

const COLLECTION: &str = "docs";

impl Suite {
    /// The corpus of `options` from `seed`, written in one `body` field (`standard`), committed into segments of a
    /// quarter of the documents each with the last part in the memtable, on a fixed clock and segment ids; and `n`
    /// queries from the seed after it.
    fn new(seed: u64, options: &CorpusOptions, n: usize) -> Self {
        let corpus = Corpus::synthetic(seed, options);
        let schema = SchemaBuilder::new(COLLECTION)
            .field("body", FieldKind::Text(TextOptions::default()))
            .build()
            .unwrap();
        let body = schema.field("body").unwrap().id();
        let db_options = DatabaseOptions {
            clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
            segment_ids: SegmentIds::seeded(seed),
            ..DatabaseOptions::default()
        };
        let mut db = Database::create_on(SimFile::new(), [9; 16], db_options).unwrap();
        db.create_collection(schema).unwrap();
        let quarter = (corpus.docs().len() / 4).max(1);
        for (i, doc) in corpus.docs().iter().enumerate() {
            let d = Document::new(doc.id.as_str()).with(body, Value::String(doc.text()));
            db.put(COLLECTION, &d, Durability::Buffered).unwrap();
            if (i + 1) % quarter == 0 && i + 1 < corpus.docs().len() {
                db.commit().unwrap();
            }
        }
        let queries =
            QueryGenerator::new(&corpus, QueryOptions::default()).generate(seed.wrapping_add(1), n);
        assert_eq!(
            queries.len(),
            n,
            "seed {seed}: the corpus gave fewer queries"
        );
        Self { seed, db, queries }
    }

    /// Every query searched under `options` (over the collection's settings) and with `relax: false`, measured; and
    /// what the engine's counters (ROADMAP 8.3.3) counted of the relaxed pass: `(queries, relaxed, empty)`.
    fn measure(&self, options: &RelaxOptions) -> (Quality, (u64, u64, u64)) {
        let snapshot = self.db.snapshot();
        let c = snapshot.collection(COLLECTION).unwrap();
        let compiler = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin()).unwrap();
        let fields = DefaultFields::all(c.schema());
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let relaxed = RelaxPlan::new(c.schema().relaxation(), options).unwrap();
        let exact = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::disabled()).unwrap();
        let request = SearchRequest {
            limit: 1,
            projection: Projection::None,
            ..SearchRequest::default()
        };
        // A budget clock that never moves: the budget (SPEC §7.6) never truncates a cascade, so the measure does not
        // depend on the machine's speed.
        let exec = Execution::default()
            .threads(Threads::Current)
            .budget_clock(Arc::new(ManualClock::new(0)));
        let search = |q: &SyntheticQuery, plan: &RelaxPlan| {
            let text = compiler.compile(&q.text, &fields, &QueryLimits::default());
            views.search_relaxed(&text, &request, plan, &exec).unwrap()
        };
        let exact_hits: Vec<bool> = self
            .queries
            .iter()
            .map(|q| !search(q, &exact).hits.is_empty())
            .collect();
        let before = c.relaxation_stats();
        let firsts: Vec<Option<(String, bool)>> = self
            .queries
            .iter()
            .map(|q| {
                let out = search(q, &relaxed);
                let first = out.hits.first()?;
                Some((
                    first.hit.document.id().to_owned(),
                    first.level != RelaxLevel::Exact,
                ))
            })
            .collect();
        let after = c.relaxation_stats();
        let mut quality = Quality::default();
        for ((q, exact_hit), first) in self.queries.iter().zip(exact_hits).zip(firsts) {
            quality.add(q, &Outcome { exact_hit, first });
        }
        let counted = (
            after.queries - before.queries,
            after.relaxed - before.relaxed,
            after.empty - before.empty,
        );
        (quality, counted)
    }
}

fn print(seed: u64, what: &str, quality: &Quality) -> String {
    let mut out = String::new();
    writeln!(out, "seed {seed}, {what}:\n{quality}").unwrap();
    out
}

#[test]
fn the_metrics_count_what_they_say_on_outcomes_by_hand() {
    let corpus = Corpus::from_texts([
        ("a", "alpha bravo charlie"),
        ("b", "alpha bravo"),
        ("c", "delta echo"),
    ])
    .unwrap();
    let query = |kind, intended: &[&str], relevant: &[&str]| SyntheticQuery {
        seed: 0,
        kind,
        text: String::new(),
        intended: intended.iter().map(|w| (*w).to_owned()).collect(),
        target: relevant[0].to_owned(),
        relevant: relevant.iter().map(|w| (*w).to_owned()).collect(),
        typo: None,
        added: None,
    };
    assert_eq!(corpus.matching(&["alpha", "bravo"]), ["a", "b"]);
    let typo = query(QueryKind::Typo, &["alpha", "bravo"], &["a", "b"]);
    let drop = query(QueryKind::Drop, &["delta"], &["c"]);
    let outcome = |exact_hit, first: Option<(&str, bool)>| Outcome {
        exact_hit,
        first: first.map(|(id, r)| (id.to_owned(), r)),
    };
    let mut q = Quality::default();
    // Relaxed, relevant; relaxed, not relevant; still empty; L0 had a hit (not relaxed).
    q.add(&typo, &outcome(false, Some(("b", true))));
    q.add(&typo, &outcome(false, Some(("c", true))));
    q.add(&drop, &outcome(false, None));
    q.add(&drop, &outcome(true, Some(("c", false))));
    assert_eq!(
        q.all,
        Counts {
            queries: 4,
            zero_before: 3,
            zero_after: 1,
            relaxed_first: 2,
            relaxed_first_relevant: 1,
        }
    );
    assert_eq!(q.all.zero_rate_before(), 0.75);
    assert_eq!(q.all.zero_rate_after(), 0.25);
    assert_eq!(q.all.first_relaxed_precision(), Some(0.5));
    assert_eq!(q.by_kind[&QueryKind::Drop].first_relaxed_precision(), None);
    assert_eq!(q.by_kind[&QueryKind::Typo].zero_rate_after(), 0.0);
    assert_eq!(Counts::default().zero_rate_after(), 0.0);
    let table = q.to_string();
    assert!(
        table.contains("typo") && table.contains("50.00%"),
        "{table}"
    );
}

/// The seed of the suite `cargo test` runs; `PERSIA_QUALITY_SEED` picks another.
const SEED: u64 = 0x8402_0000_0001;

fn seed() -> u64 {
    std::env::var("PERSIA_QUALITY_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SEED)
}

#[test]
fn the_suite_is_empty_before_relaxation_and_the_engine_counts_as_the_measure() {
    let seed = seed();
    let suite = Suite::new(seed, &CorpusOptions::default(), 150);
    let (quality, (queries, relaxed, empty)) = suite.measure(&RelaxOptions::default());
    let report = print(suite.seed, "the collection's settings", &quality);
    eprintln!("{report}");
    let all = quality.all;
    // No query matches as typed (the generator's ground truth, and the engine's analysis keeps its words).
    assert_eq!(all.zero_before, all.queries, "{report}");
    // So every first hit is a relaxed one.
    assert_eq!(all.relaxed_first, all.queries - all.zero_after, "{report}");
    // The engine counted every query, each relaxed (L0 found nothing), and as many still empty.
    assert_eq!(
        (queries, relaxed, empty),
        (all.queries, all.queries, all.zero_after),
        "{report}"
    );
}
