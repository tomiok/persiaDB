//! Background merges of a database (ROADMAP 6.5.3–6.5.5; SPEC §5.5, §8.5, §8.7.2, ADR-0032).
//!
//! Invariants:
//! 1. A merge commit removes its inputs and adds one segment of their live documents: every id reads the same before
//!    and after, a reopen sees the merge, and the merged segment holds no deleted document (6.5.3).
//! 2. A merge whose input lost documents after the plan read it is discarded, never committed: deletions committed
//!    meanwhile stay (SPEC §8.7.2). The segments are planned again.
//! 3. In-flight merges never exceed the merge threads (one when inline); cancelled ones commit nothing; dropping a
//!    database joins its workers.
//! 4. A merged segment that fails `Segment::verify` is not committed, and its inputs are not planned again.
//! 5. An object a commit stops listing (a merged-away segment, a superseded or removed sidecar) is reported
//!    referenced while a snapshot holds it, and reclaimable once none does (6.5.4).
//! 6. Searches see the same hits, with the same term frequencies, positions and norms, and the same BM25 scores over
//!    collection-level statistics (SPEC §7.2), before and after merging (6.5.5).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::assert_is_empty,
    missing_docs
)] // Tests: inputs are fixed or generated.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, ManualClock, MergeOutcome, ObsoleteKind, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::manifest::{
    CollectionDiff, Location, Manifest, ManifestDiff, SegmentEntry, SegmentId,
};
use persia_engine::merge::{
    MergeError, MergeOptions, MergePolicy, MergeReason, MergeSource, merge,
};
use persia_engine::schema::{CollectionSchema, FieldId, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter, WriteOptions};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_format::ByteSource;
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::SimFile;
use persia_storage::container::{Commit, Container};
use proptest::prelude::*;

#[path = "support/split.rs"]
mod split;

const WORDS: [&str; 6] = ["alpha", "beta", "gamma", "delta", "omega", "zeta"];

fn schema() -> CollectionSchema {
    SchemaBuilder::new("a")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field("price", FieldKind::F64 { stored: true })
        .build()
        .unwrap()
}

fn field(name: &str) -> FieldId {
    schema().field(name).unwrap().id()
}

fn doc(id: &str, text: &str) -> Document {
    let price = text.len() as f64;
    Document::new(id)
        .with(field("body"), Value::String(text.into()))
        .with(field("price"), Value::F64(price))
}

/// Two segments per tier merge, so a handful of commits gives merges.
fn options(threads: usize) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(5),
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

fn database(threads: usize) -> (SimFile, Database<SimFile>) {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [9; 16], options(threads)).unwrap();
    db.create_collection(schema()).unwrap();
    (file, db)
}

/// Commits one segment per id group.
fn commit_segments(db: &mut Database<SimFile>, groups: &[&[&str]]) {
    for group in groups {
        for id in *group {
            db.put("a", &doc(id, &format!("{id} alpha")), Durability::Buffered)
                .unwrap();
        }
        db.commit().unwrap();
    }
}

/// Every id's document, as `get` reads it.
fn documents(snapshot: &Snapshot) -> BTreeMap<String, Document> {
    let a = snapshot.collection("a").unwrap();
    let mut out = BTreeMap::new();
    for view in a.segment_views() {
        for docid in 0..view.doc_count() {
            if view.is_live(docid) {
                let d = view.document(docid).unwrap();
                let previous = out.insert(d.id().to_owned(), d);
                assert!(previous.is_none(), "an id live twice");
            }
        }
    }
    for (id, d) in &out {
        assert_eq!(a.get(id).unwrap().as_ref(), Some(d));
    }
    out
}

fn segment_count(db: &Database<SimFile>) -> usize {
    db.manifest().collection("a").unwrap().segments().len()
}

fn committed(outcomes: &[MergeOutcome]) -> usize {
    outcomes
        .iter()
        .filter(|o| matches!(o, MergeOutcome::Committed { .. }))
        .count()
}

