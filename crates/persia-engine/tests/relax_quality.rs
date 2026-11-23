//! Relaxation quality over a synthetic suite (SPEC §7.6; ROADMAP 8.4.1–8.4.3): a corpus and typo and term-drop
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
//! 4. The regression gate (ROADMAP 8.4.3) fails, naming the check, when (a) with the default settings any query has
//!    no hit, (b) with `fallback: "none"` more than 2% have none, or (c) the first-relaxed-hit precision is more than
//!    2 points below its row in `relax_quality/baseline.txt` (or the suite's query count changed); checked on counts by
//!    hand, at the exact edges.
//! 5. On the real suite the gate passes with the defaults, the measure is deterministic, and the gate fails with L2
//!    `drop_terms` or L4 `fuzzy` left out ((b) and (c)), with both and the fallback left out ((a)), and below its
//!    baseline. What it cannot see, as measured: L1 (the corpus has no synonyms) and L3 left out (L4 corrects the same
//!    queries) measure the same, and no query reaches L5 with the default levels, so (b) measures as (a) there.
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
use persia_engine::schema::{Fallback, FieldKind, Level, Levels, SchemaBuilder, TextOptions};
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

/// The metrics of a suite: over every query, per kind of query, and per kind and number of intended words (one, or
/// more: with more, L2 `drop_terms` can answer a typo by dropping the misspelled word before L4 corrects it).
#[derive(Debug, Clone, Default, PartialEq)]
struct Quality {
    all: Counts,
    by_kind: BTreeMap<QueryKind, Counts>,
    /// By kind and whether the query meant more than one word.
    by_length: BTreeMap<(QueryKind, bool), Counts>,
}

impl Quality {
    fn add(&mut self, query: &SyntheticQuery, outcome: &Outcome) {
        self.all.add(query, outcome);
        self.by_kind
            .entry(query.kind)
            .or_default()
            .add(query, outcome);
        self.by_length
            .entry((query.kind, query.intended.len() > 1))
            .or_default()
            .add(query, outcome);
    }

    /// One row per kind, each followed by its one-word (`1w`) and longer (`2w+`) queries, then `all`.
    fn rows(&self) -> Vec<(String, Counts)> {
        let mut rows = Vec::new();
        for kind in QueryKind::ALL {
            let Some(c) = self.by_kind.get(&kind) else {
                continue;
            };
            rows.push((kind.name().to_owned(), *c));
            for (multi, tag) in [(false, "1w"), (true, "2w+")] {
                if let Some(c) = self.by_length.get(&(kind, multi)) {
                    rows.push((format!(" {} {tag}", kind.name()), *c));
                }
            }
        }
        rows.push(("all".to_owned(), self.all));
        rows
    }
}
impl fmt::Display for Quality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{:<14} {:>7} {:>12} {:>11} {:>16}",
            "kind", "queries", "zero before", "zero after", "first relaxed ok"
        )?;
        for (name, c) in self.rows() {
            let precision = c
                .first_relaxed_precision()
                .map_or_else(|| "-".to_owned(), |p| format!("{:.2}%", 100.0 * p));
            writeln!(
                f,
                "{name:<14} {:>7} {:>11.2}% {:>10.2}% {precision:>16}",
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
    /// queries from the seed after it, of 1 to 3 intended words. One-word queries are what make L4 count: with 2 words
    /// or more, L2 `drop_terms`' one-word rung finds a hit before L4 runs, so the suite would pass without L4.
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
        let queries = QueryGenerator::new(
            &corpus,
            QueryOptions {
                words: 1..=3,
                ..QueryOptions::default()
            },
        )
        .generate(seed.wrapping_add(1), n);
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
    // By length: both typo queries meant two words, both drops one.
    assert_eq!(
        q.by_length[&(QueryKind::Typo, true)].relaxed_first_relevant,
        1
    );
    assert_eq!(q.by_length[&(QueryKind::Drop, false)].queries, 2);
    assert!(!q.by_length.contains_key(&(QueryKind::Typo, false)));
    assert_eq!(Counts::default().zero_rate_after(), 0.0);
    let table = q.to_string();
    assert!(
        table.contains("typo")
            && table.contains(" typo 2w+")
            && table.contains(" drop 1w")
            && table.contains("50.00%"),
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

// ---------------------------------------------------------------------------------------------------------------
// The regression gate (ROADMAP 8.4.3)

/// The first-relaxed-hit precision each suite last recorded (`relax_quality/baseline.txt`).
const BASELINE: &str = include_str!("relax_quality/baseline.txt");
/// The largest drop of the precision below its baseline the gate lets pass, in hundredths of a percentage point (2
/// points). Precisions are compared in hundredths, the baseline's two decimals, so the bound is exact: no float
/// rounding decides a run at the edge.
const PRECISION_DROP: i64 = 200;
/// The largest zero-result rate after relaxation with `fallback: "none"`, in percent, compared on the counts.
const NO_FALLBACK_ZERO: u64 = 2;

/// A precision in percent as whole hundredths of a point.
#[allow(clippy::cast_possible_truncation)] // Percentages: |p| ≤ 100.
fn hundredths(percent: f64) -> i64 {
    (percent * 100.0).round() as i64
}

/// A row of the baseline: the suite, its seed, its query count, and the precision in percent (two decimals).
#[derive(Debug, Clone, PartialEq)]
struct Row {
    suite: String,
    seed: u64,
    queries: u64,
    precision: f64,
}

impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:<5} {:>16} {:>7} {:>6.2}",
            self.suite, self.seed, self.queries, self.precision
        )
    }
}

