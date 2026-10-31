//! A search over a whole collection: its segments and its memtable (ROADMAP 7.4.6; SPEC §7.3, §8.3).
//!
//! Invariants:
//! 1. Before the commit that flushes it, the memtable's put or delete of an id hides every segment copy of it, as
//!    `CollectionSnapshot::lookup` does: a search returns each live id once, at its live version, and no deleted id.
//! 2. A memtable view hides only what its own operations name: a write after it changes nothing it returns, even
//!    after a newer view was searched (the snapshot's cache of hidden copies).
//! 3. Hits rank by score descending, then segment order with the memtable last, then docid; pruned and exhaustive
//!    top-k are the same hits, scores bit for bit (SPEC §7.3).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::Arc;

use persia_engine::db::{
    CollectionHit, CollectionSnapshot, Database, DatabaseOptions, Located, ManualClock, SegmentIds,
    Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::MemtableView;
use persia_engine::query::{BoolQuery, Query, TermQuery};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestCaseError, TestRunner};

const WORDS: [&str; WORD_COUNT as usize] = ["alpha", "beta", "gamma", "common", "omega"];
const WORD_COUNT: u8 = 5;
const IDS: u8 = 6;
const PROPTEST_SEED: u64 = 0x7406_0000_0001;
const STRATEGIES: [Pruning; 3] = [
    Pruning::Exhaustive,
    Pruning::MaxScore,
    Pruning::BlockMaxWand,
];

fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap()
}

fn fields() -> (FieldId, FieldId) {
    let s = schema();
    (s.field("body").unwrap().id(), s.field("tag").unwrap().id())
}

fn doc(id: &str, tag: &str, body: &str) -> Document {
    let (b, t) = fields();
    Document::new(id)
        .with(b, Value::String(body.into()))
        .with(t, Value::String(tag.into()))
}

fn database() -> Database<SimFile> {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [4; 16], options).unwrap();
    db.create_collection(schema()).unwrap();
    db
}

fn term(field: FieldId, t: &str) -> Query {
    Query::Term(TermQuery {
        field,
        term: t.into(),
    })
}

fn any_word() -> Query {
    let (body, _) = fields();
    Query::Bool(BoolQuery {
        should: WORDS.iter().map(|w| term(body, w)).collect(),
        ..BoolQuery::default()
    })
}

/// The document id of each hit.
fn ids(c: &CollectionSnapshot, memtable: &MemtableView, hits: &[CollectionHit]) -> Vec<String> {
    hits.iter()
        .map(|h| match h.location {
            Located::Memtable(docid) => memtable.document(docid).unwrap().id().to_owned(),
            Located::Segment { index, docid } => c.segments()[index].id_of(docid).unwrap(),
        })
        .collect()
}

/// The best `k` hits of `query` as of `memtable`, the same under every pruning strategy.
fn search(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    query: &Query,
    k: usize,
) -> Vec<CollectionHit> {
    let plan = SearchPlan::new(query, c.schema(), &c.search_statistics(memtable)).unwrap();
    let views = c.views(memtable).unwrap();
    let bits = |hits: &[CollectionHit]| -> Vec<(Located, u64)> {
        hits.iter()
            .map(|h| (h.location, h.score.to_bits()))
            .collect()
    };
    let exhaustive = views.top_k(&plan, k, Pruning::Exhaustive).unwrap();
    for pruning in STRATEGIES {
        let hits = views.top_k(&plan, k, pruning).unwrap();
        assert_eq!(bits(&hits), bits(&exhaustive), "{pruning:?}, k = {k}");
    }
    exhaustive
}

/// The ids holding `tag` as of `memtable`, best first.
fn tagged(c: &CollectionSnapshot, memtable: &MemtableView, tag: &str) -> Vec<String> {
    let (_, t) = fields();
    let hits = search(c, memtable, &term(t, tag), 100);
    ids(c, memtable, &hits)
}

