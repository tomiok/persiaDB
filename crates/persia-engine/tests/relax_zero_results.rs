//! Never zero results by default (SPEC §7.6; ROADMAP 8.4.3): under `RelaxOptions::default()`, every synthetic typo
//! and term-drop query (`persia_testutil::synthetic`) finds at least one hit, and the engine's counters
//! (`RelaxationStats`, ROADMAP 8.3.3) count no query as empty.
//!
//! Covered, for each seed: corpus shapes (the default corpus; a tiny vocabulary in long documents; a large vocabulary
//! in short documents; three documents; a handful of short texts with digits and punctuation), query options (1, 1
//! to 3, and 4 to 5 intended words; typos only, drops only, both at once) and layouts (every document in the memtable;
//! committed in 3 segments; committed in 2 segments with the rest in the WAL, reopened from its `SimFile`).
//!
//! The budget clock never moves, so the budget never truncates a cascade: the assertion does not depend on the
//! machine's speed. Every failure names the shape, seed, layout, options, query and what the cascade stated.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, missing_docs)] // Tests: inputs are generated from fixed seeds.

use std::fmt::Write as _;
use std::ops::RangeInclusive;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, SearchRequest, SegmentIds, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::relax::{RelaxOptions, RelaxPlan};
use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::Projection;
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use persia_testutil::synthetic::{Corpus, CorpusOptions, QueryGenerator, QueryOptions};

const COLLECTION: &str = "docs";

#[cfg(not(feature = "slow"))]
const SEEDS: &[u64] = &[1, 2, 3];
#[cfg(feature = "slow")]
const SEEDS: &[u64] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

/// Queries drawn per (shape, seed, layout, words, weights).
const QUERIES: usize = 8;

const WORDS: [RangeInclusive<usize>; 3] = [1..=1, 1..=3, 4..=5];
const WEIGHTS: [[usize; 3]; 3] = [[1, 0, 0], [0, 1, 0], [0, 0, 1]];

/// Where the documents are when the queries run.
#[derive(Debug, Clone, Copy)]
enum Layout {
    /// Every document in the memtable, nothing committed.
    Memtable,
    /// Committed in 3 segments (fewer for fewer documents).
    Segments,
    /// Two thirds committed in 2 segments, the rest logged in the WAL (`Fsync`), then the database dropped and
    /// reopened from its file: the queries run on segments read back and a memtable rebuilt by replay.
    Reopened,
}

const LAYOUTS: [Layout; 3] = [Layout::Memtable, Layout::Segments, Layout::Reopened];

/// A corpus shape: a name for messages and how to build its corpus from a seed.
struct Shape {
    name: &'static str,
    corpus: fn(u64) -> Corpus,
}

fn synthetic(
    docs: usize,
    vocabulary: usize,
    words: RangeInclusive<usize>,
) -> impl Fn(u64) -> Corpus {
    move |seed| {
        Corpus::synthetic(
            seed,
            &CorpusOptions {
                docs,
                vocabulary,
                words: words.clone(),
            },
        )
    }
}

/// Short texts with digits, codes and punctuation between words (none inside a word, where UAX#29 joins more than
/// `Corpus::from_texts` splits); the same for every seed, which then only changes the queries.
fn texts(_seed: u64) -> Corpus {
    Corpus::from_texts([
        ("t0", "Order 4521 shipped to Lisbon, Portugal!"),
        ("t1", "Invoice #2024 0117: paid in full."),
        ("t2", "Red running shoes, size 42 (men)."),
        ("t3", "Kitchen table; oak wood, 180 cm long"),
        ("t4", "Flight IB6250 departs from gate B12"),
        ("t5", "Room 101 - blue door, second floor"),
        ("t6", "Call 555 0199 before noon"),
        ("t7", "Recipe: 3 eggs, 200 g flour, 1 cup milk"),
        ("t8", "Error E404 page not found?"),
        ("t9", "Version 2 release notes ... bug fixes & speedups"),
    ])
    .unwrap()
}

fn shapes() -> Vec<Shape> {
    let mut shapes = vec![
        Shape {
            name: "default (200 docs, 400 words, 5-20 per doc)",
            corpus: |seed| Corpus::synthetic(seed, &CorpusOptions::default()),
        },
        Shape {
            name: "tiny vocabulary (100 docs, 20 words, 30-60 per doc)",
            corpus: |seed| synthetic(100, 20, 30..=60)(seed),
        },
        Shape {
            name: "large vocabulary (200 docs, 5000 words, 3-5 per doc)",
            corpus: |seed| synthetic(200, 5_000, 3..=5)(seed),
        },
        Shape {
            name: "three documents (400 words, 5-20 per doc)",
            corpus: |seed| synthetic(3, 400, 5..=20)(seed),
        },
        Shape {
            name: "short texts with digits and punctuation",
            corpus: texts,
        },
    ];
    if cfg!(feature = "slow") {
        shapes.push(Shape {
            name: "large corpus (2000 docs, 400 words, 5-20 per doc)",
            corpus: |seed| synthetic(2_000, 400, 5..=20)(seed),
        });
    }
    shapes
}

fn options(seed: u64) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(seed),
        ..DatabaseOptions::default()
    }
}

