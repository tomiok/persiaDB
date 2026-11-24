//! One block cache per database (engine side of ROADMAP 10.2.5; SPEC §10.6).
//!
//! Invariants:
//! 1. Every segment a database opens reads through the cache of its options: the segments an open recovers, the
//!    ones a flush and a merge write (the merge worker's verifying open too), and the ones a schema change opens
//!    again. So do the segments of a read-only database, at its open and after each refresh.
//! 2. By default each database has a cache of its own, of `BlockCache::DEFAULT_CAPACITY`; the same `Arc` given to
//!    several databases is shared by them.
//! 3. No segment of a database reads through the process-wide `BlockCache::shared()`: its counters do not move
//!    while the databases' do.
//!
//! Every test of this file opens segments through a database only, so the process-wide cache stays untouched by the
//! whole test binary and invariant 3 is checked exactly: its counters stay at zero.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, ManualClock, ReadOnlyDatabase, ReadOnlyOptions, Refreshed,
    SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::segment::{BlockCache, CacheStats};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn doc(id: &str) -> Document {
    let body = schema().field("body").unwrap().id();
    Document::new(id).with(body, Value::String(format!("text of {id}")))
}

/// Two segments per merge; `threads` 0 merges inline.
fn options(threads: usize) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(25),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads,
        },
        ..DatabaseOptions::default()
    }
}

/// No merges: every commit leaves its segment.
fn unmerged() -> DatabaseOptions {
    DatabaseOptions {
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: usize::MAX,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        ..options(0)
    }
}

/// Commits one segment holding `ids`.
fn commit(db: &mut Database<SimFile>, ids: &[&str]) {
    for id in ids {
        db.put("c", &doc(id), Durability::Fsync).unwrap();
    }
    db.commit().unwrap();
}

/// Invariant 1 for `snapshot`: each of its segments reads through `cache`. Returns how many it has.
fn all_read_through(snapshot: &Snapshot, cache: &Arc<BlockCache>) -> usize {
    let mut segments = 0;
    for (name, collection) in snapshot.collections() {
        for segment in collection.segments() {
            assert!(Arc::ptr_eq(segment.cache(), cache), "{name}");
            segments += 1;
        }
    }
    segments
}

/// Invariant 3: nothing in this test binary read through the process-wide cache.
fn shared_is_untouched() {
    assert_eq!(
        BlockCache::shared().stats(),
        CacheStats {
            hits: 0,
            misses: 0,
            blocks: 0,
            bytes: 0
        }
    );
}

/// The reads a cache served or decoded so far.
fn reads(stats: CacheStats) -> u64 {
    stats.hits + stats.misses
}

/// Reads every document of `ids` from the segments of `snapshot`.
fn read(snapshot: &Snapshot, ids: &[&str]) {
    let c = snapshot.collection("c").unwrap();
    for id in ids {
        assert_eq!(c.get(id).unwrap().unwrap().id(), *id);
    }
}

#[test]
fn a_database_has_a_cache_of_its_own_by_default() {
    let (a, b) = (DatabaseOptions::default(), DatabaseOptions::default());
    assert!(!Arc::ptr_eq(&a.block_cache, &b.block_cache));
    assert!(!Arc::ptr_eq(&a.block_cache, &BlockCache::shared()));
    assert_eq!(a.block_cache.capacity(), BlockCache::DEFAULT_CAPACITY);
    assert_eq!(BlockCache::DEFAULT_CAPACITY, 8 << 20);

    let mut first = Database::create_on(SimFile::new(), [1; 16], unmerged()).unwrap();
    let mut second = Database::create_on(SimFile::new(), [2; 16], unmerged()).unwrap();
    assert!(!Arc::ptr_eq(first.block_cache(), second.block_cache()));
    for db in [&mut first, &mut second] {
        db.create_collection(schema()).unwrap();
        commit(db, &["a", "b"]);
    }
    // Reading one database's segments moves its counters alone.
    let before = (first.block_cache_stats(), second.block_cache_stats());
    read(&first.snapshot(), &["a", "b"]);
    let after = first.block_cache_stats();
    assert!(reads(after) > reads(before.0), "{after:?}");
    assert!(after.blocks > 0 && after.bytes > 0);
    assert_eq!(second.block_cache_stats(), before.1);
    // A second read is served from the cache.
    read(&first.snapshot(), &["a", "b"]);
    let again = first.block_cache_stats();
    assert!(again.hits > after.hits);
    assert_eq!(again.misses, after.misses);
    assert_eq!(first.block_cache_stats(), first.block_cache().stats());
    shared_is_untouched();
}

