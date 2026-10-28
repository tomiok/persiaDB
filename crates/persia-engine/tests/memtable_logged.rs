//! The stage / log / publish window of logged memtable writes (ROADMAP 6.3.9; SPEC §8.2.1 "What may be logged",
//! §8.4).
//!
//! Invariants, for any interleaving of `put_logged` and `delete_logged` calls whose log closure fails or succeeds:
//! 1. After every call, a fresh view equals the view of a memtable that saw only the operations whose log
//!    succeeded: the same docids, liveness, documents, postings (docids, frequencies, positions) of every term,
//!    norms, field statistics, id lookups, live and document counts, and version.
//! 2. A view taken inside the log closure (through a `MemtableReader` taken before the call) equals the view before
//!    the call: it never sees the staged document, its new terms, its postings, its statistics or its id; nor a
//!    staged delete. It still equals it after the call returns, whatever the outcome (MVCC).
//!
//! Seeds: `PROPTEST_RNG_SEED` (default fixed below) and `PROPTEST_CASES`; a failure prints the seed it ran with.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::cell::RefCell;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::memtable::{Logged, Memtable, MemtableView};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::view::{NormReader, PostingsIterator, SearchView};
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestCaseError, TestRunner};

const WORDS: [&str; 5] = ["alpha", "beta", "gamma", "common", "omega"];
/// Words only failed operations use: a term no successful operation adds.
const GHOSTS: [&str; 3] = ["ghost", "phantom", "spectre"];
const TAGS: [&str; 3] = ["x", "y", "ghosttag"];
const IDS: u8 = 6;

const PROPTEST_SEED: u64 = 0x0DB6_3900_0002;

struct Fields {
    body: FieldId,
    tag: FieldId,
    n: FieldId,
}

fn schema() -> (CollectionSchema, Fields) {
    let schema = SchemaBuilder::new("c")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .field("n", FieldKind::I64 { stored: true })
        .build()
        .unwrap();
    let id = |name: &str| schema.field(name).unwrap().id();
    let fields = Fields {
        body: id("body"),
        tag: id("tag"),
        n: id("n"),
    };
    (schema, fields)
}

#[derive(Debug, Clone)]
enum Op {
    /// A put of id `d{id}` with body words (indexes into `WORDS`, or `GHOSTS` past its end) and a tag.
    Put {
        id: u8,
        words: Vec<u8>,
        tag: u8,
        n: i64,
        log_ok: bool,
    },
    Delete {
        id: u8,
        log_ok: bool,
    },
    /// An id the memtable rejects before logging (empty): the log must not run.
    BadDelete,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..IDS, prop::collection::vec(0..(WORDS.len() + GHOSTS.len()) as u8, 0..6), 0..TAGS.len() as u8,
              any::<i64>(), any::<bool>())
            .prop_map(|(id, words, tag, n, log_ok)| Op::Put { id, words, tag, n, log_ok }),
        2 => (0..IDS, any::<bool>()).prop_map(|(id, log_ok)| Op::Delete { id, log_ok }),
        1 => Just(Op::BadDelete),
    ]
}

fn document(f: &Fields, id: u8, words: &[u8], tag: u8, n: i64, log_ok: bool) -> Document {
    // A successful put uses only `WORDS` and the first two tags, so ghost terms appear only staged.
    let word = |w: u8| {
        let w = w as usize;
        if w < WORDS.len() {
            WORDS[w]
        } else if log_ok {
            WORDS[w % WORDS.len()]
        } else {
            GHOSTS[w - WORDS.len()]
        }
    };
    let tag = if log_ok {
        TAGS[tag as usize % 2]
    } else {
        TAGS[tag as usize]
    };
    let mut doc = Document::new(format!("d{id}"))
        .with(f.n, Value::I64(n))
        .with(f.tag, Value::String(tag.into()));
    if !words.is_empty() {
        let text: Vec<&str> = words.iter().map(|&w| word(w)).collect();
        doc = doc.with(f.body, Value::String(text.join(" ")));
    }
    doc
}