#[test]
fn inline_merges_commit_and_survive_a_reopen() {
    let (file, mut db) = database(0);
    commit_segments(
        &mut db,
        &[&["a0", "a1", "a2"], &["b0", "b1"], &["c0", "c1"], &["d0"]],
    );
    db.put("a", &doc("a1", "replaced"), Durability::Fsync)
        .unwrap();
    db.delete("a", "b0", Durability::Fsync).unwrap();
    db.commit().unwrap();
    let before = documents(&db.snapshot());

    // Inline: one merge per step, committed by it.
    let seq = db.seq();
    let first = db.merge_step().unwrap();
    assert_eq!(committed(&first), 1, "{first:?}");
    assert_eq!(db.seq(), seq + 1);
    assert_eq!(db.merges_running(), 0);
    let rest = db.finish_merges().unwrap();
    assert!(committed(&rest) >= 1, "{rest:?}");
    assert!(db.finish_merges().unwrap().is_empty());

    let after = db.snapshot();
    assert_eq!(documents(&after), before);
    let segments = db.manifest().collection("a").unwrap().segments().to_vec();
    assert_eq!(segments.len(), 1, "{segments:?}");
    assert_eq!(segments[0].tombstones, None);
    assert_eq!(segments[0].doc_count as usize, before.len());
    let a = after.collection("a").unwrap();
    assert!(a.segments()[0].merge_generation() >= 1);

    drop(db);
    let db = Database::open_on(file, options(0)).unwrap();
    assert_eq!(
        db.manifest().collection("a").unwrap().segments(),
        &segments[..]
    );
    assert_eq!(documents(&db.snapshot()), before);
}

#[test]
fn threaded_merges_reach_the_same_documents() {
    let (_, mut db) = database(2);
    commit_segments(
        &mut db,
        &[
            &["a0", "a1"],
            &["b0", "b1"],
            &["c0"],
            &["d0", "d1"],
            &["e0"],
        ],
    );
    db.delete("a", "c0", Durability::Fsync).unwrap();
    db.commit().unwrap();
    let before = documents(&db.snapshot());
    // A step starts merges, never commits one it started.
    assert!(db.merge_step().unwrap().is_empty());
    assert_eq!(db.merges_running(), 2);
    let outcomes = db.finish_merges().unwrap();
    assert!(committed(&outcomes) >= 2, "{outcomes:?}");
    assert_eq!(db.merges_running(), 0);
    assert_eq!(documents(&db.snapshot()), before);
    assert_eq!(segment_count(&db), 1);
}

#[test]
fn merges_in_flight_are_bounded_by_the_threads() {
    for threads in [1, 2, 3] {
        let (_, mut db) = database(threads);
        // Eight segments: four plans of two.
        let groups: Vec<Vec<String>> = (0..8).map(|i| vec![format!("x{i}")]).collect();
        let groups: Vec<Vec<&str>> = groups
            .iter()
            .map(|g| g.iter().map(String::as_str).collect())
            .collect();
        let groups: Vec<&[&str]> = groups.iter().map(Vec::as_slice).collect();
        commit_segments(&mut db, &groups);
        db.merge_step().unwrap();
        assert_eq!(db.merges_running(), threads);
        // A flush meanwhile is not held back, and draws an id no merge writes.
        commit_segments(&mut db, &[&["y"]]);
        db.merge_step().unwrap();
        assert!(db.merges_running() <= threads);
        db.finish_merges().unwrap();
        let ids: BTreeSet<SegmentId> = db
            .manifest()
            .collection("a")
            .unwrap()
            .segments()
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids.len(), segment_count(&db));
        assert_eq!(documents(&db.snapshot()).len(), 9);
    }
}

#[test]
fn a_merge_whose_input_lost_documents_meanwhile_is_discarded() {
    let (_, mut db) = database(1);
    commit_segments(&mut db, &[&["a0", "a1"], &["b0", "b1"]]);
    db.merge_step().unwrap();
    assert_eq!(db.merges_running(), 1);
    // The plan read `a`'s first segment without deletions; this commit gives it a sidecar.
    db.delete("a", "a0", Durability::Fsync).unwrap();
    db.commit().unwrap();
    let seq = db.seq();
    let outcomes = db.finish_merges().unwrap();
    assert!(
        matches!(&outcomes[0], MergeOutcome::Conflict { inputs, .. } if inputs.len() == 2),
        "{outcomes:?}"
    );
    // Planned again, from the segments as they are now.
    assert!(
        matches!(&outcomes[1..], [MergeOutcome::Committed { reason: MergeReason::Tier(0), seq: s, .. }] if *s == seq + 1),
        "{outcomes:?}"
    );
    let docs = documents(&db.snapshot());
    assert_eq!(
        docs.keys().map(String::as_str).collect::<Vec<_>>(),
        ["a1", "b0", "b1"]
    );
    assert_eq!(segment_count(&db), 1);
}