/// `corpus` written in one `body` field (`standard`) of a new database on a `SimFile`, laid out as `layout`.
fn build(corpus: &Corpus, seed: u64, layout: Layout) -> Database<SimFile> {
    let schema = SchemaBuilder::new(COLLECTION)
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [9; 16], options(seed)).unwrap();
    db.create_collection(schema).unwrap();
    let docs = corpus.docs();
    let third = docs.len().div_ceil(3).max(1);
    for (i, doc) in docs.iter().enumerate() {
        let d = Document::new(doc.id.as_str()).with(body, Value::String(doc.text()));
        db.put(COLLECTION, &d, Durability::Fsync).unwrap();
        let end_of_third = (i + 1) % third == 0 || i + 1 == docs.len();
        let commit = match layout {
            Layout::Memtable => false,
            Layout::Segments => end_of_third,
            Layout::Reopened => end_of_third && i < 2 * third && i + 1 < docs.len(),
        };
        if commit {
            db.commit().unwrap();
        }
    }
    if let Layout::Reopened = layout {
        drop(db);
        // A seed of its own for the reopened writer's segment ids (SPEC §8.7.1: ids are never reused).
        db = Database::open_on(file, options(seed.wrapping_add(0x5EED))).unwrap();
    }
    let snapshot = db.snapshot();
    let c = snapshot.collection(COLLECTION).unwrap();
    let segments = c.segments().len();
    let expected = match layout {
        Layout::Memtable => 0,
        Layout::Segments => docs.len().min(3),
        Layout::Reopened => docs.len().saturating_sub(1).min(2),
    };
    assert_eq!(
        segments, expected,
        "seed {seed}, {layout:?}: the layout is not the one meant"
    );
    drop(snapshot);
    db
}

/// The failures of one database: each query of `queries` searched under `RelaxOptions::default()`, plus a check that
/// the engine's counters saw every query and none empty; each failure goes to `failures`, prefixed by `context`.
fn check(
    db: &Database<SimFile>,
    queries: &[persia_testutil::synthetic::SyntheticQuery],
    context: &str,
    failures: &mut Vec<String>,
) {
    let snapshot = db.snapshot();
    let c = snapshot.collection(COLLECTION).unwrap();
    let compiler = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin()).unwrap();
    let fields = DefaultFields::all(c.schema());
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let plan = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::default()).unwrap();
    let request = SearchRequest {
        limit: 1,
        projection: Projection::None,
        ..SearchRequest::default()
    };
    // A budget clock that never moves: the budget (SPEC §7.6) never truncates a cascade.
    let exec = Execution::default()
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)));
    let before = c.relaxation_stats();
    for q in queries {
        let text = compiler.compile(&q.text, &fields, &QueryLimits::default());
        let out = views.search_relaxed(&text, &request, &plan, &exec).unwrap();
        if out.hits.is_empty() {
            let r = &out.relaxation;
            failures.push(format!(
                "{context}: query {:?} ({}, query seed {}, intended {:?}) found no hit: level {:?}, applied {:?}, \
                 truncated {}",
                q.text, q.kind, q.seed, q.intended, r.level, r.applied, r.truncated
            ));
        }
    }
    let after = c.relaxation_stats();
    let (searched, empty) = (after.queries - before.queries, after.empty - before.empty);
    if searched != queries.len() as u64 || empty != 0 {
        failures.push(format!(
            "{context}: RelaxationStats counted {searched} queries ({} searched) and {empty} empty (0 expected)",
            queries.len()
        ));
    }
}

#[test]
fn default_relaxation_finds_a_hit_for_every_synthetic_query() {
    let mut failures = Vec::new();
    let mut summary = String::new();
    for shape in shapes() {
        for &seed in SEEDS {
            let corpus = (shape.corpus)(seed);
            // Each words/weights pair draws from a seed of its own, the same for every layout.
            let mut drawn = Vec::new();
            for (w, words) in WORDS.iter().enumerate() {
                for (k, weights) in WEIGHTS.iter().enumerate() {
                    let options = QueryOptions {
                        words: words.clone(),
                        weights: *weights,
                    };
                    let query_seed = seed
                        .wrapping_mul(1_000)
                        .wrapping_add((w * WEIGHTS.len() + k) as u64);
                    let queries =
                        QueryGenerator::new(&corpus, options).generate(query_seed, QUERIES);
                    drawn.push((words.clone(), *weights, queries));
                }
            }
            let total: usize = drawn.iter().map(|(_, _, q)| q.len()).sum();
            assert!(
                total > 0,
                "shape {}, seed {seed}: the generator gave no query at all",
                shape.name
            );
            writeln!(
                summary,
                "{}, seed {seed}: {total} queries per layout",
                shape.name
            )
            .unwrap();
            for layout in LAYOUTS {
                let db = build(&corpus, seed, layout);
                for (words, weights, queries) in &drawn {
                    let context = format!(
                        "shape {}, seed {seed}, {layout:?}, words {words:?}, weights {weights:?}",
                        shape.name
                    );
                    check(&db, queries, &context, &mut failures);
                }
            }
        }
    }
    println!("{summary}");
    assert!(
        failures.is_empty(),
        "{} failures under RelaxOptions::default() (SPEC §7.6: never zero results by default):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