type Postings = Vec<(u32, u32, Vec<u32>)>;

fn postings(view: &MemtableView, field: FieldId, term: &[u8]) -> Option<Postings> {
    let mut p = view.postings(field, term).unwrap()?;
    let mut out = Vec::new();
    while let Some(d) = p.next().unwrap() {
        out.push((d, p.freq().unwrap(), p.positions().unwrap().to_vec()));
    }
    Some(out)
}

/// Everything a reader can observe of `view`, as a comparable value.
#[derive(Debug, PartialEq)]
struct Observed {
    version: u64,
    doc_count: u32,
    live_count: u32,
    live: Vec<bool>,
    documents: Vec<Document>,
    norms: Vec<u8>,
    stats: Vec<String>,
    terms: Vec<(String, Option<Postings>)>,
    ids: Vec<String>,
}

fn observe(f: &Fields, view: &MemtableView) -> Observed {
    let doc_count = view.doc_count();
    let live: Vec<bool> = (0..doc_count).map(|d| view.is_live(d)).collect();
    let documents = (0..doc_count).map(|d| view.document(d).unwrap()).collect();
    let mut norms = view.norms(f.body).unwrap();
    let norms = (0..doc_count).map(|d| norms.get(d).unwrap()).collect();
    let stats = [f.body, f.tag, f.n]
        .iter()
        .map(|&field| format!("{:?}", view.field_stats(field)))
        .collect();
    let mut terms = Vec::new();
    for w in WORDS.iter().chain(&GHOSTS) {
        terms.push(((*w).to_owned(), postings(view, f.body, w.as_bytes())));
    }
    for t in TAGS {
        terms.push((format!("tag:{t}"), postings(view, f.tag, t.as_bytes())));
    }
    let ids = (0..IDS)
        .map(|i| {
            let id = format!("d{i}");
            format!("{:?} {:?}", view.lookup(&id), view.docid_of(&id).unwrap())
        })
        .collect();
    Observed {
        version: view.version(),
        doc_count,
        live_count: view.live_count(),
        live,
        documents,
        norms,
        stats,
        terms,
        ids,
    }
}