#[test]
fn a_merge_whose_input_a_commit_removed_is_discarded() {
    let (_, mut db) = database(1);
    commit_segments(&mut db, &[&["a0", "a1"], &["b0", "b1"]]);
    let first = db.manifest().collection("a").unwrap().segments()[0].id;
    db.merge_step().unwrap();
    assert_eq!(db.merges_running(), 1);
    // Deleting every document of an input removes it from the manifest (ROADMAP 6.4.6).
    db.delete("a", "a0", Durability::Fsync).unwrap();
    db.delete("a", "a1", Durability::Fsync).unwrap();
    db.commit().unwrap();
    assert_eq!(segment_count(&db), 1);
    // The job still reads it.
    let held = |db: &Database<SimFile>| {
        db.obsolete()
            .find(|(o, _)| o.kind == ObsoleteKind::Segment(first))
            .map(|(_, held)| held)
    };
    assert_eq!(held(&db), Some(true));
    let (seq, frames) = (db.seq(), db.container().frames().len());
    let outcomes = db.finish_merges().unwrap();
    assert!(
        matches!(&outcomes[..], [MergeOutcome::Conflict { inputs, .. }] if inputs.contains(&first)),
        "{outcomes:?}"
    );
    assert_eq!((db.seq(), db.container().frames().len()), (seq, frames));
    assert_eq!(held(&db), Some(false));
    assert_eq!(
        documents(&db.snapshot())
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["b0", "b1"]
    );
}

#[test]
fn cancelled_merges_commit_nothing() {
    let (_, mut db) = database(1);
    commit_segments(&mut db, &[&["a0"], &["b0"]]);
    let seq = db.seq();
    db.merge_step().unwrap();
    let outcomes = db.cancel_merges();
    assert!(
        matches!(&outcomes[..], [MergeOutcome::Cancelled { inputs, .. }] if inputs.len() == 2),
        "{outcomes:?}"
    );
    assert_eq!(db.merges_running(), 0);
    assert_eq!(db.seq(), seq);
    assert_eq!(segment_count(&db), 2);
    // Not a failure: the next step plans them again.
    assert_eq!(committed(&db.finish_merges().unwrap()), 1);
    assert_eq!(segment_count(&db), 1);
}

#[test]
fn dropping_a_database_joins_its_merge_workers() {
    let (file, mut db) = database(2);
    commit_segments(&mut db, &[&["a0"], &["b0"], &["c0"], &["d0"]]);
    let segments = db.manifest().collection("a").unwrap().segments().to_vec();
    db.merge_step().unwrap();
    assert_eq!(db.merges_running(), 2);
    drop(db);
    // Nothing a worker did was committed.
    let db = Database::open_on(file, options(2)).unwrap();
    assert_eq!(
        db.manifest().collection("a").unwrap().segments(),
        &segments[..]
    );
}

#[test]
fn fully_deleted_segments_are_removed_by_their_commit() {
    let (_, mut db) = database(0);
    commit_segments(&mut db, &[&["a0", "a1"]]);
    db.delete("a", "a0", Durability::Fsync).unwrap();
    db.delete("a", "a1", Durability::Fsync).unwrap();
    db.commit().unwrap();
    // The commit removed the segment (ROADMAP 6.4.6): no drop plan is left for a merge. The policy's drop plan
    // covers segments older builds kept listed (`database_fixture`, `tombstones.persia`).
    assert_eq!(segment_count(&db), 0);
    assert!(db.merge_step().unwrap().is_empty());
}