/// The rows of a baseline text: one per line `suite seed queries precision`, `#` lines and blank ones skipped.
fn rows(text: &str) -> Result<Vec<Row>, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let [suite, seed, queries, precision] = parts[..] else {
                return Err(format!(
                    "a baseline row is `suite seed queries precision`: {line:?}"
                ));
            };
            let parse = |what: &str, v: &str| format!("{what} {v:?} in {line:?}");
            Ok(Row {
                suite: suite.to_owned(),
                seed: seed.parse().map_err(|_| parse("seed", seed))?,
                queries: queries.parse().map_err(|_| parse("queries", queries))?,
                precision: precision
                    .parse()
                    .map_err(|_| parse("precision", precision))?,
            })
        })
        .collect()
}

/// The gate on one suite run (ROADMAP 8.4.3), `Err` naming each check that fails:
/// (a) with the collection's default settings no query is left with no hit;
/// (b) with `fallback: "none"`, at most [`NO_FALLBACK_ZERO`] percent are;
/// (c) the default settings' first-relaxed-hit precision is at most 2 points ([`PRECISION_DROP`]) below the baseline
///     row of the same suite and seed, over the same number of queries. A run with no such row (another seed) skips
///     (c).
fn gate(
    baseline: &[Row],
    got: &Row,
    defaults: &Counts,
    no_fallback: &Counts,
) -> Result<(), String> {
    let mut failed = Vec::new();
    if defaults.zero_after != 0 {
        failed.push(format!(
            "(a) {} of {} queries have no hit with the default settings (must be 0)",
            defaults.zero_after, defaults.queries
        ));
    }
    if no_fallback.zero_after * 100 > NO_FALLBACK_ZERO * no_fallback.queries {
        let zero = 100.0 * no_fallback.zero_rate_after();
        failed.push(format!(
            "(b) {zero:.2}% of queries have no hit with `fallback: \"none\"` (at most {NO_FALLBACK_ZERO}%)"
        ));
    }
    match baseline
        .iter()
        .find(|r| r.suite == got.suite && r.seed == got.seed)
    {
        Some(row) if row.queries != got.queries => failed.push(format!(
            "(c) the suite has {} queries, its baseline row {}: record the baseline again",
            got.queries, row.queries
        )),
        Some(row) if hundredths(got.precision) < hundredths(row.precision) - PRECISION_DROP => {
            failed.push(format!(
                "(c) first-relaxed-hit precision {:.2}% is more than 2 points below the baseline's {:.2}%",
                got.precision, row.precision
            ));
        }
        _ => {}
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("\n"))
    }
}

