//! The collection's static rank in every query hit's score (ROADMAP 10.1.45; SPEC §6.2.3, §7.2, §7.3, §7.7).
//!
//! Invariants:
//! 1. A hit's score is its query's score plus `weight · fn(v)` (0 without a value or with a negative one), so of two
//!    documents with equal text scores the one of higher static rank ranks first, in a plain search, in each relaxed
//!    step, and on every page a cursor walks. Without a static rank, or with `weight` 0, they tie and rank by location.
//! 2. Pruning never changes the hits: over many FOR blocks of the static rank's column, in segments and the memtable,
//!    every strategy returns the exhaustive page, scores bit for bit, and that page is the reference's (the query's
//!    score plus the contribution, sorted).
//! 3. An explained hit is a `with_static_rank` node around its query's tree, with the contribution and the value, and
//!    scores the hit bit for bit; inside a relaxed step's `relaxed` node too. An L5 hit is `static_rank` alone: its
//!    score is the contribution, not counted twice.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are fixed.

mod reference;

use std::sync::Arc;

use persia_engine::db::{
    CollectionSnapshot, Database, DatabaseOptions, ManualClock, SearchRequest, SegmentIds, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{Query, QueryLimits, TermQuery};
use persia_engine::relax::RelaxLevel;
use persia_engine::schema::{
    CollectionSchema, FieldKind, RankFn, RankingConfig, SchemaBuilder, StaticRank, TextOptions,
};
use persia_engine::search::request::{QueryRequest, QueryResults, RunOptions};
use persia_engine::search::{Explanation, Pruning, SearchPlan};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use reference::static_rank as ref_rank;

fn schema(rank: Option<StaticRank>) -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("sales", FieldKind::I64 { stored: true })
        .ranking(RankingConfig::default().static_rank(rank))
        .build()
        .unwrap()
}