#[test]
fn obsolete_objects_wait_for_their_last_reader() {
    let (_, mut db) = database(0);
    commit_segments(&mut db, &[&["a0", "a1", "a2"], &["b0", "b1"]]);
    // A held snapshot keeps sidecar version 1 of the first segment once version 2 supersedes it.
    db.delete("a", "a0", Durability::Fsync).unwrap();
    db.commit().unwrap();
    let held = db.snapshot();
    db.delete("a", "a1", Durability::Fsync).unwrap();
    db.commit().unwrap();
    let base = db.manifest().clone();
    let entries = base.collection("a").unwrap().segments().to_vec();
    let obsolete: Vec<_> = db
        .obsolete()
        .map(|(o, referenced)| (o.kind, referenced))
        .collect();
    assert_eq!(
        obsolete,
        [(
            ObsoleteKind::Sidecar {
                segment: entries[0].id,
                version: 1
            },
            true
        )]
    );
    assert!(db.take_unreferenced().is_empty());
    drop(held);
    let free = db.take_unreferenced();
    assert_eq!(free.len(), 1);
    assert_eq!(free[0].obsolete_since, base.seq());
    assert_eq!(db.obsolete().count(), 0);

    // A merge makes both inputs and the remaining sidecar obsolete; a snapshot from before it reads them.
    let held = db.snapshot();
    let reader = db.reader();
    let outcomes = db.finish_merges().unwrap();
    assert_eq!(committed(&outcomes), 1, "{outcomes:?}");
    let mut expected = vec![
        (ObsoleteKind::Segment(entries[0].id), entries[0].location),
        (
            ObsoleteKind::Sidecar {
                segment: entries[0].id,
                version: 2,
            },
            entries[0].tombstones.unwrap().location,
        ),
        (ObsoleteKind::Segment(entries[1].id), entries[1].location),
    ];
    expected.sort_by_key(|(k, _)| format!("{k:?}"));
    let mut listed: Vec<_> = db
        .obsolete()
        .map(|(o, referenced)| {
            assert!(referenced);
            assert_eq!(o.obsolete_since, db.seq());
            (o.kind, o.location)
        })
        .collect();
    listed.sort_by_key(|(k, _)| format!("{k:?}"));
    assert_eq!(listed, expected);
    assert!(db.take_unreferenced().is_empty());
    // The reader's current snapshot is the merged one: it holds none of them.
    assert_eq!(reader.snapshot().seq(), db.seq());
    drop(held);
    assert_eq!(db.take_unreferenced().len(), 3);
}

/// A reader may keep an `Arc<Segment>` after dropping the snapshot that handed it out. The obsolete frame stays
/// referenced until that arc is gone too: deletion follows what can still read the bytes, not the snapshot value.
#[test]
fn a_cloned_segment_keeps_its_obsolete_frame_alive() {
    let (_, mut db) = database(0);
    commit_segments(&mut db, &[&["a0", "a1"], &["b0", "b1"]]);
    let held = db.snapshot();
    let segment = Arc::clone(&held.collection("a").unwrap().segments()[0]);
    let id = SegmentId::new(segment.segment_id());
    let outcomes = db.finish_merges().unwrap();
    assert_eq!(committed(&outcomes), 1, "{outcomes:?}");
    assert_eq!(segment_count(&db), 1);
    assert!(db.take_unreferenced().is_empty());

    drop(held);
    let referenced: Vec<_> = db
        .obsolete()
        .filter(|(_, referenced)| *referenced)
        .map(|(o, _)| o.kind)
        .collect();
    assert_eq!(referenced, vec![ObsoleteKind::Segment(id)]);
    let freed = db.take_unreferenced();
    assert!(
        freed.iter().all(|o| o.kind != ObsoleteKind::Segment(id)),
        "{freed:?}"
    );
    assert_eq!(freed.len(), 1, "{freed:?}");

    drop(segment);
    let freed = db.take_unreferenced();
    assert_eq!(freed.len(), 1, "{freed:?}");
    assert_eq!(freed[0].kind, ObsoleteKind::Segment(id));
    assert_eq!(db.obsolete().count(), 0);
}

/// A segment of `docs` written with id `id`.
fn segment_bytes(id: u128, docs: &[Document]) -> Vec<u8> {
    let s = schema();
    let registry = AnalyzerRegistry::builtin();
    let mut b = SegmentBuilder::new(&s, &registry).unwrap();
    for d in docs {
        b.add(d).unwrap();
    }
    let meta = SegmentMeta {
        segment_id: id,
        created_at: 0,
        merge_generation: 0,
    };
    SegmentWriter::new(&b, meta)
        .with_options(WriteOptions {
            block_size: 64,
            ..WriteOptions::default()
        })
        .finish()
        .unwrap()
}