#[test]
fn one_cache_given_to_several_databases_is_shared() {
    let cache = Arc::new(BlockCache::new(1 << 20));
    let with = || DatabaseOptions {
        block_cache: Arc::clone(&cache),
        ..unmerged()
    };
    let mut first = Database::create_on(SimFile::new(), [1; 16], with()).unwrap();
    let mut second = Database::create_on(SimFile::new(), [2; 16], with()).unwrap();
    let mut one = cache.stats();
    let mut both = one;
    for (i, db) in [&mut first, &mut second].into_iter().enumerate() {
        assert!(Arc::ptr_eq(db.block_cache(), &cache));
        db.create_collection(schema()).unwrap();
        commit(db, &["a"]);
        assert_eq!(all_read_through(&db.snapshot(), &cache), 1);
        read(&db.snapshot(), &["a"]);
        if i == 0 {
            one = cache.stats();
        } else {
            both = cache.stats();
        }
    }
    // Each database's segment has its own entries in the one cache, under the one budget: opening and reading the
    // second adds to what the first left.
    assert!(reads(both) > reads(one), "{one:?} {both:?}");
    assert!(
        both.blocks > one.blocks && both.bytes > one.bytes,
        "{one:?} {both:?}"
    );
    assert_eq!(cache.capacity(), 1 << 20);
    shared_is_untouched();
}

#[test]
fn flushed_merged_recovered_and_evolved_segments_read_through_it() {
    let file = SimFile::new();
    let cache = Arc::new(BlockCache::new(BlockCache::DEFAULT_CAPACITY));
    let with = |threads| DatabaseOptions {
        block_cache: Arc::clone(&cache),
        ..options(threads)
    };
    let mut db = Database::create_on(file.clone(), [3; 16], with(0)).unwrap();
    db.create_collection(schema()).unwrap();
    // A flush's segment.
    commit(&mut db, &["a"]);
    assert_eq!(all_read_through(&db.snapshot(), &cache), 1);
    // An inline merge's: the second commit makes two segments, which merge into one.
    commit(&mut db, &["b"]);
    db.merge_step().unwrap();
    assert_eq!(all_read_through(&db.snapshot(), &cache), 1);
    read(&db.snapshot(), &["a", "b"]);
    // A schema change opens the segments again, with the new schema.
    let evolved = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    db.evolve_collection(&evolved).unwrap();
    assert_eq!(all_read_through(&db.snapshot(), &cache), 1);
    read(&db.snapshot(), &["a", "b"]);
    drop(db);
    // An open recovers its segments through the cache of the options it is given: another one here.
    let other = Arc::new(BlockCache::new(1 << 16));
    let reopened = DatabaseOptions {
        block_cache: Arc::clone(&other),
        ..options(0)
    };
    let db = Database::open_on(file, reopened).unwrap();
    assert_eq!(all_read_through(&db.snapshot(), &other), 1);
    let before = cache.stats();
    read(&db.snapshot(), &["a", "b"]);
    assert!(other.stats().misses > 0);
    assert_eq!(
        (cache.stats().hits, cache.stats().misses),
        (before.hits, before.misses)
    );
    shared_is_untouched();
}

#[test]
fn a_merge_worker_verifies_its_output_through_it() {
    let cache = Arc::new(BlockCache::new(BlockCache::DEFAULT_CAPACITY));
    let with = DatabaseOptions {
        block_cache: Arc::clone(&cache),
        ..options(1)
    };
    let mut db = Database::create_on(SimFile::new(), [4; 16], with).unwrap();
    db.create_collection(schema()).unwrap();
    commit(&mut db, &["a"]);
    commit(&mut db, &["b"]);
    let before = cache.stats();
    // The worker merges the two segments, opens what it wrote and verifies it, on its own thread.
    db.merge_step().unwrap();
    let outcomes = db.finish_merges().unwrap();
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(db.merge_stats().committed, 1);
    let after = cache.stats();
    assert!(after.misses > before.misses, "{before:?} {after:?}");
    // The committed output is opened through it too.
    assert_eq!(all_read_through(&db.snapshot(), &cache), 1);
    read(&db.snapshot(), &["a", "b"]);
    shared_is_untouched();
}