#[test]
fn an_upsert_hides_the_committed_copy_before_the_commit() {
    let mut db = database();
    db.put("c", &doc("x", "a", "one"), Durability::Fsync)
        .unwrap();
    db.put("c", &doc("y", "a", "one"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    db.put("c", &doc("x", "b", "two"), Durability::Fsync)
        .unwrap();

    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    // The committed copy of `x` is still live in its segment: only the memtable hides it.
    assert!(
        c.segment_views()
            .next()
            .unwrap()
            .docid_of("x")
            .unwrap()
            .is_some()
    );
    assert_eq!(tagged(c, &memtable, "a"), ["y"]);
    assert_eq!(tagged(c, &memtable, "b"), ["x"]);
    let views = c.views(&memtable).unwrap();
    assert_eq!(
        views.segments().next().unwrap().docid_of("x").unwrap(),
        None
    );

    // After the commit the tombstones hide it: the same ids.
    db.commit().unwrap();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    assert_eq!(tagged(c, &memtable, "a"), ["y"]);
    assert_eq!(tagged(c, &memtable, "b"), ["x"]);
}

#[test]
fn a_delete_hides_an_id_the_memtable_never_held() {
    let mut db = database();
    for id in ["x", "y", "z"] {
        db.put("c", &doc(id, "a", "one"), Durability::Fsync)
            .unwrap();
    }
    db.commit().unwrap();
    db.delete("c", "y", Durability::Fsync).unwrap();
    // An id no segment holds hides nothing.
    db.delete("c", "nope", Durability::Fsync).unwrap();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    assert_eq!(memtable.live_count(), 0);
    assert_eq!(tagged(c, &memtable, "a"), ["x", "z"]);
}

#[test]
fn a_view_hides_only_its_own_operations() {
    let mut db = database();
    db.put("c", &doc("x", "a", "one"), Durability::Fsync)
        .unwrap();
    db.put("c", &doc("y", "a", "one"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let before = c.memtable();
    db.put("c", &doc("x", "b", "two"), Durability::Fsync)
        .unwrap();
    db.delete("c", "y", Durability::Fsync).unwrap();
    let after = c.memtable();

    // The newer view first, so the snapshot caches what it hides; the older view must not see that.
    assert_eq!(tagged(c, &after, "a"), Vec::<String>::new());
    assert_eq!(tagged(c, &before, "a"), ["x", "y"]);
    assert_eq!(tagged(c, &before, "b"), Vec::<String>::new());
    assert_eq!(tagged(c, &after, "b"), ["x"]);
    // And an intermediate one extends from nothing it did not name.
    db.delete("c", "x", Durability::Fsync).unwrap();
    assert_eq!(tagged(c, &c.memtable(), "b"), Vec::<String>::new());
    assert_eq!(tagged(c, &after, "b"), ["x"]);
}

#[test]
fn ties_rank_segments_in_order_then_the_memtable() {
    let mut db = database();
    db.put("c", &doc("s1", "a", "w"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    db.put("c", &doc("s2", "a", "w"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    db.put("c", &doc("m", "a", "w"), Durability::Fsync).unwrap();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    assert_eq!(c.segments().len(), 2);
    let (_, t) = fields();
    let hits = search(c, &memtable, &term(t, "a"), 3);
    assert!(
        hits.iter()
            .all(|h| h.score.to_bits() == hits[0].score.to_bits())
    );
    assert_eq!(
        hits.iter().map(|h| h.location).collect::<Vec<_>>(),
        [
            Located::Segment { index: 0, docid: 0 },
            Located::Segment { index: 1, docid: 0 },
            Located::Memtable(0),
        ]
    );
    // A tie at the threshold: the later views cannot replace the first hit.
    assert_eq!(
        ids(c, &memtable, &search(c, &memtable, &term(t, "a"), 1)),
        ["s1"]
    );
    assert_eq!(search(c, &memtable, &term(t, "a"), 0), []);
}

#[derive(Debug, Clone)]
enum Op {
    Put { id: u8, tag: bool, words: Vec<u8> },
    Delete(u8),
    Commit,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (0..IDS, any::<bool>(), prop::collection::vec(0..WORD_COUNT, 1..5))
            .prop_map(|(id, tag, words)| Op::Put { id, tag, words }),
        2 => (0..IDS).prop_map(Op::Delete),
        1 => Just(Op::Commit),
    ]
}

fn run<S>(
    name: &'static str,
    cases: u32,
    strategy: S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) where
    S: Strategy,
    S::Value: Debug,
{
    let seed = std::env::var("PROPTEST_RNG_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(PROPTEST_SEED);
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(cases);
    let config = ProptestConfig {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        source_file: Some(file!()),
        test_name: Some(name),
        ..ProptestConfig::default()
    };
    let mut runner = TestRunner::new(config);
    if let Err(e) = runner.run(&strategy, test) {
        panic!("{name} failed with PROPTEST_RNG_SEED={seed}: {e}\n{runner}");
    }
}

/// After every operation, a search returns exactly the live ids of a model (each once), and every pruning strategy
/// the same hits; a view searched again after later writes returns what it did.
#[test]
fn searches_see_each_live_id_once() {
    let cases = if cfg!(feature = "slow") { 512 } else { 64 };
    run(
        "searches_see_each_live_id_once",
        cases,
        prop::collection::vec(op(), 1..40),
        |ops| {
            let mut db = database();
            let mut model = BTreeMap::<String, (bool, Vec<u8>)>::new();
            let mut earlier: Option<(Arc<Snapshot>, MemtableView, Vec<CollectionHit>)> = None;
            for op in &ops {
                match op {
                    Op::Put { id, tag, words } => {
                        let body: Vec<_> = words.iter().map(|&w| WORDS[w as usize]).collect();
                        let tag_s = if *tag { "a" } else { "b" };
                        let d = doc(&format!("d{id}"), tag_s, &body.join(" "));
                        db.put("c", &d, Durability::Buffered).unwrap();
                        model.insert(format!("d{id}"), (*tag, words.clone()));
                    }
                    Op::Delete(id) => {
                        db.delete("c", &format!("d{id}"), Durability::Buffered)
                            .unwrap();
                        model.remove(&format!("d{id}"));
                    }
                    Op::Commit => {
                        db.commit().unwrap();
                    }
                }
                let snapshot = db.snapshot();
                let c = snapshot.collection("c").unwrap();
                let memtable = c.memtable();
                let want = |tag: bool| -> BTreeSet<String> {
                    model
                        .iter()
                        .filter(|(_, (t, _))| *t == tag)
                        .map(|(id, _)| id.clone())
                        .collect()
                };
                for tag in [true, false] {
                    let got = tagged(c, &memtable, if tag { "a" } else { "b" });
                    let set: BTreeSet<_> = got.iter().cloned().collect();
                    prop_assert_eq!(set.len(), got.len(), "an id twice: {:?}", got);
                    prop_assert_eq!(set, want(tag));
                }
                let all = search(c, &memtable, &any_word(), 100);
                let all_ids: BTreeSet<_> = ids(c, &memtable, &all).into_iter().collect();
                prop_assert_eq!(all_ids, model.keys().cloned().collect::<BTreeSet<_>>());
                for k in [1, 2, 5] {
                    let top = search(c, &memtable, &any_word(), k);
                    prop_assert_eq!(&top[..], &all[..k.min(all.len())]);
                }
                // The view before this operation still returns what it did then.
                if let Some((snapshot, memtable, hits)) = earlier.take() {
                    let c = snapshot.collection("c").unwrap();
                    prop_assert_eq!(search(c, &memtable, &any_word(), 100), hits);
                }
                earlier = Some((Arc::clone(&snapshot), memtable, all));
            }
            Ok(())
        },
    );
}

#[test]
fn ids_first_named_after_lists_each_new_id_once() {
    let mut db = database();
    db.put("c", &doc("x", "a", "one"), Durability::Fsync)
        .unwrap(); // 1
    db.put("c", &doc("x", "a", "two"), Durability::Fsync)
        .unwrap(); // 2
    db.delete("c", "unseen", Durability::Fsync).unwrap(); // 3
    db.put("c", &doc("y", "a", "one"), Durability::Fsync)
        .unwrap(); // 4
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let view = c.memtable();
    db.put("c", &doc("z", "a", "one"), Durability::Fsync)
        .unwrap(); // 5, after the view
    db.delete("c", "x", Durability::Fsync).unwrap();
    let owned = |v: Vec<(u64, &str)>| -> Vec<(u64, String)> {
        v.into_iter().map(|(n, s)| (n, s.to_owned())).collect()
    };
    assert_eq!(view.version(), 4);
    assert_eq!(
        view.ids_first_named_after(0),
        owned(vec![(1, "x"), (3, "unseen"), (4, "y")])
    );
    assert_eq!(
        view.ids_first_named_after(1),
        owned(vec![(3, "unseen"), (4, "y")])
    );
    assert_eq!(view.ids_first_named_after(4), []);
    assert_eq!(view.ids_first_named_after(9), []);
    assert_eq!(c.memtable().ids_first_named_after(4), owned(vec![(5, "z")]));
}

/// A view of another snapshot's memtable is the caller's mistake: it must not change what correct searches of the
/// snapshot return.
#[test]
fn a_view_of_another_memtable_does_not_poison_the_cache() {
    let mut db = database();
    db.put("c", &doc("x", "a", "one"), Durability::Fsync)
        .unwrap();
    db.put("c", &doc("y", "a", "one"), Durability::Fsync)
        .unwrap();
    let before = db.snapshot();
    db.commit().unwrap();
    db.put("c", &doc("z", "a", "one"), Durability::Fsync)
        .unwrap();
    let after = db.snapshot();
    let c = after.collection("c").unwrap();
    let foreign = before.collection("c").unwrap().memtable();
    let _ = c.views(&foreign).unwrap();
    let mut got = tagged(c, &c.memtable(), "a");
    got.sort();
    assert_eq!(got, ["x", "y", "z"]);
}

/// Searches of interleaved older and newer views on several threads, while the writer keeps writing, return what each
/// view alone shows.
#[test]
fn concurrent_views_agree_with_a_fresh_snapshot() {
    let mut db = database();
    for i in 0..20 {
        db.put(
            "c",
            &doc(&format!("d{i}"), "a", "one"),
            Durability::Buffered,
        )
        .unwrap();
    }
    db.commit().unwrap();
    let shared = db.snapshot();
    let mut views = Vec::new();
    for i in 0..20 {
        let d = if i % 3 == 0 {
            db.delete("c", &format!("d{i}"), Durability::Buffered)
        } else {
            db.put(
                "c",
                &doc(&format!("d{i}"), "b", "two"),
                Durability::Buffered,
            )
        };
        d.unwrap();
        views.push(shared.collection("c").unwrap().memtable());
    }
    // After operation `i`, ids `d0..=di` are deleted or retagged.
    let want: Vec<Vec<String>> = (0..20)
        .map(|i| {
            let mut ids: Vec<_> = (i + 1..20).map(|j| format!("d{j}")).collect();
            ids.sort();
            ids
        })
        .collect();
    std::thread::scope(|s| {
        for t in 0..4 {
            let (views, want, shared) = (&views, &want, &shared);
            s.spawn(move || {
                let c = shared.collection("c").unwrap();
                for round in 0..10 {
                    for (i, v) in views
                        .iter()
                        .enumerate()
                        .rev()
                        .skip(t)
                        .step_by(1 + round % 3)
                    {
                        let mut ids = tagged(c, v, "a");
                        ids.sort();
                        assert_eq!(&ids, &want[i], "view {i}");
                    }
                }
            });
        }
        for i in 0..20 {
            db.put(
                "c",
                &doc(&format!("w{i}"), "a", "three"),
                Durability::Buffered,
            )
            .unwrap();
        }
    });
}

/// Postings of several blocks in each of three segments, with upserts and deletes in the memtable: block-max WAND
/// skips blocks under thresholds seeded from earlier views, and still returns the exhaustive hits, each live id once.
#[test]
fn multi_block_segments_and_memtable_prune_to_the_exhaustive_hits() {
    let seed: u64 = std::env::var("PERSIA_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x7406);
    let mut state = seed;
    let mut next = move |n: u64| {
        // SplitMix64: deterministic, seeded.
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % n
    };
    let body = |next: &mut dyn FnMut(u64) -> u64| -> String {
        (0..=next(6))
            .map(|_| WORDS[usize::try_from(next(WORDS.len() as u64)).unwrap()])
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut db = database();
    let mut model = BTreeSet::new();
    for segment in 0..3 {
        for i in 0..300 {
            let id = format!("s{segment}-{i}");
            db.put("c", &doc(&id, "a", &body(&mut next)), Durability::Buffered)
                .unwrap();
            model.insert(id);
        }
        db.commit().unwrap();
    }
    for _ in 0..150 {
        let id = format!("s{}-{}", next(3), next(300));
        if next(3) == 0 {
            db.delete("c", &id, Durability::Buffered).unwrap();
            model.remove(&id);
        } else {
            db.put("c", &doc(&id, "a", &body(&mut next)), Durability::Buffered)
                .unwrap();
            model.insert(id);
        }
    }
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    assert_eq!(c.segments().len(), 3);
    let memtable = c.memtable();
    let (body_field, _) = fields();
    for query in [any_word(), term(body_field, "common")] {
        let all = search(c, &memtable, &query, 2000);
        let ids_all = ids(c, &memtable, &all);
        let set: BTreeSet<_> = ids_all.iter().cloned().collect();
        assert_eq!(set.len(), ids_all.len(), "seed {seed}: an id twice");
        if matches!(query, Query::Bool(_)) {
            assert_eq!(set, model, "seed {seed}");
        }
        for k in [1, 10, 50] {
            assert_eq!(
                search(c, &memtable, &query, k),
                all[..k],
                "seed {seed}, k = {k}"
            );
        }
    }
}