/// Segment `id`'s bytes with damage behind valid CRCs that `open` and the merge accept, but `verify` of the merged
/// segment finds (SPEC §5.5): the first such edit of the first block-framed section, flipping one bit per byte.
fn damaged_segment(id: u128, docs: &[Document]) -> Vec<u8> {
    let s = schema();
    let registry = AnalyzerRegistry::builtin();
    let split = split::Split::new(&segment_bytes(id, docs));
    for (which, (_, _, stream, _)) in split.framed.iter().enumerate() {
        for at in 0..stream.len() {
            let mut edited = stream.clone();
            edited[at] ^= 0x01;
            let bytes = split.with_stream(which, &edited);
            let Ok(seg) = Segment::open(ByteSource::new(bytes.clone().into()), &s) else {
                continue;
            };
            let sources = [MergeSource {
                segment: &seg,
                tombstones: None,
            }];
            let Ok(Some(merged)) = merge(&s, &registry, &sources) else {
                continue;
            };
            let out = merged.write(0xfeed, 0, WriteOptions::default()).unwrap();
            if Segment::open(ByteSource::new(out.into()), &s).is_ok_and(|m| m.verify().is_err()) {
                return bytes;
            }
        }
    }
    panic!("no damage found that only the merged segment's verify detects");
}

/// A database whose collection `a` has the committed `segments` (id, bytes, documents), written by hand.
fn crafted(segments: &[(u128, Vec<u8>, u32)]) -> SimFile {
    let file = SimFile::new();
    let mut c = Container::create_on(file.clone(), [8; 16]).unwrap();
    let mut entries = Vec::new();
    for (id, bytes, doc_count) in segments {
        let f = c
            .append(FrameKind::Segment, bytes, Compression::None)
            .unwrap();
        entries.push(SegmentEntry {
            id: SegmentId::new(*id),
            doc_count: *doc_count,
            size: bytes.len() as u64,
            location: Location::Frame {
                offset: f.offset,
                len: f.len,
            },
            tombstones: None,
        });
    }
    c.sync().unwrap();
    let mut diff = ManifestDiff {
        wal_start: Some(c.end()),
        ..ManifestDiff::default()
    };
    diff.collections.insert(
        "a".into(),
        CollectionDiff::Create {
            schema: schema(),
            segments: entries,
        },
    );
    let next = Manifest::empty().apply(&diff).unwrap();
    let wal_start = c.end();
    let encoded = next.encode().unwrap();
    let m = c
        .append(FrameKind::Manifest, &encoded, Compression::None)
        .unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start,
        fence_token: next.fence_token(),
    })
    .unwrap();
    file
}

#[test]
fn a_merged_segment_that_fails_verify_is_not_committed_nor_retried() {
    let docs: Vec<Document> = (0..12)
        .map(|i| doc(&format!("x{i:02}"), WORDS[i % WORDS.len()]))
        .collect();
    let good = vec![doc("y", "beta gamma")];
    let damaged = damaged_segment(1, &docs);
    for threads in [0, 1] {
        let file = crafted(&[(1, damaged.clone(), 12), (2, segment_bytes(2, &good), 1)]);
        let mut db = Database::open_on(file, options(threads)).unwrap();
        let segments = db.manifest().collection("a").unwrap().segments().to_vec();
        // Steps, not `finish_merges`, which would never return if a failed merge were planned again.
        let mut outcomes = db.merge_step().unwrap();
        if threads > 0 {
            assert!(outcomes.is_empty());
            while db.merges_running() > 0 {
                outcomes.extend(db.merge_step().unwrap());
                std::thread::yield_now();
            }
        }
        assert!(
            matches!(
                &outcomes[..],
                [MergeOutcome::Failed { inputs, error: MergeError::Verify { source }, .. }]
                    if inputs.len() == 2 && source.is_corruption()
            ),
            "{outcomes:?}"
        );
        // Not committed, and not planned again.
        assert_eq!(
            db.manifest().collection("a").unwrap().segments(),
            &segments[..]
        );
        assert!(db.merge_step().unwrap().is_empty());
        assert_eq!(db.merges_running(), 0);
        // Writes go on.
        db.put("a", &doc("z", "delta"), Durability::Fsync).unwrap();
        db.commit().unwrap();
    }
}

// --- 6.5.5: search results survive merging --------------------------------------------------------------------

/// What a search for one term finds in one document: its frequency, positions and norm (SPEC §5.3.4).
type Hit = (u32, Vec<u32>, u8);