/// The gate's verdict on one suite run: the row the run would record, the printed measure, the gate's result, and the
/// counts it judged.
struct Verdict {
    row: Row,
    report: String,
    result: Result<(), String>,
    /// The counts of the settings given, and of the same with `fallback: "none"`.
    defaults: Counts,
    no_fallback: Counts,
}

/// The gate on `suite` (named `name` in the baseline) with `settings` standing for the collection's default
/// settings: `settings` itself for checks (a) and (c), and `settings` with `fallback: "none"` for (b), (c) against
/// `baseline`. The gate runs it with [`RelaxOptions::default`] and the committed baseline; a test runs it with a
/// level left out to see the gate fail.
fn evaluate(name: &str, suite: &Suite, settings: &RelaxOptions, baseline: &[Row]) -> Verdict {
    let seed = suite.seed;
    let (defaults, _) = suite.measure(settings);
    let none = RelaxOptions {
        fallback: Some(Fallback::None),
        ..settings.clone()
    };
    let (no_fallback, _) = suite.measure(&none);
    let row = Row {
        suite: name.to_owned(),
        seed,
        queries: defaults.all.queries,
        precision: defaults
            .all
            .first_relaxed_precision()
            .map_or(0.0, |p| (10_000.0 * p).round() / 100.0),
    };
    let report = format!(
        "{}{}baseline row (tests/relax_quality/baseline.txt):\n{row}\n",
        print(seed, "the default settings", &defaults),
        print(seed, "`fallback: \"none\"`", &no_fallback),
    );
    let result = gate(baseline, &row, &defaults.all, &no_fallback.all);
    Verdict {
        row,
        report,
        result,
        defaults: defaults.all,
        no_fallback: no_fallback.all,
    }
}

/// The default levels without `left_out`.
fn levels_without(left_out: Level) -> RelaxOptions {
    RelaxOptions {
        levels: Some(Levels::Custom(
            Level::ALL.into_iter().filter(|l| *l != left_out).collect(),
        )),
        ..RelaxOptions::default()
    }
}

/// Runs the gate on the suite `name` over each of `seeds` with the default settings, printing each run's measure,
/// then every run's baseline row in one block (the rows to record when the measure changes on purpose), and fails
/// naming every run the gate failed. With `compare`, also prints (and does not gate) the levels without L2
/// `drop_terms`, the trade-off `docs/relaxation.md` states for typo-heavy input.
fn run_gate(name: &str, seeds: &[u64], options: &CorpusOptions, n: usize, compare: bool) {
    let baseline = rows(BASELINE).unwrap();
    let mut recorded = String::new();
    let mut failed = String::new();
    for &seed in seeds {
        let suite = Suite::new(seed, options, n);
        if compare {
            let (quality, _) = suite.measure(&levels_without(Level::DropTerms));
            eprintln!(
                "{}",
                print(seed, "levels without `drop_terms` (not gated)", &quality)
            );
        }
        let verdict = evaluate(name, &suite, &RelaxOptions::default(), &baseline);
        eprintln!("{}", verdict.report);
        writeln!(recorded, "{}", verdict.row).unwrap();
        if let Err(why) = verdict.result {
            writeln!(failed, "suite {name}, seed {seed}:\n{why}\n").unwrap();
        }
    }
    eprintln!("baseline rows of suite {name} (tests/relax_quality/baseline.txt):\n{recorded}");
    assert!(
        failed.is_empty(),
        "relaxation quality gate failed:\n{failed}\nEach run's measure is printed above. If the change is intended, \
         replace suite {name}'s rows in tests/relax_quality/baseline.txt with these and say in the PR why the \
         precision moved:\n{recorded}"
    );
}

