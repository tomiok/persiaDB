//! Collection statistics and BM25F scoring (ROADMAP 7.2, 7.3; SPEC §7.2), differentially against the reference
//! (`tests/reference/bm25f.rs`).
//!
//! Invariants:
//! 1. `N`, `df` and `avglen` over segments (deleted documents included) and a memtable view (live documents only)
//!    are what the reference counts from the plain documents (7.2.1, 7.2.2).
//! 2. Every live document's score for a `term` or `combined` query matches the reference within [`EPSILON`]
//!    (relative): the engine reads frequencies from postings and lengths from norm bytes, the reference counts
//!    tokens and quantizes their number (7.3.1, 7.3.4).
//! 3. A term's `TermInfo` bounds (and a list's block bounds) bound the score of every document of the list (7.3.3).
//! 4. A database snapshot computes its segments' statistics once, counts committed deletions until a merge drops
//!    them, and adds the memtable's live documents in a search (7.2.1).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::type_complexity,
    missing_docs
)] // Tests: inputs are generated or fixed.

mod reference;

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{Database, DatabaseOptions, SegmentIds};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::Memtable;
use persia_engine::query::{CombinedQuery, TermQuery, WeightedTerm};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::score::{FieldHit, TermScorer};
use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
use persia_engine::stats::{SearchStatistics, SegmentStatistics, StatisticsProvider};
use persia_engine::view::{NormReader, PostingsIterator, SearchView, SegmentView};
use persia_engine::wal::Durability;
use persia_format::ByteSource;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;
use reference::bm25f::{Clause, Corpus, Doc, Field as RefField};
use roaring::RoaringBitmap;

/// The largest relative difference allowed between the engine's score and the reference's: both evaluate the same
/// formula on the same quantized lengths, in different floating-point forms (SPEC §7.2).
const EPSILON: f64 = 1e-9;

const VOCAB: [&str; 6] = ["ant", "bee", "cat", "dog", "elk", "fox"];
const TAGS: [&str; 3] = ["x", "y", "z"];
const TITLE: usize = 0;
const BODY: usize = 1;
const TAG: usize = 2;

#[derive(Debug, Clone, Copy)]
struct Params {
    title: (f64, f64, f64),
    body: (f64, f64, f64),
}

fn text(analyzer: &AnalyzerRef, (boost, k1, b): (f64, f64, f64)) -> FieldKind {
    FieldKind::Text(TextOptions {
        analyzer: analyzer.clone(),
        boost,
        k1,
        b,
        ..TextOptions::default()
    })
}

fn schema(p: Params) -> CollectionSchema {
    // The whitespace analyzer gives the words as they are: the reference's tokens.
    let ws = AnalyzerRef::new("whitespace", 1).unwrap();
    SchemaBuilder::new("c")
        .field("title", text(&ws, p.title))
        .field("body", text(&ws, p.body))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap()
}

fn ids(schema: &CollectionSchema) -> [FieldId; 3] {
    ["title", "body", "tag"].map(|n| schema.field(n).unwrap().id())
}

fn document(schema: &CollectionSchema, id: &str, doc: &Doc) -> Document {
    let [title, body, tag] = ids(schema);
    let mut d = Document::new(id);
    if !doc[TITLE].is_empty() {
        d = d.with(title, Value::String(doc[TITLE].join(" ")));
    }
    if !doc[BODY].is_empty() {
        d = d.with(body, Value::String(doc[BODY].join(" ")));
    }
    if !doc[TAG].is_empty() {
        d = d.with(tag, Value::Strings(doc[TAG].clone()));
    }
    d
}

fn segment(schema: &CollectionSchema, id: u128, docs: &[(String, Doc)]) -> Segment {
    let mut b = SegmentBuilder::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    for (name, doc) in docs {
        b.add(&document(schema, name, doc)).unwrap();
    }
    let meta = SegmentMeta {
        segment_id: id,
        created_at: 0,
        merge_generation: 0,
    };
    let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
    Segment::open(ByteSource::new(bytes.into()), schema).unwrap()
}