/// For every term of [`WORDS`], each live document holding it (by id), and its BM25 score (SPEC §7.2, one field,
/// weight 1, `k1` 1.2, `b` 0.75) over collection-level statistics: `N`, `df` and the average length counted over
/// the live documents of every segment. A deleted document counts nowhere, so the statistics are the same however
/// the documents are spread over segments. The norm byte stands in for the length: it is the quantized length, and
/// what matters here is that scoring sees the same values before and after.
fn search(snapshot: &Snapshot) -> BTreeMap<&'static str, BTreeMap<String, (Hit, f64)>> {
    let body = field("body");
    let a = snapshot.collection("a").unwrap();
    let views: Vec<_> = a.segment_views().collect();
    let mut hits: BTreeMap<&'static str, BTreeMap<String, Hit>> = BTreeMap::new();
    let (mut n, mut total_len) = (0_u64, 0_u64);
    for view in &views {
        let mut norms = view.norms(body).unwrap();
        for docid in (0..view.doc_count()).filter(|&d| view.is_live(d)) {
            n += 1;
            total_len += u64::from(norms.get(docid).unwrap());
        }
        for word in WORDS {
            let Some(mut postings) = view.postings(body, word.as_bytes()).unwrap() else {
                continue;
            };
            while let Some(docid) = postings.next().unwrap() {
                assert!(view.is_live(docid));
                let freq = postings.freq().unwrap();
                let positions = postings.positions().unwrap().to_vec();
                let hit = (freq, positions, norms.get(docid).unwrap());
                let id = view.document(docid).unwrap().id().to_owned();
                assert!(hits.entry(word).or_default().insert(id, hit).is_none());
            }
        }
    }
    let avg = if n == 0 {
        1.0
    } else {
        total_len as f64 / n as f64
    };
    let (k1, b) = (1.2, 0.75);
    hits.into_iter()
        .map(|(word, docs)| {
            let df = docs.len() as f64;
            let idf = (1.0 + (n as f64 - df + 0.5) / (df + 0.5)).ln();
            let scored = docs
                .into_iter()
                .map(|(id, hit)| {
                    let tf = f64::from(hit.0) / (1.0 - b + b * f64::from(hit.2) / avg);
                    let score = idf * tf * (k1 + 1.0) / (k1 + tf);
                    (id, (hit, score))
                })
                .collect();
            (word, scored)
        })
        .collect()
}

/// One write: a put of `id` with some words, or a delete.
#[derive(Debug, Clone)]
enum Op {
    Put(u8, Vec<u8>),
    Delete(u8),
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..16_u8, prop::collection::vec(0..WORDS.len() as u8, 0..6)).prop_map(|(id, w)| Op::Put(id, w)),
        1 => (0..16_u8).prop_map(Op::Delete),
    ]
}

fn apply(db: &mut Database<SimFile>, op: &Op) {
    match op {
        Op::Put(id, words) => {
            let text: Vec<&str> = words.iter().map(|&w| WORDS[usize::from(w)]).collect();
            db.put(
                "a",
                &doc(&format!("d{id}"), &text.join(" ")),
                Durability::Buffered,
            )
            .unwrap();
        }
        Op::Delete(id) => db
            .delete("a", &format!("d{id}"), Durability::Buffered)
            .unwrap(),
    }
}

