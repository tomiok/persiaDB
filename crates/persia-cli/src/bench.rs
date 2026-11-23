//! `persia bench` (ROADMAP 10.4.3): a quick micro-benchmark of this build on this machine.
//!
//! **The numbers are local guidance only, not a published benchmark.** They depend on the machine, its load, the
//! build profile and the file system under `--dir`, and one run is one sample. Reproducible numbers, with their
//! method, live in `docs/benchmarks.md` (SPEC §16.3); nothing this command prints belongs in a claim.
//!
//! What a run does:
//! 1. Generates a synthetic corpus from `--seed` (module `corpus`): `--docs` documents with a `title` and a `body`
//!    (`text`), a `category` (`keyword`) and a `price` (`i64`).
//! 2. Creates a scratch directory under `--dir` (default: the system temporary directory; module `workdir`) and a new
//!    database in it. It never opens an existing database, and the directory is removed when the run ends, also when
//!    it fails.
//! 3. Indexes the corpus in batches (`Buffered` writes), then commits once: the commit flushes the memtable to a
//!    segment and syncs. A memtable that fills up earlier commits on its own, inside the indexing time.
//! 4. Runs `--queries` queries from the same seed, of five kinds in rotation (`term`, `terms`, `phrase`, `filter`,
//!    `typo`), each compiled from user text and searched with the collection's default relaxation (SPEC §7.6), as a
//!    default search is, for the first page of 20 hits. A latency covers the compilation and the search.
//!
//! The same seed gives the same corpus and the same queries: the report carries a fingerprint of each, and the hits
//! each kind found, so two runs can be seen to have done the same work. Timings are never the same twice.
//!
//! `--json` writes one object on one line, with these keys and no others (times in nanoseconds):
//!
//! ```text
//! {
//!   "notice":  the sentence that these numbers are local guidance only,
//!   "build":   "release" or "debug" (an unoptimized build is several times slower),
//!   "seed": .., "docs": .., "queries": ..,
//!   "corpus":   {"fingerprint": 16 hex digits, "container_bytes": the database's size after the commit},
//!   "indexing": {"nanos": .., "docs_per_second": .. or null, "commit_nanos": ..},
//!   "query_mix": {"fingerprint": 16 hex digits},
//!   "query_kinds": [ in rotation order
//!     {"kind": "term", "count": .., "hits": .., "latency_nanos": {"p50": .., "p90": .., "p99": .., "max": ..} or null}
//!   ]
//! }
//! ```
//!
//! Percentiles are nearest-rank (module `percentile`); `latency_nanos` is `null` for a kind that ran no query, and
//! `docs_per_second` for an indexing time the clock read as zero.

mod corpus;
mod percentile;
mod workdir;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use persia_engine::analysis::AnalyzerRegistry;
use persia_engine::db::{Database, DatabaseOptions, Execution, SearchRequest};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, Filter, FilterValue, QueryCompiler, QueryLimits};
use persia_engine::relax::{RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::wal::Durability;
use serde_json::{Value as Json, json};

use corpus::{Fingerprint, QueryKind, SyntheticDoc, SyntheticQuery};
use percentile::Percentiles;
use workdir::WorkDir;

/// What every report says first (the module docs; CLAUDE.md pillar 5).
pub const NOTICE: &str = "local guidance only, not a published benchmark: these numbers depend on this machine, its \
                          load and this build (reproducible numbers: docs/benchmarks.md)";

/// `--docs` when not given: small, so the default run takes a few seconds.
pub const DEFAULT_DOCS: u32 = 20_000;
/// `--queries` when not given.
pub const DEFAULT_QUERIES: u32 = 1000;
/// `--seed` when not given.
pub const DEFAULT_SEED: u64 = 42;

/// How this binary was built, as the report names it: a debug build's numbers say little about a release build's.
const fn build() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// The collection the corpus is indexed into.
const COLLECTION: &str = "bench";
/// The database file in the scratch directory.
const DATABASE: &str = "bench.persia";
/// Documents per `put_batch`.
const BATCH: usize = 256;

/// The arguments of `persia bench`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Where the scratch directory is created; the system temporary directory when `None`.
    pub dir: Option<PathBuf>,
    /// Documents in the corpus.
    pub docs: u32,
    /// Queries in the mix.
    pub queries: u32,
    /// The seed of both.
    pub seed: u64,
    /// One JSON object instead of text.
    pub json: bool,
}