#[test]
fn the_gate_checks_each_rule_on_counts_by_hand() {
    let baseline = rows("# comment\n\nfast 7 100 60.00\nslow 1 600 65.33\n").unwrap();
    let counts = |queries, zero_after| Counts {
        queries,
        zero_after,
        ..Counts::default()
    };
    let row = |seed, queries, precision| Row {
        suite: "fast".to_owned(),
        seed,
        queries,
        precision,
    };
    let ok = counts(100, 0);
    assert_eq!(gate(&baseline, &row(7, 100, 60.0), &ok, &ok), Ok(()));
    // Within 2 points below, and any rise, pass; past 2 points below fails (c).
    assert_eq!(gate(&baseline, &row(7, 100, 58.0), &ok, &ok), Ok(()));
    assert_eq!(gate(&baseline, &row(7, 100, 99.0), &ok, &ok), Ok(()));
    let low = gate(&baseline, &row(7, 100, 57.99), &ok, &ok).unwrap_err();
    assert!(
        low.starts_with("(c) first-relaxed-hit precision 57.99%"),
        "{low}"
    );
    // At every hundredth the baseline can hold, a precision exactly 2 points below passes and one hundredth more
    // fails: the values go through the text a baseline is written in, so float rounding (a subtraction crossing a
    // power of two) cannot move a run across the line.
    for base in 200..=10_000_u32 {
        let text = |h: u32| format!("{}.{:02}", h / 100, h % 100);
        let baseline = rows(&format!("fast 7 100 {}", text(base))).unwrap();
        let at = |h: u32| row(7, 100, text(h).parse().unwrap());
        assert_eq!(
            gate(&baseline, &at(base - 200), &ok, &ok),
            Ok(()),
            "baseline {}: 2 points below must pass",
            text(base)
        );
        if base > 200 {
            assert!(
                gate(&baseline, &at(base - 201), &ok, &ok).is_err(),
                "baseline {}: 2.01 points below must fail",
                text(base)
            );
        }
    }
    // Another query count is a changed suite.
    let changed = gate(&baseline, &row(7, 99, 60.0), &ok, &ok).unwrap_err();
    assert!(changed.contains("record the baseline again"), "{changed}");
    // A seed without a row skips (c).
    assert_eq!(gate(&baseline, &row(8, 100, 0.0), &ok, &ok), Ok(()));
    // (a): one empty query with the defaults fails; (b): 2% without the fallback passes, 3% fails.
    let a = gate(&baseline, &row(7, 100, 60.0), &counts(100, 1), &ok).unwrap_err();
    assert!(a.starts_with("(a) 1 of 100"), "{a}");
    assert_eq!(
        gate(&baseline, &row(7, 100, 60.0), &ok, &counts(100, 2)),
        Ok(())
    );
    let b = gate(&baseline, &row(7, 100, 60.0), &ok, &counts(100, 3)).unwrap_err();
    assert!(b.starts_with("(b) 3.00%"), "{b}");
    // Every failure is named.
    let all = gate(
        &baseline,
        &row(7, 100, 0.0),
        &counts(100, 1),
        &counts(100, 3),
    )
    .unwrap_err();
    assert_eq!(all.lines().count(), 3, "{all}");
    // Malformed rows are errors, not skipped.
    assert!(rows("fast 7 100").is_err());
    assert!(rows("fast seven 100 60.0").is_err());
    // The committed baseline parses, and holds the suites this file runs.
    let committed = rows(BASELINE).unwrap();
    assert!(
        committed
            .iter()
            .any(|r| r.suite == "fast" && r.seed == SEED)
    );
    for seed in SLOW_SEEDS {
        assert!(
            committed
                .iter()
                .any(|r| r.suite == "slow" && r.seed == seed),
            "no baseline row for slow seed {seed}"
        );
    }
}