/// The hits of every document of `view` holding one of `scorer`'s terms (`terms[i]` in field `i`), by docid; the
/// bounds of each field's list (its first block's, which `TermInfo`-like holds for a one-block list) checked as it
/// goes.
fn hits<V: SearchView>(
    view: &V,
    scorer: &TermScorer,
    terms: &[&str],
    text: &[bool],
) -> BTreeMap<u32, Vec<Option<FieldHit>>> {
    let width = scorer.fields().len();
    let mut out = BTreeMap::<u32, Vec<Option<FieldHit>>>::new();
    for (i, (&field, term)) in scorer.fields().iter().zip(terms).enumerate() {
        let Some(mut postings) = view.postings(field, term.as_bytes()).unwrap() else {
            continue;
        };
        let mut norms = text[i].then(|| view.norms(field).unwrap());
        while let Some(docid) = postings.next().unwrap() {
            let hit = FieldHit {
                tf: postings.freq().unwrap(),
                norm: norms.as_mut().map_or(0, |n| n.get(docid).unwrap()),
            };
            // Invariant 3, per block: the block's bounds cover the document's hit.
            let block = postings.block_max().unwrap();
            assert!(block.max_tf >= hit.tf && block.min_norm <= hit.norm);
            out.entry(docid).or_insert_with(|| vec![None; width])[i] = Some(hit);
        }
    }
    out
}

fn close(engine: f64, reference: f64, what: &str) {
    assert!(
        (engine - reference).abs() <= EPSILON * reference.abs().max(1e-300),
        "{what}: engine {engine} vs reference {reference}"
    );
}

/// A generated collection: segments of documents (each with its deleted ones) and memtable operations.
#[derive(Debug, Clone)]
struct Case {
    params: Params,
    segments: Vec<Vec<(Doc, bool)>>,
    memtable: Vec<(Doc, bool)>,
    weights: (f64, f64),
}

fn doc_strategy() -> impl Strategy<Value = Doc> {
    let words = |max| prop::collection::vec(prop::sample::select(&VOCAB[..]), 0..max);
    (
        // Long titles too: lengths from 40 on are quantized (SPEC §5.3.4).
        prop_oneof![words(8), words(90)],
        words(12),
        prop::collection::vec(any::<bool>(), 3),
    )
        .prop_map(|(title, body, tags)| {
            let tag = TAGS
                .iter()
                .zip(tags)
                .filter(|(_, on)| *on)
                .map(|(t, _)| (*t).to_owned())
                .collect();
            let own = |w: Vec<&str>| w.into_iter().map(str::to_owned).collect();
            vec![own(title), own(body), tag]
        })
}

fn field_params() -> impl Strategy<Value = (f64, f64, f64)> {
    (
        prop_oneof![Just(1.0), 0.01..=1000.0_f64],
        prop_oneof![Just(1.2), 0.0..=10.0_f64],
        prop_oneof![Just(0.75), Just(0.0), Just(1.0), 0.0..=1.0_f64],
    )
}

fn case() -> impl Strategy<Value = Case> {
    let docs = |max| prop::collection::vec((doc_strategy(), prop::bool::weighted(0.2)), 1..max);
    (
        field_params(),
        field_params(),
        prop::collection::vec(docs(25), 0..4),
        prop::collection::vec((doc_strategy(), prop::bool::weighted(0.2)), 0..10),
        (0.01..=100.0_f64, 0.01..=100.0_f64),
    )
        .prop_map(|(title, body, segments, memtable, weights)| Case {
            params: Params { title, body },
            segments,
            memtable,
            weights,
        })
}