/// What a run measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    seed: u64,
    docs: u32,
    queries: u32,
    corpus: Fingerprint,
    container_bytes: u64,
    indexing: Duration,
    commit: Duration,
    query_mix: Fingerprint,
    kinds: Vec<KindReport>,
}

/// One kind of query in a [`Report`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct KindReport {
    kind: QueryKind,
    count: u64,
    /// Hits returned, over the kind's queries (at most 20 each): the same for the same seed and sizes.
    hits: u64,
    latency: Option<Percentiles>,
}

/// Runs the benchmark `options` describes and writes its report to `out`.
pub fn bench(out: &mut dyn Write, options: &Options) -> Result<()> {
    let base = options.dir.clone().unwrap_or_else(std::env::temp_dir);
    let report = run_in(&base, options)?;
    if options.json {
        writeln!(out, "{}", report.to_json())?;
    } else {
        report.write_text(out)?;
    }
    Ok(())
}

/// One run in a new scratch directory under `base`, removed before this returns.
pub fn run_in(base: &Path, options: &Options) -> Result<Report> {
    in_scratch(base, |dir| run(&dir.join(DATABASE), options))
}

/// Runs `f` on a new scratch directory under `base` and removes the directory with everything `f` left in it: after
/// a success, where a failed removal is the error, and after a failure, where `f`'s error is (the removal then only
/// warns, module `workdir`).
fn in_scratch<T>(base: &Path, f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let work = WorkDir::create_in(base)?;
    let done = f(work.path())?;
    work.remove()?;
    Ok(done)
}

/// Indexes the corpus into a new database at `path` and runs the query mix on it.
fn run(path: &Path, options: &Options) -> Result<Report> {
    let schema = schema()?;
    let fields = Fields::of(&schema)?;
    let mut db = Database::create(path, DatabaseOptions::default())
        .with_context(|| format!("creating {}", path.display()))?;
    db.create_collection(schema)
        .context("creating the collection")?;

    let docs = u64::from(options.docs);
    let mut corpus = Fingerprint::default();
    let mut indexing = Duration::ZERO;
    let batch_len = u64::try_from(BATCH).unwrap_or(u64::MAX);
    for start in (0..docs).step_by(BATCH) {
        let end = start.saturating_add(batch_len).min(docs);
        // Generating the batch is not indexing: only the puts are timed.
        let batch: Vec<Document> = (start..end)
            .map(|index| {
                let doc = corpus::document(options.seed, index);
                corpus.document(&doc);
                fields.document(&doc)
            })
            .collect();
        let started = Instant::now();
        db.put_batch(COLLECTION, &batch, Durability::Buffered)
            .context("indexing a batch")?;
        indexing = indexing.saturating_add(started.elapsed());
    }
    let started = Instant::now();
    db.commit().context("committing")?;
    let commit = started.elapsed();
    let container_bytes = db.container().end();

    let (query_mix, kinds) = search(&db, &fields, options)?;
    Ok(Report {
        seed: options.seed,
        docs: options.docs,
        queries: options.queries,
        corpus,
        container_bytes,
        indexing,
        commit,
        query_mix,
        kinds,
    })
}