/// The gate on the real suite (ROADMAP 8.4.3): it passes with the default settings and the committed baseline, and
/// fails when a level the suite exercises is left out, when the fallback goes too, and when the baseline is not met.
#[test]
fn the_gate_fails_on_the_suite_without_a_level_or_below_its_baseline() {
    let suite = Suite::new(SEED, &CorpusOptions::default(), 150);
    let committed = rows(BASELINE).unwrap();
    let ok = evaluate("fast", &suite, &RelaxOptions::default(), &committed);
    assert_eq!(ok.result, Ok(()), "{}", ok.report);
    // The measure is deterministic: measured again, and on a suite rebuilt from the seed, it is the same.
    let again = Suite::new(SEED, &CorpusOptions::default(), 150);
    let defaults = RelaxOptions::default();
    assert_eq!(suite.measure(&defaults).0, again.measure(&defaults).0);
    // L2 or L4 left out: the queries it answered fall to the fallback (c), or, without it, find nothing (b).
    for level in [Level::DropTerms, Level::Fuzzy] {
        let v = evaluate("fast", &suite, &levels_without(level), &committed);
        let err = v.result.unwrap_err();
        assert!(
            err.contains("(b) ") && err.contains("(c) first-relaxed-hit precision"),
            "without {level:?}: {err}\n{}",
            v.report
        );
    }
    // Both, and the fallback with them: queries are left with no hit at all (a).
    let bare = RelaxOptions {
        levels: Some(Levels::Custom(vec![Level::Synonyms, Level::Prefix])),
        fallback: Some(Fallback::None),
        ..RelaxOptions::default()
    };
    let v = evaluate("fast", &suite, &bare, &committed);
    assert!(
        v.result.as_ref().unwrap_err().starts_with("(a) "),
        "{:?}\n{}",
        v.result,
        v.report
    );
    // What the suite cannot see: it has no synonyms (L1), and L4 corrects every query L3 would complete, so leaving
    // either out measures the same. A broken L3 that matched the wrong terms would still move the precision. These
    // pin today's blind spots, not a property the gate wants: ROADMAP 8.4.6 adds the queries that see them, and turns
    // both assertions into failures (update them then).
    for level in [Level::Synonyms, Level::Prefix] {
        let v = evaluate("fast", &suite, &levels_without(level), &committed);
        assert_eq!(v.row, ok.row, "without {level:?}\n{}", v.report);
    }
    // No query reaches L5 with the default levels (L2's one-word rung always finds a hit), so (b) measures as (a):
    // with the default levels check (b) never fails today, only with a level left out (above). Also a blind spot that
    // 8.4.6's queries only L5 answers turn into a failure.
    assert_eq!(ok.no_fallback, ok.defaults);
    // The baseline not met: a row 2 points above the measure passes, 2.01 fails; another query count fails.
    let at = |precision: f64, queries| {
        vec![Row {
            suite: "fast".to_owned(),
            seed: SEED,
            queries,
            precision,
        }]
    };
    let judge = |baseline: &[Row]| gate(baseline, &ok.row, &ok.defaults, &ok.no_fallback);
    let p = ok.row.precision;
    assert_eq!(judge(&at(p + 2.0, 150)), Ok(()));
    let above = judge(&at(p + 2.01, 150)).unwrap_err();
    assert!(
        above.starts_with("(c) first-relaxed-hit precision"),
        "{above}"
    );
    assert!(
        judge(&at(p, 149))
            .unwrap_err()
            .contains("record the baseline again")
    );
    // (b)'s bound is on the counts: 3 of 150 is 2%, 4 is not.
    let no_hits = |zero_after| Counts {
        queries: 150,
        zero_after,
        ..Counts::default()
    };
    let none_at = |zero_after| gate(&committed, &ok.row, &ok.defaults, &no_hits(zero_after));
    assert_eq!(none_at(3), Ok(()));
    assert!(none_at(4).unwrap_err().starts_with("(b) 2.67%"));
}

/// The suite of every `cargo test` (seconds): 200 documents, 150 queries (ROADMAP 8.4.3).
#[test]
fn relaxation_quality_gate() {
    run_gate("fast", &[seed()], &CorpusOptions::default(), 150, false);
}

/// The seeds of the slow suite.
const SLOW_SEEDS: [u64; 5] = [1, 2, 3, 4, 5];

/// The larger suite (`--features slow`): 1 000 documents over 1 500 words, 600 queries, each of [`SLOW_SEEDS`].
#[cfg(feature = "slow")]
#[test]
fn relaxation_quality_gate_slow() {
    let options = CorpusOptions {
        docs: 1000,
        vocabulary: 1500,
        words: 5..=20,
    };
    run_gate("slow", &SLOW_SEEDS, &options, 600, true);
}