fn check(case: &Case) {
    let schema = schema(case.params);
    let [title, body, tag] = ids(&schema);
    let registry = AnalyzerRegistry::builtin();

    // The engine's collection and the reference's corpus, side by side; `live` maps ids to their documents.
    let mut corpus = Corpus {
        fields: vec![
            RefField {
                k1: case.params.title.1,
                b: case.params.title.2,
                keyword: false,
            },
            RefField {
                k1: case.params.body.1,
                b: case.params.body.2,
                keyword: false,
            },
            RefField {
                k1: 1.2,
                b: 0.0,
                keyword: true,
            },
        ],
        counted: Vec::new(),
    };
    let mut live = BTreeMap::<String, Doc>::new();
    let mut segments = Vec::new();
    let mut tombstones = Vec::new();
    for (s, docs) in case.segments.iter().enumerate() {
        let named: Vec<(String, Doc)> = docs
            .iter()
            .enumerate()
            .map(|(j, (d, _))| (format!("s{s}-{j}"), d.clone()))
            .collect();
        let seg = Arc::new(segment(&schema, s as u128 + 1, &named));
        let mut deleted = RoaringBitmap::new();
        for (j, ((name, d), (_, del))) in named.iter().zip(docs).enumerate() {
            // A segment's deleted documents count until a merge drops them (SPEC §7.2).
            corpus.counted.push(d.clone());
            if *del {
                deleted.insert(j as u32);
            } else {
                live.insert(name.clone(), d.clone());
            }
        }
        tombstones.push((!deleted.is_empty()).then(|| seg.tombstones(deleted).unwrap()));
        segments.push(seg);
    }
    let mut mem = Memtable::new(&schema, &registry).unwrap();
    for (j, (d, del)) in case.memtable.iter().enumerate() {
        let name = format!("m{j}");
        mem.put(&document(&schema, &name, d)).unwrap();
        if *del {
            assert!(mem.delete(&name).unwrap());
        } else {
            // The memtable counts its live documents only.
            corpus.counted.push(d.clone());
            live.insert(name, d.clone());
        }
    }
    let view = mem.view();
    let segment_stats = SegmentStatistics::new(&segments);
    let stats = SearchStatistics::new(&segment_stats, &view);

    // Invariant 1.
    assert_eq!(stats.doc_count(), corpus.n());
    for (i, field) in [title, body, tag].into_iter().enumerate() {
        let totals = stats.field(field);
        let with = corpus.counted.iter().filter(|d| !d[i].is_empty()).count() as u64;
        let sum: usize = corpus.counted.iter().map(|d| d[i].len()).sum();
        assert_eq!(
            (totals.docs_with_field, totals.sum_field_len),
            (with, sum as u64)
        );
        close(totals.avglen(), corpus.avglen(i), "avglen");
        let terms: &[&str] = if i == TAG { &TAGS } else { &VOCAB };
        for t in terms {
            assert_eq!(
                stats.df(field, t.as_bytes()).unwrap(),
                corpus.df(i, t),
                "df {t}"
            );
            // Cached: the same answer twice.
            assert_eq!(
                segment_stats.df(field, t.as_bytes()).unwrap(),
                segment_stats.df(field, t.as_bytes()).unwrap()
            );
        }
    }

    // Invariants 2 and 3: every query word, as a `combined` over both text fields, a `term` on each, and a
    // `keyword` term.
    let (wt, wb) = case.weights;
    let mut queries: Vec<(TermScorer, Vec<Clause>, Vec<&str>, Vec<bool>)> = Vec::new();
    for word in VOCAB {
        let combined = CombinedQuery {
            fields: vec![
                WeightedTerm {
                    field: title,
                    term: word.into(),
                    weight: wt,
                },
                WeightedTerm {
                    field: body,
                    term: word.into(),
                    weight: wb,
                },
            ],
        };
        let clause = |field, weight| Clause {
            field,
            term: word.into(),
            weight,
        };
        queries.push((
            TermScorer::for_combined(&combined, &schema, &stats).unwrap(),
            vec![clause(TITLE, wt), clause(BODY, wb)],
            vec![word, word],
            vec![true, true],
        ));
        for (field, index, boost) in [
            (title, TITLE, case.params.title.0),
            (body, BODY, case.params.body.0),
        ] {
            let q = TermQuery {
                field,
                term: word.into(),
            };
            queries.push((
                TermScorer::for_term(&q, &schema, &stats).unwrap(),
                vec![clause(index, boost)],
                vec![word],
                vec![true],
            ));
        }
    }
    for t in TAGS {
        let q = TermQuery {
            field: tag,
            term: t.into(),
        };
        queries.push((
            TermScorer::for_term(&q, &schema, &stats).unwrap(),
            vec![Clause {
                field: TAG,
                term: t.into(),
                weight: 1.0,
            }],
            vec![t],
            vec![false],
        ));
    }

    for (scorer, clauses, terms, text) in &queries {
        let mut scored = 0;
        let mut check_view = |view: &dyn Fn() -> Vec<(String, f64, f64)>| {
            for (id, engine, bound) in view() {
                let reference = corpus.score(clauses, &live[&id]);
                close(engine, reference, &format!("{id} {clauses:?}"));
                assert!(
                    engine <= bound,
                    "{id}: {engine} above its term bound {bound}"
                );
                scored += 1;
            }
        };
        for (seg, t) in segments.iter().zip(&tombstones) {
            let v = SegmentView::new(seg, t.as_ref());
            check_view(&|| {
                // The term's bounds from each field's `TermInfo` (invariant 3).
                let bounds: Vec<Option<FieldHit>> = scorer
                    .fields()
                    .iter()
                    .zip(terms)
                    .map(|(&f, term)| {
                        let mut index = seg.terms(f).unwrap();
                        index
                            .get(term.as_bytes())
                            .unwrap()
                            .map(|i| FieldHit::from(&i))
                    })
                    .collect();
                let bound = scorer.score(&bounds);
                hits(&v, scorer, terms, text)
                    .into_iter()
                    .map(|(docid, h)| {
                        (
                            v.document(docid).unwrap().id().to_owned(),
                            scorer.score(&h),
                            bound,
                        )
                    })
                    .collect()
            });
        }
        check_view(&|| {
            hits(&view, scorer, terms, text)
                .into_iter()
                .map(|(docid, h)| {
                    (
                        view.document(docid).unwrap().id().to_owned(),
                        scorer.score(&h),
                        f64::MAX,
                    )
                })
                .collect()
        });
        // Every live document the reference scores above 0 was reached through the postings.
        let expected = live
            .values()
            .filter(|d| corpus.score(clauses, d) > 0.0)
            .count();
        assert_eq!(scored, expected, "{clauses:?}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 2000 } else { 96 }))]

    #[test]
    fn engine_matches_reference(case in case()) {
        check(&case);
    }
}

/// A list long enough for skip entries (SPEC §5.3.2): its block bounds still cover every document.
#[test]
fn many_blocks_match_reference() {
    let docs: Vec<(Doc, bool)> = (0..700_usize)
        .map(|i| {
            let title = (0..=(i % 50))
                .map(|k| VOCAB[(i + k) % 3].to_owned())
                .collect();
            let body = vec![VOCAB[i % 6].to_owned(); i % 4];
            (vec![title, body, vec![TAGS[i % 3].to_owned()]], i % 7 == 0)
        })
        .collect();
    check(&Case {
        params: Params {
            title: (3.0, 1.2, 0.75),
            body: (1.0, 2.0, 0.3),
        },
        segments: vec![docs[..400].to_vec(), docs[400..].to_vec()],
        memtable: docs[..20].to_vec(),
        weights: (2.0, 0.5),
    });
}

#[test]
fn snapshot_statistics_are_cached_and_count_deletions() {
    let ws = AnalyzerRef::new("whitespace", 1).unwrap();
    let schema = SchemaBuilder::new("c")
        .field("body", text(&ws, (1.0, 1.2, 0.75)))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let doc = |id: &str, text: &str| Document::new(id).with(body, Value::String(text.into()));
    let options = DatabaseOptions {
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [1; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    for (id, text) in [("a", "red shoe"), ("b", "red hat"), ("c", "blue")] {
        db.put("c", &doc(id, text), Durability::Fsync).unwrap();
    }
    db.commit().unwrap();
    db.delete("c", "a", Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("c", &doc("d", "red red red sock"), Durability::Fsync)
        .unwrap();

    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let stats = c.statistics();
    assert!(
        std::ptr::eq(stats, c.statistics()),
        "computed once per snapshot"
    );
    // `a` is deleted but still counted: no merge dropped it.
    assert_eq!(stats.doc_count(), 3);
    assert_eq!(stats.df(body, b"red").unwrap(), 2);
    assert_eq!(stats.field(body).sum_field_len, 5);
    // The memtable's `d` is added in a search.
    let memtable = c.memtable();
    let search = c.search_statistics(&memtable);
    assert_eq!(search.doc_count(), 4);
    assert_eq!(search.df(body, b"red").unwrap(), 3);
    assert_eq!(search.field(body).docs_with_field, 4);
    assert_eq!(search.field(body).sum_field_len, 9);
    // A snapshot of the next commit counts `d` in its segments; the old snapshot is unchanged.
    db.commit().unwrap();
    let next = db.snapshot();
    let next_stats = next.collection("c").unwrap().statistics();
    assert_eq!(next_stats.doc_count(), 4);
    assert_eq!(stats.doc_count(), 3);
}
