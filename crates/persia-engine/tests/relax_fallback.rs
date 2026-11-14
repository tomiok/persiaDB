//! L5 `fallback` through a database (SPEC §7.6, ROADMAP 8.2.5): the collection's own ranking of the documents no
//! earlier step matched.
//!
//! Invariants (checked against an oracle over random corpora: puts, upserts and deletes, committed at random points so
//! the documents spread over segments and the memtable):
//! 1. L5 returns exactly the top `N` by (static rank, recency) of the live documents the filter keeps, the excluded
//!    clause does not match and the earlier levels missed. Static rank is SPEC §6.2.3's; recency, without merges, is
//!    write order (newest first), as segments are listed in commit order and each holds its documents in write order.
//! 2. Pages by cursor are slices of that ranking; the response's `fallback` names the order when L5 found the hits.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, missing_docs)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, RelaxedResults, SearchRequest, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, Filter, FilterValue, QueryCompiler, QueryLimits};
use persia_engine::relax::{FallbackOrder, RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    Fallback, FieldKind, KeywordOptions, Level, Levels, RankFn, RankingConfig, RelaxationConfig,
    SchemaBuilder, StaticRank, TextOptions,
};
use persia_engine::score::static_rank;
use persia_engine::search::{Projection, Pruning};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;

const WORDS: [&str; 5] = ["ant", "bee", "cat", "dog", "eel"];

/// One write: a put of `id` (an upsert when it is live), or its delete, then maybe a commit.
#[derive(Debug, Clone)]
enum Op {
    Put {
        id: u8,
        words: Vec<usize>,
        tag: bool,
        sales: Option<i64>,
    },
    Delete(u8),
}

fn op() -> impl Strategy<Value = (Op, bool)> {
    let put = (
        0..12u8,
        prop::collection::vec(0..WORDS.len(), 1..4),
        any::<bool>(),
        prop::option::weighted(0.8, -3..6i64),
    )
        .prop_map(|(id, words, tag, sales)| Op::Put {
            id,
            words,
            tag,
            sales,
        });
    let op = prop_oneof![4 => put, 1 => (0..12u8).prop_map(Op::Delete)];
    (op, prop::bool::weighted(0.2))
}

/// The collection's static rank: none, `log1p` of weight 1, `linear` of weight 2, or of weight 0 (recency alone).
fn ranking() -> impl Strategy<Value = Option<StaticRank>> {
    prop_oneof![
        Just(None),
        Just(Some(StaticRank::new("sales"))),
        Just(Some(StaticRank {
            field: "sales".into(),
            weight: 2.0,
            function: RankFn::Linear,
        })),
        Just(Some(StaticRank {
            field: "sales".into(),
            weight: 0.0,
            function: RankFn::Linear,
        })),
    ]
}

/// A live document as the oracle knows it: its write order (`seq`, later is more recent) and fields.
#[derive(Debug, Clone)]
struct Live {
    seq: usize,
    words: Vec<usize>,
    tag: bool,
    sales: Option<i64>,
}

fn database(
    ops: &[(Op, bool)],
    rank: Option<&StaticRank>,
) -> (Database<SimFile>, BTreeMap<String, Live>) {
    let mut builder = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("sales", FieldKind::I64 { stored: false })
        .field("tag", FieldKind::Keyword(KeywordOptions::default()));
    if let Some(rank) = rank {
        builder = builder.ranking(RankingConfig {
            static_rank: Some(rank.clone()),
        });
    }
    let schema = builder.build().unwrap();
    let field = |n: &str| schema.field(n).unwrap().id();
    let (body, sales, tag) = (field("body"), field("sales"), field("tag"));
    let mut db = Database::create_on(SimFile::new(), [4; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    let mut live = BTreeMap::new();
    for (seq, (op, commit)) in ops.iter().enumerate() {
        match op {
            Op::Put {
                id,
                words,
                tag: t,
                sales: s,
            } => {
                let text: Vec<&str> = words.iter().map(|&w| WORDS[w]).collect();
                let mut doc = Document::new(format!("d{id}"))
                    .with(body, Value::String(text.join(" ")))
                    .with(tag, Value::String(if *t { "x" } else { "y" }.into()));
                if let Some(s) = s {
                    doc = doc.with(sales, Value::I64(*s));
                }
                db.put("c", &doc, Durability::Buffered).unwrap();
                live.insert(
                    format!("d{id}"),
                    Live {
                        seq,
                        words: words.clone(),
                        tag: *t,
                        sales: *s,
                    },
                );
            }
            Op::Delete(id) => {
                db.delete("c", &format!("d{id}"), Durability::Buffered)
                    .unwrap();
                live.remove(&format!("d{id}"));
            }
        }
        if *commit {
            db.commit().unwrap();
        }
    }
    (db, live)
}

/// The query: `zzq` (in no document) and `word`, maybe excluding `excluded`, maybe filtered to one tag.
#[derive(Debug, Clone)]
struct Case {
    word: usize,
    excluded: Option<usize>,
    filter: Option<bool>,
}

fn case() -> impl Strategy<Value = Case> {
    (
        0..WORDS.len(),
        prop::option::of(0..WORDS.len()),
        prop::option::of(any::<bool>()),
    )
        .prop_map(|(word, excluded, filter)| Case {
            word,
            excluded,
            filter,
        })
}

/// L0, L2 and L5, relaxing until every step is built (`min_results` 10 000).
fn plan() -> RelaxPlan {
    RelaxPlan::new(
        &RelaxationConfig::default(),
        &RelaxOptions {
            min_results: Some(*RelaxationConfig::MIN_RESULTS.end()),
            levels: Some(Levels::Custom(vec![Level::DropTerms])),
            fallback: Some(Fallback::StaticRank),
            ..RelaxOptions::default()
        },
    )
    .unwrap()
}

fn search(db: &Database<SimFile>, case: &Case, request: &SearchRequest) -> RelaxedResults {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let mut text = String::new();
    if let Some(e) = case.excluded {
        text = format!("-{} ", WORDS[e]);
    }
    text.push_str("zzq ");
    text.push_str(WORDS[case.word]);
    let mut compiled = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(
            &text,
            &DefaultFields::all(c.schema()),
            &QueryLimits::default(),
        );
    if let Some(x) = case.filter {
        let tag = c.schema().field("tag").unwrap().id();
        compiled = compiled.with_filter([Filter::Term {
            field: tag,
            value: FilterValue::Keyword(if x { "x" } else { "y" }.into()),
        }]);
    }
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let exec = Execution::new(Pruning::default())
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)));
    views
        .search_relaxed(&compiled, request, &plan(), &exec)
        .unwrap()
}