fn assert_same_results(before: &Snapshot, after: &Snapshot) -> Result<(), TestCaseError> {
    prop_assert_eq!(documents(before), documents(after));
    let (b, a) = (search(before), search(after));
    prop_assert_eq!(
        b.iter()
            .map(|(w, d)| (
                *w,
                d.iter()
                    .map(|(id, (h, _))| (id.clone(), h.clone()))
                    .collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>(),
        a.iter()
            .map(|(w, d)| (
                *w,
                d.iter()
                    .map(|(id, (h, _))| (id.clone(), h.clone()))
                    .collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>()
    );
    for (word, docs) in &b {
        for (id, (_, score)) in docs {
            let merged = a[word][id].1;
            prop_assert!(
                (score - merged).abs() <= 1e-9 * score.abs().max(1.0),
                "{word} {id}: {score} vs {merged}"
            );
        }
    }
    Ok(())
}

/// The statistics a scorer sums over segments (SPEC §7.2): documents, documents with `body` tokens, the sum of
/// `body` lengths, and each word's `df`.
#[derive(Debug, PartialEq, Eq)]
struct Stats {
    docs: u64,
    docs_with_field: u64,
    sum_field_len: u64,
    df: BTreeMap<&'static str, u64>,
}

/// [`Stats`] as the segments store them (§5.1): deleted documents included.
fn stored_stats(snapshot: &Snapshot) -> Stats {
    let body = field("body");
    let a = snapshot.collection("a").unwrap();
    let mut stats = Stats {
        docs: 0,
        docs_with_field: 0,
        sum_field_len: 0,
        df: BTreeMap::new(),
    };
    for segment in a.segments() {
        stats.docs += u64::from(segment.doc_count());
        if let Some(f) = segment.field_stats(body) {
            stats.docs_with_field += u64::from(f.docs_with_field);
            stats.sum_field_len += f.sum_field_len;
        }
        let mut terms = segment.terms(body).unwrap();
        for word in WORDS {
            if let Some(t) = terms.get(word.as_bytes()).unwrap() {
                *stats.df.entry(word).or_default() += u64::from(t.df());
            }
        }
    }
    stats
}

/// [`Stats`] counted over the live documents only. Every `body` token is one of [`WORDS`], so a document's length
/// is the sum of its words' frequencies.
fn live_stats(snapshot: &Snapshot) -> Stats {
    let mut lengths: BTreeMap<String, u64> = BTreeMap::new();
    let mut df = BTreeMap::new();
    for (word, docs) in search(snapshot) {
        df.insert(word, docs.len() as u64);
        for (id, ((freq, _, _), _)) in docs {
            *lengths.entry(id).or_default() += u64::from(freq);
        }
    }
    Stats {
        docs: documents(snapshot).len() as u64,
        docs_with_field: lengths.len() as u64,
        sum_field_len: lengths.values().sum(),
        df,
    }
}

fn has_deletions(snapshot: &Snapshot) -> bool {
    snapshot
        .collection("a")
        .unwrap()
        .tombstones()
        .iter()
        .any(Option::is_some)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if cfg!(feature = "slow") { 256 } else { 32 },
        ..ProptestConfig::default()
    })]

    /// Commits batches of writes, then merges until the policy plans nothing, inline (checked after each merge) or
    /// on a worker. Searching gives what it gave before any merge: the same hits, frequencies, positions and norms,
    /// and the same scores over live statistics. The merged segments' stored statistics are recomputed from what
    /// they hold (ROADMAP 6.5.2.3), so with no deletions left they equal the live ones; and when no input had
    /// deletions, the stored statistics a scorer sums (SPEC §7.2), and so its scores, are unchanged by merging.
    #[test]
    fn merging_keeps_search_results(
        batches in prop::collection::vec(prop::collection::vec(arb_op(), 1..8), 2..7),
        threads in 0..2_usize,
    ) {
        let (_, mut db) = database(threads);
        for batch in &batches {
            for op in batch {
                apply(&mut db, op);
            }
            db.commit().unwrap();
        }
        let before = db.snapshot();
        let outcomes = if threads == 0 {
            let mut all = Vec::new();
            loop {
                let outcomes = db.merge_step().unwrap();
                if outcomes.is_empty() {
                    break;
                }
                assert_same_results(&before, &db.snapshot())?;
                all.extend(outcomes);
            }
            all
        } else {
            db.finish_merges().unwrap()
        };
        for outcome in &outcomes {
            prop_assert!(matches!(outcome, MergeOutcome::Committed { .. }), "{:?}", outcome);
        }
        let after = db.snapshot();
        assert_same_results(&before, &after)?;
        prop_assert_eq!(live_stats(&before), live_stats(&after));
        if !has_deletions(&after) {
            prop_assert_eq!(stored_stats(&after), live_stats(&after));
        }
        if !has_deletions(&before) {
            prop_assert_eq!(stored_stats(&before), stored_stats(&after));
        }
        // Merged down: no segment left with a deletion past the policy's threshold, none fully deleted.
        let entries = db.manifest().collection("a").unwrap().segments().to_vec();
        prop_assert!(entries.len() <= 1, "{:?}", entries);
        prop_assert!(entries
            .iter()
            .all(|e| e.tombstones.is_none_or(|t| u64::from(t.deleted) * 100 <= u64::from(e.doc_count) * 20)));
    }
}

/// The database stays shareable across threads (`Arc<RwLock<Database>>`, scoped readers) with merge workers in it.
#[test]
fn a_database_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<Database<SimFile>>();
    check::<Database<std::fs::File>>();
}