fn database(schema: CollectionSchema) -> Database<SimFile> {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(45),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [45; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    db
}

fn put(db: &mut Database<SimFile>, id: &str, body: &str, sales: Option<i64>) {
    let schema = db.snapshot().collection("c").unwrap().schema().clone();
    let field = |n: &str| schema.field(n).unwrap().id();
    let mut doc = Document::new(id).with(field("body"), Value::String(body.into()));
    if let Some(s) = sales {
        doc = doc.with(field("sales"), Value::I64(s));
    }
    db.put("c", &doc, Durability::Buffered).unwrap();
}

/// Four documents of the same text (so the same text score), two committed and two in the memtable: `a` sells 1,
/// `b` 100, `c` nothing, `d` −5 (ranks as 0).
fn equal_texts(rank: Option<StaticRank>) -> Database<SimFile> {
    let mut db = database(schema(rank));
    put(&mut db, "a", "red shoes", Some(1));
    put(&mut db, "b", "red shoes", Some(100));
    db.commit().unwrap();
    put(&mut db, "c", "red shoes", None);
    put(&mut db, "d", "red shoes", Some(-5));
    db
}

fn options() -> RunOptions {
    RunOptions::new()
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

fn run(c: &CollectionSnapshot, json: &str) -> QueryResults {
    QueryRequest::from_json(json, c.schema(), &QueryLimits::default())
        .unwrap()
        .run(c, &options())
        .unwrap()
        .results
}

/// Each hit's id, score and explanation, from either search.
fn hits(results: &QueryResults) -> Vec<(String, f64, Option<Explanation>)> {
    match results {
        QueryResults::Plain(r) => r
            .hits
            .iter()
            .map(|h| (h.document.id().to_owned(), h.score, h.explanation.clone()))
            .collect(),
        QueryResults::Relaxed(r) => r
            .hits
            .iter()
            .map(|h| {
                let hit = &h.hit;
                (
                    hit.document.id().to_owned(),
                    hit.score,
                    hit.explanation.clone(),
                )
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn ids(results: &QueryResults) -> Vec<String> {
    hits(results).into_iter().map(|h| h.0).collect()
}

/// Invariant 1: equal text scores rank by static rank, the contribution added to the text score exactly.
#[test]
fn equal_text_scores_rank_by_static_rank() {
    let db = equal_texts(Some(StaticRank::new("sales")));
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    for json in [
        r#"{"q": "red", "relax": false}"#,
        r#"{"q": "red"}"#,
        // L0 finds nothing: the hits are a later step's, ranked within it by the same total.
        r#"{"q": "red zebra"}"#,
    ] {
        let found = hits(&run(c, json));
        let order: Vec<&str> = found.iter().map(|h| h.0.as_str()).collect();
        // `c` and `d` add 0 and tie: by location, the memtable's own docid order.
        assert_eq!(order, ["b", "a", "c", "d"], "{json}");
        let text = found[2].1;
        assert_eq!(found[3].1, text, "{json}");
        let contribution =
            |sales: f64| ref_rank::contribution(1.0, ref_rank::RankFn::Log1p, Some(sales));
        assert_eq!(found[0].1, text + contribution(100.0), "{json}");
        assert_eq!(found[1].1, text + contribution(1.0), "{json}");
    }
    let relaxed = run(c, r#"{"q": "red zebra"}"#);
    let QueryResults::Relaxed(r) = &relaxed else {
        panic!("{relaxed:?}")
    };
    assert!(r.hits.iter().all(|h| h.level == RelaxLevel::DropTerms));
}

/// Invariant 1: without a static rank, or with `weight` 0, the same documents tie and rank by location.
#[test]
fn without_a_static_rank_equal_texts_tie() {
    for rank in [None, Some(StaticRank::new("sales").weight(0.0))] {
        let db = equal_texts(rank.clone());
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let found = hits(&run(c, r#"{"q": "red", "relax": false, "explain": true}"#));
        assert_eq!(
            found.iter().map(|h| h.0.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c", "d"],
            "{rank:?}"
        );
        assert!(found.iter().all(|h| h.1 == found[0].1));
        // No static rank node either.
        assert!(
            found
                .iter()
                .all(|h| matches!(h.2, Some(Explanation::Word(_)))),
            "{found:?}"
        );
    }
}

/// Invariant 1: pages by cursor are slices of the static-ranked order.
#[test]
fn cursor_pages_follow_the_static_rank() {
    let db = equal_texts(Some(StaticRank::new("sales").function(RankFn::Linear)));
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    for relax in ["false", "true"] {
        let mut paged = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let json = match &after {
                Some(token) => {
                    format!(
                        r#"{{"q": "red", "relax": {relax}, "limit": 1, "search_after": "{token}"}}"#
                    )
                }
                None => format!(r#"{{"q": "red", "relax": {relax}, "limit": 1}}"#),
            };
            let results = run(c, &json);
            let token = match &results {
                QueryResults::Plain(r) => r.hits.last().map(|h| h.cursor().to_token()),
                QueryResults::Relaxed(r) => r.hits.last().map(|h| h.cursor().to_token()),
                other => panic!("{other:?}"),
            };
            paged.extend(ids(&results));
            match token {
                Some(t) => after = Some(t),
                None => break,
            }
        }
        assert_eq!(paged, ["b", "a", "c", "d"], "relax {relax}");
    }
}

/// Invariant 3: the explanation is the query's tree plus the static rank, scoring the hit bit for bit; an L5 hit's is
/// the static rank alone, its score the contribution only.
#[test]
fn explained_hits_add_the_static_rank_node() {
    let db = equal_texts(Some(StaticRank::new("sales")));
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    for (json, relaxed) in [
        (r#"{"q": "red", "relax": false, "explain": true}"#, false),
        (r#"{"q": "red", "explain": true}"#, true),
    ] {
        for (id, score, explanation) in hits(&run(c, json)) {
            let explanation = explanation.unwrap();
            assert_eq!(explanation.score().to_bits(), score.to_bits(), "{id}");
            let tree = match (&explanation, relaxed) {
                (Explanation::Relaxed(r), true) => *r.explanation.clone(),
                (other, false) => other.clone(),
                (other, _) => panic!("{other:?}"),
            };
            let Explanation::WithStaticRank {
                score: total,
                query,
                static_rank,
                value,
            } = tree
            else {
                panic!("{id}: {tree:?}")
            };
            assert!(matches!(*query, Explanation::Word(_)), "{query:?}");
            assert_eq!(total.to_bits(), score.to_bits());
            assert_eq!(total, query.score() + static_rank);
            let sales = match id.as_str() {
                "a" => Some(1.0),
                "b" => Some(100.0),
                "c" => None,
                _ => Some(-5.0),
            };
            assert_eq!(value, sales, "{id}");
            assert_eq!(
                static_rank,
                ref_rank::contribution(1.0, ref_rank::RankFn::Log1p, sales)
            );
        }
    }
    // L5: no query word scores, the contribution is the whole score, once.
    let fallback = run(c, r#"{"q": "purple", "explain": true}"#);
    for (id, score, explanation) in hits(&fallback) {
        let Some(Explanation::Relaxed(r)) = explanation else {
            panic!("{id}")
        };
        let Explanation::StaticRank { score: s, .. } = *r.explanation else {
            panic!("{id}: {:?}", r.explanation)
        };
        assert_eq!(
            (s.to_bits(), r.level),
            (score.to_bits(), RelaxLevel::Fallback)
        );
    }
    // L5 breaks ties by recency, the newest first: `d` before `c` (SPEC §7.6).
    assert_eq!(ids(&fallback), ["b", "a", "d", "c"]);
    // The response JSON writes the node.
    let response = QueryRequest::from_json(
        r#"{"q": "red", "relax": false, "explain": true}"#,
        c.schema(),
        &QueryLimits::default(),
    )
    .unwrap()
    .run(c, &options())
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&response.to_json(c.schema()).unwrap()).unwrap();
    let node = &json["hits"][0]["explanation"]["with_static_rank"];
    assert_eq!(node["static_rank"]["value"], 100.0, "{json}");
    assert!(node["query"]["word"].is_object(), "{json}");
    assert_eq!(node["score"], json["hits"][0]["score"], "{json}");
}

/// Invariant 2: over a segment of many FOR blocks and a memtable, every strategy returns the exhaustive page, which is
/// the reference's: the text score plus the contribution, best first, then by location.
#[test]
fn pruning_keeps_the_static_ranked_page() {
    for (function, weight) in [
        (RankFn::Log1p, 1.0),
        (RankFn::Linear, 0.01),
        (RankFn::Log1p, 1000.0),
    ] {
        let rank = StaticRank::new("sales").function(function).weight(weight);
        let mut db = database(schema(Some(rank)));
        let mut sales = std::collections::BTreeMap::new();
        // xorshift: fixed, so the corpus is the same on every run.
        let mut x = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for i in 0..1_500 {
            let words = ["red", "shoes", "hat", "boots", "red red"];
            let text = format!(
                "{} {}",
                words[usize::try_from(next() % 5).unwrap()],
                words[usize::try_from(next() % 5).unwrap()]
            );
            let sales_value =
                (next() % 4 != 0).then(|| i64::try_from(next() % 3_000).unwrap() - 200);
            let id = format!("d{i:04}");
            put(&mut db, &id, &text, sales_value);
            sales.insert(id, sales_value);
            if i == 1_199 {
                db.commit().unwrap();
            }
        }
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let memtable = c.memtable();
        let views = c.views(&memtable).unwrap();
        let body = c.schema().field("body").unwrap().id();
        let query = Query::Term(TermQuery {
            field: body,
            term: "red".into(),
        });
        let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable)).unwrap();
        let page = |pruning: Pruning, limit: usize| {
            let request = SearchRequest {
                limit,
                ..SearchRequest::default()
            };
            views
                .search(&plan, &request, pruning)
                .unwrap()
                .hits
                .into_iter()
                .map(|h| (h.document.id().to_owned(), h.score.to_bits()))
                .collect::<Vec<_>>()
        };
        let all = page(Pruning::Exhaustive, 10_000);
        assert!(all.len() > 500, "{}", all.len());
        for limit in [1, 10, 100] {
            let want = page(Pruning::Exhaustive, limit);
            assert_eq!(want[..], all[..limit], "{function:?} {weight} top {limit}");
            for pruning in [Pruning::MaxScore, Pruning::BlockMaxWand] {
                assert_eq!(
                    page(pruning, limit),
                    want,
                    "{function:?} {weight} {pruning:?} top {limit}"
                );
            }
        }
        // Each hit is its query's score plus the reference's contribution of its value, and the page is in the order
        // of those totals.
        let request = SearchRequest {
            limit: 10_000,
            explain: true,
            ..SearchRequest::default()
        };
        let explained = views
            .search(&plan, &request, Pruning::Exhaustive)
            .unwrap()
            .hits;
        let reference = match function {
            RankFn::Linear => ref_rank::RankFn::Linear,
            _ => ref_rank::RankFn::Log1p,
        };
        for pair in explained.windows(2) {
            assert!(
                pair[0].score >= pair[1].score,
                "{} {}",
                pair[0].score,
                pair[1].score
            );
        }
        for hit in &explained {
            let id = hit.document.id();
            let Some(Explanation::WithStaticRank {
                score,
                query,
                static_rank,
                value,
            }) = &hit.explanation
            else {
                panic!("{id}: {:?}", hit.explanation)
            };
            #[allow(clippy::cast_precision_loss)] // small values, exact
            let sales = sales[id].map(|v| v as f64);
            assert_eq!(*value, sales, "{id}");
            let want = ref_rank::contribution(weight, reference, sales);
            assert!(
                (static_rank - want).abs() <= 1e-12 * want.max(1.0),
                "{id}: {static_rank} vs {want}"
            );
            assert_eq!(score.to_bits(), hit.score.to_bits(), "{id}");
            assert_eq!(*score, ref_rank::total(query.score(), *static_rank), "{id}");
        }
    }
}