/// The oracle's L5: the live documents the filter keeps, without the excluded word, that L2 (any document holding
/// `word`; `zzq` is in none) missed, by static rank then recency (write order, newest first).
fn oracle(
    live: &BTreeMap<String, Live>,
    case: &Case,
    rank: Option<&StaticRank>,
) -> Vec<(String, f64)> {
    let mut hits: Vec<(String, f64, usize)> = live
        .iter()
        .filter(|(_, d)| case.filter.is_none_or(|x| d.tag == x))
        .filter(|(_, d)| case.excluded.is_none_or(|e| !d.words.contains(&e)))
        .filter(|(_, d)| !d.words.contains(&case.word))
        .map(|(id, d)| {
            let score = rank.map_or(0.0, |r| {
                static_rank(r, d.sales.map(Value::I64).as_ref()) + 0.0
            });
            (id.clone(), score, d.seq)
        })
        .collect();
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.cmp(&a.2)));
    hits.into_iter().map(|(id, s, _)| (id, s)).collect()
}

fn request(limit: usize) -> SearchRequest {
    SearchRequest {
        limit,
        projection: Projection::None,
        ..SearchRequest::default()
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if cfg!(feature = "slow") { 512 } else { 64 },
        ..ProptestConfig::default()
    })]

    #[test]
    fn l5_is_the_top_by_static_rank_and_recency_of_what_the_filters_keep_and_earlier_levels_missed(
        ops in prop::collection::vec(op(), 1..40),
        rank in ranking(),
        case in case(),
        k in 1..5usize,
    ) {
        let (db, live) = database(&ops, rank.as_ref());
        let want = oracle(&live, &case, rank.as_ref());
        let out = search(&db, &case, &request(100));
        let l5: Vec<(String, f64)> = out
            .hits
            .iter()
            .filter(|h| h.level == RelaxLevel::Fallback)
            .map(|h| (h.hit.document.id().to_owned(), h.hit.score))
            .collect();
        prop_assert_eq!(&l5, &want);
        // L5's hits come last, after every earlier step's.
        prop_assert!(out.hits.iter().skip(out.hits.len() - l5.len()).all(|h| h.level == RelaxLevel::Fallback));
        let order = FallbackOrder::of(&RankingConfig { static_rank: rank.clone() });
        prop_assert_eq!(out.relaxation.fallback, (!want.is_empty()).then_some(order));
        prop_assert_eq!(out.relaxation.level, RelaxLevel::Fallback);
        // The top `k` of a page of `k`, and pages by cursor, are slices of the same ranking.
        let whole: Vec<String> = out.hits.iter().map(|h| h.hit.document.id().to_owned()).collect();
        let mut paged = Vec::new();
        let mut page = request(k);
        loop {
            let out = search(&db, &case, &page);
            let Some(last) = out.hits.last() else { break };
            prop_assert_eq!(out.relaxation.fallback, (!want.is_empty()).then_some(order));
            page.search_after = Some(last.cursor());
            paged.extend(out.hits.iter().map(|h| h.hit.document.id().to_owned()));
            prop_assert!(paged.len() <= whole.len(), "a page repeats hits");
        }
        prop_assert_eq!(paged, whole);
    }
}

#[test]
fn a_document_upserted_later_is_more_recent_and_a_deleted_one_never_returns() {
    // Without a static rank: `d1` then `d2` in a segment, `d1` upserted into the memtable, `d3` put and deleted.
    let ops = [
        (
            Op::Put {
                id: 1,
                words: vec![0],
                tag: true,
                sales: None,
            },
            false,
        ),
        (
            Op::Put {
                id: 2,
                words: vec![0],
                tag: true,
                sales: None,
            },
            true,
        ),
        (
            Op::Put {
                id: 1,
                words: vec![1],
                tag: true,
                sales: None,
            },
            false,
        ),
        (
            Op::Put {
                id: 3,
                words: vec![1],
                tag: true,
                sales: None,
            },
            false,
        ),
        (Op::Delete(3), false),
    ];
    let (db, _) = database(&ops, None);
    let case = Case {
        word: 4,
        excluded: None,
        filter: None,
    };
    let out = search(&db, &case, &request(10));
    let ids: Vec<&str> = out.hits.iter().map(|h| h.hit.document.id()).collect();
    assert_eq!(ids, ["d1", "d2"]);
    assert_eq!(out.relaxation.fallback, Some(FallbackOrder::Recency));
}