fn window(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let (schema, f) = schema();
    let registry = AnalyzerRegistry::builtin();
    let mut mem = Memtable::new(&schema, &registry).unwrap();
    // Applies only the operations whose log succeeded, with the infallible `put` / `delete`.
    let mut reference = Memtable::new(&schema, &registry).unwrap();
    let reader = mem.reader();
    let mut log = Vec::new();
    for (step, op) in ops.iter().enumerate() {
        let before = observe(&f, &reference.view());
        prop_assert_eq!(
            &observe(&f, &mem.view()),
            &before,
            "step {}: before {:?}",
            step,
            op
        );
        // The view inside the closure, kept past the call.
        let inside: RefCell<Option<MemtableView>> = RefCell::new(None);
        let peek = || {
            let view = reader.view();
            *inside.borrow_mut() = Some(view);
        };
        match op {
            Op::Put {
                id,
                words,
                tag,
                n,
                log_ok,
            } => {
                let doc = document(&f, *id, words, *tag, *n, *log_ok);
                let result = mem.put_logged(&doc, |bytes| {
                    peek();
                    log.push(bytes.to_vec());
                    if *log_ok { Ok(()) } else { Err("disk") }
                });
                if *log_ok {
                    let expected = reference.put(&doc).unwrap();
                    prop_assert_eq!(result, Ok(expected), "step {}", step);
                    prop_assert_eq!(
                        &Document::decode(log.last().unwrap(), &schema).unwrap(),
                        &doc
                    );
                } else {
                    prop_assert_eq!(result, Err(Logged::Log("disk")), "step {}", step);
                }
            }
            Op::Delete { id, log_ok } => {
                let id = format!("d{id}");
                let result = mem.delete_logged(&id, || {
                    peek();
                    if *log_ok { Ok(()) } else { Err("disk") }
                });
                if *log_ok {
                    let expected = reference.delete(&id).unwrap();
                    prop_assert_eq!(result, Ok(expected), "step {}", step);
                } else {
                    prop_assert_eq!(result, Err(Logged::Log("disk")), "step {}", step);
                }
            }
            Op::BadDelete => {
                let result = mem.delete_logged("", || {
                    peek();
                    Ok::<(), &str>(())
                });
                prop_assert!(
                    matches!(result, Err(Logged::Rejected(_))),
                    "step {}: {:?}",
                    step,
                    result
                );
                prop_assert!(
                    inside.borrow().is_none(),
                    "step {}: the log ran for a rejected delete",
                    step
                );
            }
        }
        let after = observe(&f, &reference.view());
        prop_assert_eq!(
            &observe(&f, &mem.view()),
            &after,
            "step {}: after {:?}",
            step,
            op
        );
        prop_assert_eq!(
            &observe(&f, &reader.view()),
            &after,
            "step {}: the reader",
            step
        );
        prop_assert_eq!(mem.version(), reference.version());
        prop_assert_eq!(mem.live_count(), reference.live_count());
        prop_assert_eq!(mem.is_empty(), reference.is_empty());
        if let Some(inside) = inside.into_inner() {
            prop_assert_eq!(
                &observe(&f, &inside),
                &before,
                "step {}: a view inside the log closure of {:?}",
                step,
                op
            );
        }
    }
    // Every successful put was logged with its full encoding, and the memtable's live documents are the reference's.
    let mut live = Vec::new();
    let view = mem.view();
    for d in 0..view.doc_count() {
        if view.is_live(d) {
            live.push(view.document(d).unwrap());
        }
    }
    let rview = reference.view();
    let rlive: Vec<Document> = (0..rview.doc_count())
        .filter(|&d| rview.is_live(d))
        .map(|d| rview.document(d).unwrap())
        .collect();
    prop_assert_eq!(live, rlive);
    Ok(())
}

#[test]
fn a_failed_log_leaves_no_trace_and_the_log_window_is_invisible() {
    let seed = std::env::var("PROPTEST_RNG_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(PROPTEST_SEED);
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(if cfg!(feature = "slow") { 2048 } else { 128 });
    let config = ProptestConfig {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        source_file: Some(file!()),
        test_name: Some("a_failed_log_leaves_no_trace_and_the_log_window_is_invisible"),
        ..ProptestConfig::default()
    };
    let mut runner = TestRunner::new(config);
    if let Err(e) = runner.run(&prop::collection::vec(op(), 0..40), window) {
        panic!("failed with PROPTEST_RNG_SEED={seed}: {e}\n{runner}");
    }
}

/// A sanity check of the generator: the interesting paths happen (a ghost term staged and rolled back, then a
/// successful put reusing its docid).
#[test]
fn a_rolled_back_ghost_term_and_docid_stay_unseen() {
    let ops = vec![
        Op::Put {
            id: 0,
            words: vec![0, 1],
            tag: 0,
            n: 1,
            log_ok: true,
        },
        Op::Put {
            id: 1,
            words: vec![5, 6, 0],
            tag: 2,
            n: 2,
            log_ok: false,
        },
        Op::Put {
            id: 1,
            words: vec![0, 3],
            tag: 1,
            n: 3,
            log_ok: true,
        },
        Op::Delete {
            id: 0,
            log_ok: false,
        },
        Op::Put {
            id: 2,
            words: vec![7],
            tag: 2,
            n: 4,
            log_ok: false,
        },
        Op::Delete {
            id: 1,
            log_ok: true,
        },
        Op::Put {
            id: 1,
            words: vec![1],
            tag: 0,
            n: 5,
            log_ok: true,
        },
    ];
    window(ops).unwrap();
}