#[test]
fn a_read_only_database_reads_through_its_cache_at_open_and_after_each_refresh() {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [5; 16], unmerged()).unwrap();
    db.create_collection(schema()).unwrap();
    commit(&mut db, &["a"]);

    // The existing opens: a cache of the reader's own, of the default capacity.
    let plain = ReadOnlyDatabase::open_on(file.clone(), &AnalyzerRegistry::builtin()).unwrap();
    assert_eq!(plain.block_cache().capacity(), BlockCache::DEFAULT_CAPACITY);
    assert!(!Arc::ptr_eq(plain.block_cache(), db.block_cache()));
    assert_eq!(all_read_through(&plain.snapshot(), plain.block_cache()), 1);

    // With options: the cache given.
    let cache = Arc::new(BlockCache::new(1 << 18));
    let options = ReadOnlyOptions::new().block_cache(Arc::clone(&cache));
    let reader = ReadOnlyDatabase::open_on_with_options(file.clone(), &options).unwrap();
    assert!(Arc::ptr_eq(reader.block_cache(), &cache));
    assert_eq!(all_read_through(&reader.snapshot(), &cache), 1);
    read(&reader.snapshot(), &["a"]);
    assert!(reader.block_cache_stats().misses > 0);
    assert_eq!(reader.block_cache_stats(), cache.stats());

    // An incremental refresh opens only the new segment: through the same cache.
    let first = reader.snapshot();
    db.put("c", &doc("w"), Durability::Fsync).unwrap();
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Incremental { records: 1, .. }
    ));
    // A flush commit reloads: every segment is opened again, through the same cache.
    commit(&mut db, &["b"]);
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Reloaded { .. }
    ));
    assert_eq!(all_read_through(&reader.snapshot(), &cache), 2);
    read(&reader.snapshot(), &["a", "b", "w"]);
    // The snapshot taken first still reads, through the cache it was opened with.
    assert_eq!(all_read_through(&first, &cache), 1);
    read(&first, &["a"]);
    // The writer's cache saw none of the reader's reads, nor the reader's the writer's.
    let writer = db.block_cache_stats();
    read(&reader.snapshot(), &["a", "b"]);
    assert_eq!(db.block_cache_stats(), writer);
    shared_is_untouched();
}

#[test]
fn an_incremental_refresh_that_takes_a_commit_opens_its_segments_through_the_cache() {
    // A reader that replayed no record takes a flush commit incrementally (ADR-0060): the segments it opens are new.
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [7; 16], unmerged()).unwrap();
    db.create_collection(schema()).unwrap();
    let cache = Arc::new(BlockCache::new(1 << 18));
    let options = ReadOnlyOptions::new().block_cache(Arc::clone(&cache));
    let reader = ReadOnlyDatabase::open_on_with_options(file, &options).unwrap();
    assert_eq!(all_read_through(&reader.snapshot(), &cache), 0);
    commit(&mut db, &["a"]);
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Incremental { .. }
    ));
    assert_eq!(all_read_through(&reader.snapshot(), &cache), 1);
    read(&reader.snapshot(), &["a"]);
    assert!(cache.stats().misses > 0);
    shared_is_untouched();
}

/// `id`'s `body` in `snapshot`, read from its segment.
fn body_of(snapshot: &Snapshot, id: &str) -> String {
    let body = schema().field("body").unwrap().id();
    let doc = snapshot.collection("c").unwrap().get(id).unwrap().unwrap();
    match doc.get(body) {
        Some(Value::String(text)) => text.clone(),
        other => panic!("{id}: {other:?}"),
    }
}

/// The ids `q` finds in `snapshot` (a plain search, so the postings are read from the segments).
fn found(snapshot: &Snapshot, q: &str) -> Vec<String> {
    use persia_engine::db::Threads;
    use persia_engine::query::QueryLimits;
    use persia_engine::search::request::{QueryRequest, QueryResults, RunOptions};
    let c = snapshot.collection("c").unwrap();
    let json = serde_json::json!({"q": q, "relax": false, "limit": 1000}).to_string();
    let request = QueryRequest::from_json(&json, c.schema(), &QueryLimits::default()).unwrap();
    let options = RunOptions::new()
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)));
    match request.run(c, &options).unwrap().results {
        QueryResults::Plain(r) => {
            let mut ids: Vec<_> = r.hits.iter().map(|h| h.document.id().to_owned()).collect();
            ids.sort();
            ids
        }
        other => panic!("{other:?}"),
    }
}

