//! The blob references GC marks (SPEC §9.5, ROADMAP 9.5.1): `Database::referenced_blobs`, against a model.
//!
//! Invariants:
//! 1. Exactly the blobs a reader or a replay can still name: those of the documents live as of the last flush
//!    commit, of every put since it (the memtable holds replaced and deleted documents too), and, for each snapshot a
//!    reader still holds, the same two sets as of that snapshot's flush epoch (its segments, and its memtable, which
//!    took writes until the next flush). Merges change none of them; a reopen replays the puts since the last commit.
//! 2. A blob only a deleted, committed, merged-away document named stops being referenced once nothing holds it.
//! 3. A poisoned database refuses: its view may not be what the disk holds.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use persia_engine::db::{Database, DatabaseOptions, ManualClock, SegmentIds, Snapshot};
use persia_engine::document::Document;
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_format::{BlobId, BlobRef};
use persia_storage::container::sim::{CrashLoss, SimFile};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(5),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        ..DatabaseOptions::default()
    }
}

fn blob(seed: u8) -> BlobRef {
    BlobRef::new(
        BlobId::from_bytes([seed; 32]),
        u64::from(seed),
        "a/b",
        64 << 10,
        BTreeMap::new(),
    )
    .unwrap()
}

fn doc(id: u8, blob_seed: Option<u8>) -> Document {
    let mut d = Document::new(format!("d{id}"));
    d.set_blob_ref(blob_seed.map(blob));
    d
}

fn ids(seeds: impl IntoIterator<Item = u8>) -> HashSet<BlobId> {
    seeds.into_iter().map(|s| blob(s).blob_id()).collect()
}

fn create() -> (SimFile, Database<SimFile>) {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [6; 16], options()).unwrap();
    db.create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    (file, db)
}

#[derive(Debug, Clone)]
enum Op {
    Put(u8, Option<u8>),
    Delete(u8),
    Commit,
    Merge,
    Hold,
    Release(usize),
    Reopen,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (0..6_u8, proptest::option::of(0..8_u8)).prop_map(|(d, b)| Op::Put(d, b)),
        2 => (0..6_u8).prop_map(Op::Delete),
        2 => Just(Op::Commit),
        1 => Just(Op::Merge),
        1 => Just(Op::Hold),
        1 => any::<usize>().prop_map(Op::Release),
        1 => Just(Op::Reopen),
    ]
}

/// What a reader holding a snapshot of a flush epoch can name: the epoch's committed live blobs and its puts.
#[derive(Debug, Clone, Default)]
struct Epoch {
    committed: BTreeMap<u8, Option<u8>>,
    puts: BTreeSet<u8>,
}

impl Epoch {
    fn blobs(&self) -> impl Iterator<Item = u8> + '_ {
        self.committed
            .values()
            .flatten()
            .copied()
            .chain(self.puts.iter().copied())
    }
}

fn run(ops: &[Op]) {
    let (mut file, mut db) = create();
    // Live documents now (memtable included), and the current epoch.
    let mut live: BTreeMap<u8, Option<u8>> = BTreeMap::new();
    let mut epoch = Arc::new(std::sync::Mutex::new(Epoch::default()));
    let mut held: Vec<(Arc<Snapshot>, Arc<std::sync::Mutex<Epoch>>)> = Vec::new();
    let mut ever = HashSet::new();
    for (step, op) in ops.iter().enumerate() {
        match op {
            Op::Put(d, b) => {
                db.put("c", &doc(*d, *b), Durability::Fsync).unwrap();
                live.insert(*d, *b);
                epoch.lock().unwrap().puts.extend(*b);
                ever.extend(ids(*b));
            }
            Op::Delete(d) => {
                db.delete("c", &format!("d{d}"), Durability::Fsync).unwrap();
                live.remove(d);
            }
            Op::Commit => {
                db.commit().unwrap();
                // Held snapshots keep their epoch (its memtable took its last write); the next one starts here.
                epoch = Arc::new(std::sync::Mutex::new(Epoch {
                    committed: live.clone(),
                    puts: BTreeSet::new(),
                }));
            }
            Op::Merge => {
                db.finish_merges().unwrap();
            }
            Op::Hold => held.push((db.snapshot(), Arc::clone(&epoch))),
            Op::Release(i) => {
                if !held.is_empty() {
                    held.remove(i % held.len());
                }
            }
            Op::Reopen => {
                held.clear();
                drop(db);
                file = file.crash(CrashLoss::Nothing);
                db = Database::open_on(file.clone(), options()).unwrap();
            }
        }
        let mut expected = ids(epoch.lock().unwrap().blobs());
        for (_, e) in &held {
            expected.extend(ids(e.lock().unwrap().blobs()));
        }
        let found = db.referenced_blobs().unwrap();
        let what = || format!("after step {step} ({op:?}) of {ops:?}");
        if held.is_empty() {
            assert_eq!(found, expected, "{}", what());
        } else {
            // A held snapshot also holds the segments a merge made obsolete, whose documents all count, deleted
            // ones included (a reader may hold one with older deletions): more, never less.
            assert!(
                found.is_superset(&expected),
                "{}: {found:?} ⊉ {expected:?}",
                what()
            );
            assert!(found.is_subset(&ever), "{}", what());
        }
    }
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 512 } else { 64 },
        rng_seed: RngSeed::Fixed(0x9_5_1),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn referenced_blobs_match_the_model(ops in proptest::collection::vec(op(), 1..40)) {
        run(&ops);
    }
}