/// Runs the query mix on the committed corpus: the mix's fingerprint and each kind's count, hits and latencies.
fn search(
    db: &Database,
    fields: &Fields,
    options: &Options,
) -> Result<(Fingerprint, Vec<KindReport>)> {
    let snapshot = db.snapshot();
    let collection = snapshot
        .collection(COLLECTION)
        .ok_or_else(|| anyhow!("the bench collection is missing from its snapshot"))?;
    let schema = collection.schema();
    let memtable = collection.memtable();
    let views = collection.views(&memtable).context("opening the views")?;
    let compiler = QueryCompiler::new(schema, &AnalyzerRegistry::builtin())
        .context("building the query compiler")?;
    let defaults = DefaultFields::all(schema);
    let limits = QueryLimits::default();
    let relax = RelaxPlan::new(schema.relaxation(), &RelaxOptions::default())
        .context("planning the relaxation")?;
    let request = SearchRequest::default();
    let exec = Execution::default();

    let mut mix = Fingerprint::default();
    let mut samples: Vec<(QueryKind, Vec<u64>, u64)> = QueryKind::ALL
        .iter()
        .map(|kind| (*kind, Vec::new(), 0))
        .collect();
    for index in 0..u64::from(options.queries) {
        let query = corpus::query(options.seed, index, u64::from(options.docs));
        mix.query(&query);
        let started = Instant::now();
        let text = compiler.compile(&query.text, &defaults, &limits);
        let searched = match fields.filter(&query) {
            Some(filter) => text.with_filter([filter]),
            None => text,
        };
        let results = views
            .search_relaxed(&searched, &request, &relax, &exec)
            .with_context(|| format!("searching {:?}", query.text))?;
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if let Some((_, latencies, hits)) =
            samples.iter_mut().find(|(kind, ..)| *kind == query.kind)
        {
            latencies.push(nanos);
            *hits = hits.saturating_add(u64::try_from(results.hits.len()).unwrap_or(u64::MAX));
        }
    }
    let kinds = samples
        .into_iter()
        .map(|(kind, latencies, hits)| KindReport {
            kind,
            count: u64::try_from(latencies.len()).unwrap_or(u64::MAX),
            hits,
            latency: Percentiles::of(&latencies),
        })
        .collect();
    Ok((mix, kinds))
}

/// The schema of the bench collection. `body` indexes positions, so a phrase is a phrase there (SPEC §7.1.1).
fn schema() -> Result<CollectionSchema> {
    let body = TextOptions {
        positions: true,
        ..TextOptions::default()
    };
    SchemaBuilder::new(COLLECTION)
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("body", FieldKind::Text(body))
        .field("category", FieldKind::Keyword(KeywordOptions::default()))
        .field("price", FieldKind::I64 { stored: true })
        .build()
        .context("building the bench schema")
}

/// The field ids of [`schema`].
#[derive(Debug, Clone, Copy)]
struct Fields {
    title: FieldId,
    body: FieldId,
    category: FieldId,
    price: FieldId,
}

impl Fields {
    fn of(schema: &CollectionSchema) -> Result<Self> {
        let id = |name: &str| {
            schema
                .field(name)
                .map(persia_engine::schema::Field::id)
                .ok_or_else(|| anyhow!("the bench schema lacks the field {name:?}"))
        };
        Ok(Self {
            title: id("title")?,
            body: id("body")?,
            category: id("category")?,
            price: id("price")?,
        })
    }

    fn document(&self, doc: &SyntheticDoc) -> Document {
        Document::new(doc.id.clone())
            .with(self.title, Value::String(doc.title.join(" ")))
            .with(self.body, Value::String(doc.body.join(" ")))
            .with(self.category, Value::String(doc.category.clone()))
            .with(self.price, Value::I64(doc.price))
    }