/// Invariant 2, for what the cache holds: two databases of the same id and segment-id seed write segments of the same
/// ids, and the one cache they share still keys their blocks apart. A mix-up would serve one database's bytes to the
/// other: wrong documents, without an error.
#[test]
fn a_shared_cache_keeps_the_blocks_of_its_databases_apart() {
    let cache = Arc::new(BlockCache::new(1 << 20));
    let body = schema().field("body").unwrap().id();
    let ids = ["a", "b", "c"];
    let mut dbs = Vec::new();
    for word in ["alpha", "omega"] {
        let with = DatabaseOptions {
            block_cache: Arc::clone(&cache),
            ..unmerged()
        };
        let mut db = Database::create_on(SimFile::new(), [9; 16], with).unwrap();
        db.create_collection(schema()).unwrap();
        for id in ids {
            let doc = Document::new(id).with(body, Value::String(format!("{word} {id}")));
            db.put("c", &doc, Durability::Fsync).unwrap();
        }
        db.commit().unwrap();
        dbs.push((word, db));
    }
    let segment_ids = |db: &Database<SimFile>| -> Vec<u128> {
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        c.segments().iter().map(|s| s.segment_id()).collect()
    };
    // The precondition that makes this test bite: the two segments have the same id.
    assert_eq!(segment_ids(&dbs[0].1), segment_ids(&dbs[1].1));
    assert_eq!(segment_ids(&dbs[0].1).len(), 1);
    // Interleaved, so that every read after the first round is served from blocks the other database also cached.
    let mut last = cache.stats();
    for round in 0..3 {
        for (word, db) in &dbs {
            let snapshot = db.snapshot();
            assert_eq!(all_read_through(&snapshot, &cache), 1);
            for id in ids {
                assert_eq!(body_of(&snapshot, id), format!("{word} {id}"), "{round}");
            }
            assert_eq!(found(&snapshot, word), ids, "{word} {round}");
            for (other, _) in dbs.iter().filter(|(w, _)| w != word) {
                assert!(found(&snapshot, other).is_empty(), "{word} {round}");
            }
        }
        let now = cache.stats();
        if round > 0 {
            assert!(now.hits > last.hits, "{round}: {now:?}");
        }
        last = now;
    }
    shared_is_untouched();
}

/// One budget for all the databases that share a cache (SPEC §10.6): what it keeps never goes over its capacity,
/// and reads stay right while it evicts.
#[test]
fn a_shared_cache_holds_at_most_its_capacity_for_all_its_databases() {
    let capacity = 16 << 10;
    let cache = Arc::new(BlockCache::new(capacity));
    let body = schema().field("body").unwrap().id();
    let text =
        |db: usize, i: usize| format!("database {db} document {i} {}", "padding ".repeat(i % 17));
    let mut dbs = Vec::new();
    for n in 0..3_u8 {
        let with = DatabaseOptions {
            block_cache: Arc::clone(&cache),
            ..unmerged()
        };
        let mut db = Database::create_on(SimFile::new(), [n; 16], with).unwrap();
        db.create_collection(schema()).unwrap();
        for i in 0..300 {
            let doc =
                Document::new(format!("d{i}")).with(body, Value::String(text(usize::from(n), i)));
            db.put("c", &doc, Durability::Buffered).unwrap();
        }
        db.commit().unwrap();
        dbs.push(db);
    }
    let mut misses = Vec::new();
    for round in 0..2 {
        for (n, db) in dbs.iter().enumerate() {
            let snapshot = db.snapshot();
            for i in (0..300).step_by(7) {
                assert_eq!(body_of(&snapshot, &format!("d{i}")), text(n, i), "{round}");
                let held = cache.stats();
                assert!(held.bytes <= capacity, "{round} {n} {i}: {held:?}");
            }
        }
        misses.push(cache.stats().misses);
    }
    // The second round decoded again what the budget evicted: the bound was reached, not merely respected.
    assert!(misses[1] > misses[0], "{misses:?}");
    let stats = cache.stats();
    assert!(stats.misses > 0 && stats.blocks > 0, "{stats:?}");
    shared_is_untouched();
}

/// A segment's entries are dropped with it (SPEC §10.6): a database closed gives back what it held in a cache it
/// shared, and leaves the other's entries in place.
#[test]
fn closing_a_database_frees_its_entries_in_a_shared_cache() {
    let cache = Arc::new(BlockCache::new(BlockCache::DEFAULT_CAPACITY));
    let open = |id: u8| {
        let with = DatabaseOptions {
            block_cache: Arc::clone(&cache),
            ..unmerged()
        };
        let mut db = Database::create_on(SimFile::new(), [id; 16], with).unwrap();
        db.create_collection(schema()).unwrap();
        commit(&mut db, &["a", "b", "c"]);
        read(&db.snapshot(), &["a", "b", "c"]);
        db
    };
    let first = open(1);
    let one = cache.stats();
    assert!(one.blocks > 0, "{one:?}");
    let second = open(2);
    let both = cache.stats();
    assert!(both.blocks > one.blocks, "{one:?} {both:?}");
    drop(first);
    let left = cache.stats();
    assert_eq!(
        (left.blocks, left.bytes),
        (both.blocks - one.blocks, both.bytes - one.bytes),
        "{one:?} {both:?} {left:?}"
    );
    // The second still reads, from what it left there.
    read(&second.snapshot(), &["a", "b", "c"]);
    assert!(cache.stats().hits > left.hits);
    drop(second);
    let empty = cache.stats();
    assert_eq!((empty.blocks, empty.bytes), (0, 0), "{empty:?}");
    shared_is_untouched();
}