#[test]
fn a_blob_only_a_merged_away_deleted_document_named_is_referenced_while_its_segment_is_held() {
    let (_, mut db) = create();
    db.put("c", &doc(1, Some(1)), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("c", &doc(2, Some(2)), Durability::Fsync).unwrap();
    db.commit().unwrap();
    // A reader keeps the first segment, not its snapshot.
    let segment = Arc::clone(&db.snapshot().collection("c").unwrap().segments()[0]);
    assert_eq!(db.finish_merges().unwrap().len(), 1);
    db.delete("c", "d1", Durability::Fsync).unwrap();
    db.commit().unwrap();
    // Only the obsolete segment the reader holds still shows d1 (SPEC §8.5).
    assert_eq!(db.referenced_blobs().unwrap(), ids([1, 2]));
    drop(segment);
    assert_eq!(db.referenced_blobs().unwrap(), ids([2]));
}

#[test]
fn a_poisoned_database_refuses_to_list_its_references() {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [6; 16], options()).unwrap();
    db.create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    file.set_fault(Some(persia_storage::container::sim::SimFault::FailAt(
        file.mutations(),
    )));
    assert!(db.put("c", &doc(1, Some(1)), Durability::Fsync).is_err());
    assert!(matches!(
        db.referenced_blobs(),
        Err(persia_engine::db::DatabaseError::Poisoned)
    ));
}

/// A reader may keep a collection snapshot cloned out of its snapshot, or a memtable view, or a segment's older
/// deletions, after dropping the snapshot: each still shows what it showed.
#[test]
fn held_collection_snapshots_memtable_views_and_sidecars_keep_their_blobs() {
    let (_, mut db) = create();
    // A collection snapshot cloned out, across the replacement of its document's blob (the segment stays live).
    db.put("c", &doc(0, None), Durability::Fsync).unwrap();
    db.put("c", &doc(1, Some(1)), Durability::Fsync).unwrap();
    db.commit().unwrap();
    let held = db.snapshot().collection("c").unwrap().clone();
    db.put("c", &doc(1, Some(2)), Durability::Fsync).unwrap();
    db.commit().unwrap();
    assert_eq!(db.referenced_blobs().unwrap(), ids([1, 2]));
    drop(held);
    assert_eq!(db.referenced_blobs().unwrap(), ids([2]));

    // A memtable view, across its flush and the replacement of its document's blob.
    db.put("c", &doc(1, Some(3)), Durability::Fsync).unwrap();
    let view = db.snapshot().collection("c").unwrap().memtable();
    db.commit().unwrap();
    db.put("c", &doc(1, Some(4)), Durability::Fsync).unwrap();
    db.commit().unwrap();
    assert_eq!(db.referenced_blobs().unwrap(), ids([3, 4]));
    drop(view);
    assert_eq!(db.referenced_blobs().unwrap(), ids([4]));

    // A segment's older deletions, kept with the segment while the snapshot is dropped.
    for (d, b) in [(5, 5), (6, 6), (7, 7)] {
        db.put("c", &doc(d, Some(b)), Durability::Fsync).unwrap();
    }
    db.commit().unwrap();
    db.delete("c", "d7", Durability::Fsync).unwrap();
    db.commit().unwrap();
    let (segment, deleted) = {
        let snapshot = db.snapshot();
        let c = snapshot.collection("c").unwrap();
        let last = c.segments().len() - 1;
        (
            Arc::clone(&c.segments()[last]),
            c.tombstones()[last].clone().unwrap(),
        )
    };
    db.delete("c", "d5", Durability::Fsync).unwrap();
    db.commit().unwrap();
    assert_eq!(db.referenced_blobs().unwrap(), ids([4, 5, 6]));
    drop((segment, deleted));
    assert_eq!(db.referenced_blobs().unwrap(), ids([4, 6]));
}

/// What `ReadOnlyDatabase::blob_references` lists for `verify` (SPEC §9.5, ROADMAP 9.5.3): `(collection, id, blob
/// seed, in a segment)` of each reference, in order.
fn listed(file: &SimFile) -> Vec<(String, String, u8, bool)> {
    let reader = persia_engine::db::ReadOnlyDatabase::open_on(
        file.crash(CrashLoss::Nothing),
        &persia_analysis::AnalyzerRegistry::builtin(),
    )
    .unwrap();
    let mut out = Vec::new();
    reader
        .blob_references(|r| {
            let in_segment = matches!(r.site, persia_engine::db::BlobRefSite::Segment { .. });
            let seed = r.blob_ref.blob_id().as_bytes()[0];
            out.push((r.collection, r.id, seed, in_segment));
            Ok::<_, persia_engine::db::DatabaseError>(())
        })
        .unwrap();
    out
}

/// Live segment documents as their committed tombstones leave them, then every WAL put from `wal_start` on, replaced
/// or deleted since included; documents without a reference are skipped.
#[test]
fn blob_references_lists_live_segment_documents_and_every_wal_put() {
    let (file, mut db) = create();
    db.put("c", &doc(1, Some(1)), Durability::Fsync).unwrap();
    db.put("c", &doc(2, Some(2)), Durability::Fsync).unwrap();
    db.put("c", &doc(3, None), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.delete("c", "d2", Durability::Fsync).unwrap();
    db.commit().unwrap();
    // After the last commit: only in the WAL, the replaced put too.
    db.put("c", &doc(4, Some(4)), Durability::Fsync).unwrap();
    db.put("c", &doc(4, Some(5)), Durability::Fsync).unwrap();
    db.delete("c", "d4", Durability::Fsync).unwrap();
    let found = listed(&file);
    let row = |id: &str, seed, seg| ("c".to_owned(), id.to_owned(), seed, seg);
    assert_eq!(
        found,
        [row("d1", 1, true), row("d4", 4, false), row("d4", 5, false)]
    );
}