    /// The filter of a `filter` query: its category.
    fn filter(&self, query: &SyntheticQuery) -> Option<Filter> {
        query.category.as_ref().map(|category| Filter::Term {
            field: self.category,
            value: FilterValue::Keyword(category.clone()),
        })
    }
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// `docs` per second over `nanos`; `None` when the clock read no time at all.
fn per_second(docs: u32, nanos: u64) -> Option<u64> {
    let rate = u128::from(docs)
        .checked_mul(1_000_000_000)?
        .checked_div(u128::from(nanos))?;
    Some(u64::try_from(rate).unwrap_or(u64::MAX))
}

/// `nanos` for people: three significant places in the largest unit that keeps it at or above 1.
fn human(nanos: u64) -> String {
    let (unit, per) = match nanos {
        0..1_000 => return format!("{nanos} ns"),
        1_000..1_000_000 => ("µs", 1_000),
        1_000_000..1_000_000_000 => ("ms", 1_000_000),
        _ => ("s", 1_000_000_000),
    };
    let whole = nanos.checked_div(per).unwrap_or(0);
    let thousandths = nanos
        .checked_rem(per)
        .unwrap_or(0)
        .saturating_mul(1000)
        .checked_div(per)
        .unwrap_or(0);
    format!("{whole}.{thousandths:03} {unit}")
}

impl Report {
    /// The `--json` object (the module docs give its shape).
    pub fn to_json(&self) -> Json {
        let indexing = nanos(self.indexing);
        json!({
            "notice": NOTICE,
            "build": build(),
            "seed": self.seed,
            "docs": self.docs,
            "queries": self.queries,
            "corpus": {"fingerprint": self.corpus.hex(), "container_bytes": self.container_bytes},
            "indexing": {
                "nanos": indexing,
                "docs_per_second": per_second(self.docs, indexing),
                "commit_nanos": nanos(self.commit),
            },
            "query_mix": {"fingerprint": self.query_mix.hex()},
            "query_kinds": self.kinds.iter().map(KindReport::to_json).collect::<Vec<_>>(),
        })
    }

    /// The text report.
    pub fn write_text(&self, out: &mut dyn Write) -> Result<()> {
        let indexing = nanos(self.indexing);
        let rate = per_second(self.docs, indexing)
            .map_or_else(|| "n/a".to_owned(), |rate| format!("{rate} docs/s"));
        let rows = [
            ("notice", NOTICE.to_owned()),
            ("build", build().to_owned()),
            ("seed", self.seed.to_string()),
            (
                "corpus",
                format!(
                    "{} documents, fingerprint {}, {} bytes on disk",
                    self.docs,
                    self.corpus.hex(),
                    self.container_bytes
                ),
            ),
            ("indexing", format!("{rate} ({})", human(indexing))),
            ("commit", human(nanos(self.commit))),
            (
                "queries",
                format!("{}, fingerprint {}", self.queries, self.query_mix.hex()),
            ),
        ];
        for (key, value) in rows {
            writeln!(out, "{key:<18}{value}")?;
        }
        writeln!(
            out,
            "\n{:<8} {:>8} {:>10} {:>12} {:>12} {:>12} {:>12}",
            "kind", "count", "hits", "p50", "p90", "p99", "max"
        )?;
        for kind in &self.kinds {
            let cell = |pick: fn(&Percentiles) -> u64| {
                kind.latency
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |l| human(pick(l)))
            };
            writeln!(
                out,
                "{:<8} {:>8} {:>10} {:>12} {:>12} {:>12} {:>12}",
                kind.kind.name(),
                kind.count,
                kind.hits,
                cell(|l| l.p50),
                cell(|l| l.p90),
                cell(|l| l.p99),
                cell(|l| l.max)
            )?;
        }
        Ok(())
    }
}

impl KindReport {
    fn to_json(&self) -> Json {
        json!({
            "kind": self.kind.name(),
            "count": self.count,
            "hits": self.hits,
            "latency_nanos": self.latency.map(|l| json!({"p50": l.p50, "p90": l.p90, "p99": l.p99, "max": l.max})),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(docs: u32, queries: u32, seed: u64) -> Options {
        Options {
            dir: None,
            docs,
            queries,
            seed,
            json: false,
        }
    }

    fn sample() -> Report {
        let mut corpus = Fingerprint::default();
        corpus.document(&corpus::document(1, 0));
        let mut query_mix = Fingerprint::default();
        query_mix.query(&corpus::query(1, 0, 1));
        Report {
            seed: 42,
            docs: 3000,
            queries: 7,
            corpus,
            container_bytes: 123_456,
            indexing: Duration::from_millis(1500),
            commit: Duration::from_micros(20_250),
            query_mix,
            kinds: vec![
                KindReport {
                    kind: QueryKind::Term,
                    count: 4,
                    hits: 61,
                    latency: Some(Percentiles {
                        p50: 12_345,
                        p90: 999,
                        p99: 1_500_000,
                        max: 2_000_000_000,
                    }),
                },
                KindReport {
                    kind: QueryKind::Typo,
                    count: 0,
                    hits: 0,
                    latency: None,
                },
            ],
        }
    }

    /// The whole object, key by key: the shape scripts read (the module docs). `serde_json` writes an object's
    /// keys in ascending order.
    #[test]
    fn the_json_object_has_its_documented_shape() {
        let report = sample();
        let expected = format!(
            concat!(
                r#"{{"build":"{build}","corpus":{{"container_bytes":123456,"fingerprint":"{corpus}"}},"docs":3000,"#,
                r#""indexing":{{"commit_nanos":20250000,"docs_per_second":2000,"nanos":1500000000}},"#,
                r#""notice":"{notice}","queries":7,"#,
                r#""query_kinds":[{{"count":4,"hits":61,"kind":"term","#,
                r#""latency_nanos":{{"max":2000000000,"p50":12345,"p90":999,"p99":1500000}}}},"#,
                r#"{{"count":0,"hits":0,"kind":"typo","latency_nanos":null}}],"#,
                r#""query_mix":{{"fingerprint":"{mix}"}},"seed":42}}"#,
            ),
            corpus = report.corpus.hex(),
            mix = report.query_mix.hex(),
            notice = NOTICE,
            build = build(),
        );
        assert_eq!(report.to_json().to_string(), expected);
    }

    #[test]
    fn the_text_report_says_first_that_the_numbers_are_local_guidance() {
        let report = sample();
        let mut out = Vec::new();
        report.write_text(&mut out).unwrap();
        let expected = format!(
            "\
notice            {NOTICE}
build             {}
seed              42
corpus            3000 documents, fingerprint {}, 123456 bytes on disk
indexing          2000 docs/s (1.500 s)
commit            20.250 ms
queries           7, fingerprint {}

kind        count       hits          p50          p90          p99          max
term            4         61    12.345 µs       999 ns     1.500 ms      2.000 s
typo            0          0            -            -            -            -
",
            build(),
            report.corpus.hex(),
            report.query_mix.hex()
        );
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    /// A phrase query is a phrase only in a field that indexes positions (SPEC §7.1.1): `body` does, so the
    /// `phrase` kind measures phrases.
    #[test]
    fn the_bench_schema_indexes_the_positions_of_body() {
        let schema = schema().unwrap();
        let positions = |name: &str| match schema.field(name).unwrap().kind() {
            FieldKind::Text(options) => options.positions,
            other => panic!("{name} is {other:?}"),
        };
        assert!(positions("body"));
        assert!(!positions("title"));
        let names: Vec<&str> = schema
            .fields()
            .iter()
            .map(persia_engine::schema::Field::name)
            .collect();
        assert_eq!(names, ["body", "category", "price", "title"]);
    }

    /// A `filter` query filters by its category, on the `category` field; the other kinds have no filter.
    #[test]
    fn a_filter_query_filters_by_its_category() {
        let schema = schema().unwrap();
        let fields = Fields::of(&schema).unwrap();
        let filtered = corpus::query(42, 3, 100);
        assert_eq!(filtered.kind, QueryKind::Filter);
        assert_eq!(
            fields.filter(&filtered),
            Some(Filter::Term {
                field: schema.field("category").unwrap().id(),
                value: FilterValue::Keyword("cat-02".into()),
            })
        );
        assert_eq!(fields.filter(&corpus::query(42, 0, 100)), None);
    }

    #[test]
    fn the_notice_says_what_the_numbers_are_not() {
        assert!(NOTICE.starts_with("local guidance only, not a published benchmark"));
        assert!(NOTICE.contains("docs/benchmarks.md"));
    }

    #[test]
    fn a_rate_is_whole_documents_per_second() {
        assert_eq!(per_second(3000, 1_500_000_000), Some(2000));
        assert_eq!(per_second(1, 3_000_000_000), Some(0));
        assert_eq!(per_second(7, 1), Some(7_000_000_000));
        assert_eq!(per_second(u32::MAX, 1), Some(4_294_967_295_000_000_000));
        // A clock too coarse to see the indexing: no rate, not a division by zero.
        assert_eq!(per_second(3000, 0), None);
    }

    #[test]
    fn durations_read_in_their_largest_unit() {
        assert_eq!(human(0), "0 ns");
        assert_eq!(human(999), "999 ns");
        assert_eq!(human(1_000), "1.000 µs");
        assert_eq!(human(1_999), "1.999 µs");
        assert_eq!(human(999_999), "999.999 µs");
        assert_eq!(human(1_000_000), "1.000 ms");
        assert_eq!(human(20_250_000), "20.250 ms");
        assert_eq!(human(999_999_999), "999.999 ms");
        assert_eq!(human(1_000_000_000), "1.000 s");
        assert_eq!(human(61_005_000_000), "61.005 s");
        assert_eq!(human(u64::MAX), "18446744073.709 s");
        assert_eq!(nanos(Duration::from_micros(3)), 3000);
        assert_eq!(nanos(Duration::MAX), u64::MAX);
    }

    /// The same seed and sizes: the same corpus, the same queries and the same hits, whatever the timings. Another
    /// seed, or another size, is other work.
    #[test]
    fn the_same_seed_does_the_same_work() {
        let base = tempfile::tempdir().unwrap();
        let work = |report: &Report| {
            let kinds: Vec<_> = report
                .kinds
                .iter()
                .map(|k| (k.kind, k.count, k.hits))
                .collect();
            (
                report.corpus,
                report.query_mix,
                report.container_bytes,
                kinds,
            )
        };
        let first = run_in(base.path(), &options(300, 60, 42)).unwrap();
        let again = run_in(base.path(), &options(300, 60, 42)).unwrap();
        assert_eq!(work(&first), work(&again));
        let other_seed = run_in(base.path(), &options(300, 60, 43)).unwrap();
        assert_ne!(first.corpus, other_seed.corpus);
        assert_ne!(first.query_mix, other_seed.query_mix);
        let more_docs = run_in(base.path(), &options(301, 60, 42)).unwrap();
        assert_ne!(first.corpus, more_docs.corpus);
        let more_queries = run_in(base.path(), &options(300, 61, 42)).unwrap();
        assert_eq!(first.corpus, more_queries.corpus);
        assert_ne!(first.query_mix, more_queries.query_mix);
        // Every run removed its scratch directory.
        assert_eq!(std::fs::read_dir(base.path()).unwrap().count(), 0);
    }

    /// What was measured is the corpus that was indexed: every document is there, and every kind of query ran and
    /// found documents (each is drawn from the corpus; a `typo` finds its word through relaxation).
    #[test]
    fn a_run_indexes_every_document_and_every_kind_finds_hits() {
        let base = tempfile::tempdir().unwrap();
        let work = WorkDir::create_in(base.path()).unwrap();
        let path = work.path().join(DATABASE);
        let report = run(&path, &options(600, 50, 7)).unwrap();
        assert_eq!((report.seed, report.docs, report.queries), (7, 600, 50));
        assert_eq!(
            report.container_bytes,
            std::fs::metadata(&path).unwrap().len()
        );
        let mut corpus = Fingerprint::default();
        for index in 0..600 {
            corpus.document(&corpus::document(7, index));
        }
        assert_eq!(report.corpus, corpus);
        let mut mix = Fingerprint::default();
        for index in 0..50 {
            mix.query(&corpus::query(7, index, 600));
        }
        assert_eq!(report.query_mix, mix);
        let kinds: Vec<_> = report.kinds.iter().map(|k| (k.kind, k.count)).collect();
        assert_eq!(kinds, QueryKind::ALL.map(|kind| (kind, 10)));
        for kind in &report.kinds {
            assert!(kind.hits >= kind.count, "{kind:?}");
            assert!(kind.hits <= kind.count * 20, "{kind:?}");
            let latency = kind.latency.unwrap();
            assert!(
                latency.p50 <= latency.p90
                    && latency.p90 <= latency.p99
                    && latency.p99 <= latency.max
            );
        }

        // The database it left (before the scratch directory goes): 600 documents, committed, as generated.
        let db = Database::open(&path, DatabaseOptions::default()).unwrap();
        let snapshot = db.snapshot();
        let collection = snapshot.collection(COLLECTION).unwrap();
        assert_eq!(collection.memtable().live_count(), 0);
        let stored: u64 = collection
            .segments()
            .iter()
            .map(|s| u64::from(s.doc_count()))
            .sum();
        assert_eq!(stored, 600);
        let fields = Fields::of(collection.schema()).unwrap();
        for index in [0, 255, 256, 599] {
            let doc = corpus::document(7, index);
            assert_eq!(
                collection.get(&doc.id).unwrap(),
                Some(fields.document(&doc))
            );
        }
        assert_eq!(collection.get("doc-00000600").unwrap(), None);
    }

    /// A mix shorter than the rotation leaves kinds without a query: they are reported with no latency.
    #[test]
    fn a_kind_without_a_query_has_no_latency() {
        let base = tempfile::tempdir().unwrap();
        let report = run_in(base.path(), &options(50, 2, 1)).unwrap();
        let found: Vec<_> = report
            .kinds
            .iter()
            .map(|k| (k.kind.name(), k.count, k.latency.is_some()))
            .collect();
        assert_eq!(
            found,
            [
                ("term", 1, true),
                ("terms", 1, true),
                ("phrase", 0, false),
                ("filter", 0, false),
                ("typo", 0, false)
            ]
        );
    }

    /// A run that fails part-way still removes its scratch directory and what it wrote there, returns its own
    /// error, and never touches what the base directory already held.
    #[test]
    fn a_failed_run_removes_its_scratch_directory() {
        let base = tempfile::tempdir().unwrap();
        let existing = base.path().join(DATABASE);
        std::fs::write(&existing, b"a user's database").unwrap();
        let mut seen = None;
        let err = in_scratch(base.path(), |dir| -> Result<()> {
            std::fs::write(dir.join(DATABASE), b"half a database")?;
            seen = Some(dir.to_path_buf());
            Err(anyhow!("the disk filled up"))
        })
        .unwrap_err();
        assert_eq!(format!("{err:#}"), "the disk filled up");
        let dir = seen.unwrap();
        assert_eq!(dir.parent(), Some(base.path()));
        assert!(!dir.exists());
        assert_eq!(std::fs::read(&existing).unwrap(), b"a user's database");
        assert_eq!(std::fs::read_dir(base.path()).unwrap().count(), 1);

        // A base that is not a directory fails before anything is created.
        let err = run_in(&existing, &options(10, 5, 1)).unwrap_err();
        assert!(format!("{err:#}").contains("is not a directory"), "{err:#}");
        assert_eq!(std::fs::read(&existing).unwrap(), b"a user's database");

        // A successful run beside a database of the same name leaves it alone too.
        run_in(base.path(), &options(20, 5, 1)).unwrap();
        assert_eq!(std::fs::read(&existing).unwrap(), b"a user's database");
        assert_eq!(std::fs::read_dir(base.path()).unwrap().count(), 1);
    }
}
