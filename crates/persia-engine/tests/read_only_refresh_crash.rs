//! Refreshing a read-only database over crash and fault images (ROADMAP 6.6.6, SPEC §4.4, §8.2.1, §8.6, ADR-0060).
//!
//! The reader keeps its handle on a `SimFile` of its own, into which each test copies the bytes a real reader's
//! handle would read at that moment (the writer's live image, a crash image, a restarted writer's file): a `SimFile`
//! clone shares the writer's fault schedule, and a crash image is an independent file, so neither models a reader
//! that keeps its handle while a new writer process recovers the file.
//!
//! Invariants:
//! 1. A writer that dies at any mutating call of a workload (a batch, an `Async` put, a flush commit, a created
//!    collection, a merge commit) leaves an image that a refresh takes without reporting corruption, and the reader
//!    then equals a fresh read-only open of the image. After a restart (the image as a process death or a power loss
//!    leaves it) and the new writer's recovery, writes and commit, every refresh again equals a fresh open, shows
//!    the new writer's documents, and keeps every write acknowledged at a level the crash keeps (SPEC §8.2.1).
//! 2. A frame written only in part at the tail (a prefix of its bytes, with or without the file already grown), and
//!    any seeded power-loss image of unsynced chunks, never fails a refresh, which equals a fresh open.
//! 3. The content-hash guard (XXH3-128, ADR-0060 item 3): a power loss drops a replayed `Async` chunk and a new
//!    writer logs another one with the same offset, length and `frame_seq`; the refresh reloads and the lost record is
//!    gone. More generally, a refresh after such a loss never shows a record of the lost history.
//! 4. A refresh whose incremental replay fails part-way returns the error and leaves the published state as it was
//!    (`seq`, replayed records, WAL end, collections, documents); views of it never see the records applied before
//!    the failure (the memtables stay held). The next refresh reloads in full and so never applies them twice.
//! 5. A power loss that drops the superblock slot the reader read (written, its `fdatasync` failed) makes a refresh
//!    fail with `RefreshRefused` (an older commit, not corruption; nothing changes) until the restarted writer's open
//!    retires the commit that slot named with one of its own (ROADMAP 6.6.9); the refresh after that one reloads.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::result_large_err,
    clippy::type_complexity,
    clippy::items_after_statements,
    missing_docs
)] // Tests.

use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseError, DatabaseOptions, ManualClock, ReadOnlyDatabase, Refreshed, SegmentIds,
    Snapshot,
};
use persia_engine::document::{Document, FieldSet, Value};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::view::SearchView;
use persia_engine::wal::{Durability, ReplayError, WalRecord, encode_chunk};
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_format::superblock::FIRST_FRAME_OFFSET;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Container, ContainerFile, FrameRef, Mode};

fn schema(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn doc(id: &str, text: &str) -> Document {
    let body = schema("x").field("body").unwrap().id();
    Document::new(id).with(body, Value::String(text.into()))
}

/// Inline merges, two segments per tier, small WAL chunks.
fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(3),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        wal_chunk_target: 256,
        ..DatabaseOptions::default()
    }
}

/// A writer with collection `a` and one committed segment.
fn base() -> (SimFile, Database<SimFile>) {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [7; 16], options()).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("s1", "seeded"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    (file, db)
}

/// What a reader's handle on the file reads now: `bytes`.
fn mirror(bytes: &[u8], reader_file: &SimFile) {
    reader_file.write_all_at(bytes, 0).unwrap();
    reader_file.set_len(bytes.len() as u64).unwrap();
}

/// A reader of its own copy of `writer_file`.
fn reader_of(writer_file: &SimFile) -> (SimFile, ReadOnlyDatabase<SimFile>) {
    let file = SimFile::from_bytes(writer_file.bytes());
    let reader = ReadOnlyDatabase::open_on(file.clone(), &AnalyzerRegistry::builtin()).unwrap();
    (file, reader)
}

fn names(s: &Snapshot) -> Vec<String> {
    s.collections().map(|(n, _)| n.to_owned()).collect()
}

/// The reader shows what a fresh read-only open of `file` shows, for every id of `ids`.
fn assert_fresh(reader: &ReadOnlyDatabase<SimFile>, file: &SimFile, ids: &[&str], ctx: &str) {
    let fresh = ReadOnlyDatabase::open_on(file.clone(), &AnalyzerRegistry::builtin())
        .unwrap_or_else(|e| panic!("{ctx}: a fresh open fails: {e:?}"));
    assert_eq!(reader.seq(), fresh.seq(), "seq: {ctx}");
    assert_eq!(*reader.manifest(), *fresh.manifest(), "manifest: {ctx}");
    assert_eq!(reader.replayed(), fresh.replayed(), "replayed: {ctx}");
    assert_eq!(reader.wal_end(), fresh.wal_end(), "WAL end: {ctx}");
    let (got, want) = (reader.snapshot(), fresh.snapshot());
    assert_eq!(names(&got), names(&want), "collections: {ctx}");
    for (name, collection) in want.collections() {
        let mine = got.collection(name).unwrap();
        let (m, w) = (mine.memtable(), collection.memtable());
        assert_eq!(m.live_count(), w.live_count(), "{name} live: {ctx}");
        assert_eq!(m.doc_count(), w.doc_count(), "{name} docs: {ctx}");
        assert_eq!(
            mine.segments().len(),
            collection.segments().len(),
            "{name} segments: {ctx}"
        );
        for id in ids {
            assert_eq!(
                mine.get(id).unwrap(),
                collection.get(id).unwrap(),
                "{name}/{id}: {ctx}"
            );
        }
    }
}

/// The reader shows the writer's `seq` and documents (the writer holds no `Buffered` record).
fn assert_writer(
    reader: &ReadOnlyDatabase<SimFile>,
    db: &Database<SimFile>,
    ids: &[&str],
    ctx: &str,
) {
    assert_eq!(reader.seq(), db.seq(), "seq against the writer: {ctx}");
    let (got, want) = (reader.snapshot(), db.snapshot());
    assert_eq!(names(&got), names(&want), "collections: {ctx}");
    for (name, collection) in want.collections() {
        let mine = got.collection(name).unwrap();
        for id in ids {
            assert_eq!(
                mine.get(id).unwrap(),
                collection.get(id).unwrap(),
                "{name}/{id} against the writer: {ctx}"
            );
        }
    }
}

/// What a failed refresh must leave as it was.
fn published(reader: &ReadOnlyDatabase<SimFile>) -> (u64, u64, u64, Vec<String>) {
    (
        reader.seq(),
        reader.replayed(),
        reader.wal_end(),
        names(&reader.snapshot()),
    )
}

/// Refreshes `reader` over what its `file` holds now: never corruption; a failure changes nothing; then the reader
/// equals a fresh open of `file`.
fn refresh(
    reader: &ReadOnlyDatabase<SimFile>,
    file: &SimFile,
    ids: &[&str],
    ctx: &str,
) -> Option<Refreshed> {
    let before = published(reader);
    match reader.refresh() {
        Ok(refreshed) => {
            assert_fresh(reader, file, ids, &format!("{ctx}, {refreshed:?}"));
            Some(refreshed)
        }
        Err(e) => {
            assert!(!e.is_corruption(), "{ctx}: reported as corruption: {e:?}");
            assert_eq!(
                published(reader),
                before,
                "{ctx}: a failed refresh changed the state"
            );
            None
        }
    }
}

/// The text of `id` in collection `name` as `reader` shows it.
fn text_of(reader: &ReadOnlyDatabase<SimFile>, name: &str, id: &str) -> Option<String> {
    let snapshot = reader.snapshot();
    let d = snapshot.collection(name)?.get(id).unwrap()?;
    match d.fields().next().map(|(_, v)| v.clone()) {
        Some(Value::String(text)) => Some(text),
        other => panic!("{name}/{id}: no body: {other:?}"),
    }
}

/// The steps of the writer workload of invariant 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// `put_batch` of `b0..b3` to `a`, `Fsync`.
    Batch,
    /// `y1` to `a`, `Async`.
    AsyncPut,
    /// A flush commit: the second segment of `a`.
    Flush,
    /// `create_collection("b")`.
    Create,
    /// `z1` to `b`, `Fsync`.
    PutB,
    /// `merge_step` until the merge of `a`'s two segments commits.
    Merge,
    /// `y2` to `a`, `Async`, after the last commit.
    AsyncTail,
}

const STEPS: [Step; 7] = [
    Step::Batch,
    Step::AsyncPut,
    Step::Flush,
    Step::Create,
    Step::PutB,
    Step::Merge,
    Step::AsyncTail,
];

const IDS: [&str; 10] = [
    "s1", "b0", "b1", "b2", "b3", "y1", "z1", "y2", "after", "missing",
];

fn run_step(db: &mut Database<SimFile>, step: Step) -> Result<(), DatabaseError> {
    match step {
        Step::Batch => {
            let docs: Vec<_> = (0..4)
                .map(|i| doc(&format!("b{i}"), &format!("batch {i}")))
                .collect();
            db.put_batch("a", &docs, Durability::Fsync)
        }
        Step::AsyncPut => db.put("a", &doc("y1", "async one"), Durability::Async),
        Step::Flush => db.commit().map(drop),
        Step::Create => db.create_collection(schema("b")).map(drop),
        Step::PutB => db.put("b", &doc("z1", "in b"), Durability::Fsync),
        Step::Merge => {
            let seq = db.seq();
            for _ in 0..4 {
                db.merge_step()?;
                if db.seq() != seq {
                    break;
                }
            }
            Ok(())
        }
        Step::AsyncTail => db.put("a", &doc("y2", "async two"), Durability::Async),
    }
}

/// The ids, by collection, a step acknowledged, and whether only a process death (not a power loss) keeps them.
fn acknowledged(step: Step) -> (&'static str, &'static [&'static str], bool) {
    match step {
        Step::Batch => ("a", &["b0", "b1", "b2", "b3"], false),
        Step::AsyncPut => ("a", &["y1"], true),
        // The flush commit made `y1` durable.
        Step::Flush => ("a", &["y1"], false),
        Step::PutB => ("b", &["z1"], false),
        Step::AsyncTail => ("a", &["y2"], true),
        Step::Create | Step::Merge => ("a", &[], false),
    }
}

/// Runs the workload with `fault` armed (none: the whole workload), refreshing a reader after each step that
/// succeeded; then the writer dies and, for `loss`, restarts. Returns the mutation count of the workload.
fn died_at(fault: Option<SimFault>, loss: CrashLoss) -> u64 {
    let ctx = format!("{fault:?}, {loss:?}");
    let (file, mut db) = base();
    let start = file.mutations();
    let (reader_file, reader) = reader_of(&file);
    file.set_fault(fault);
    let mut acked = Vec::new();
    for step in STEPS {
        let seq = db.seq();
        match run_step(&mut db, step) {
            Ok(()) => {
                if fault.is_none() && step == Step::Merge {
                    assert_ne!(db.seq(), seq, "the merge did not commit");
                }
                if !file.crashed() {
                    acked.push(step);
                }
                mirror(&file.bytes(), &reader_file);
                refresh(
                    &reader,
                    &reader_file,
                    &IDS,
                    &format!("{ctx}, after {step:?}"),
                );
            }
            Err(_) => break,
        }
    }
    let mutations = file.mutations() - start;
    drop(db);
    // The writer is gone: its last image, as the reader's handle reads it.
    mirror(&file.bytes(), &reader_file);
    refresh(
        &reader,
        &reader_file,
        &IDS,
        &format!("{ctx}, the writer died"),
    );

    // A new writer process recovers what the crash left.
    let restarted = file.crash(loss);
    let mut db = Database::open_on(restarted.clone(), options())
        .unwrap_or_else(|e| panic!("{ctx}: the restart fails: {e:?}"));
    let steps: [(&str, fn(&mut Database<SimFile>)); 3] = [
        ("recovered", |_| {}),
        ("put after the restart", |db| {
            db.put("a", &doc("after", "restarted"), Durability::Fsync)
                .unwrap();
        }),
        ("commit after the restart", |db| {
            db.commit().unwrap();
        }),
    ];
    for (what, write) in steps {
        write(&mut db);
        mirror(&restarted.bytes(), &reader_file);
        let ctx = format!("{ctx}, {what}");
        let refreshed = refresh(&reader, &reader_file, &IDS, &ctx);
        assert!(refreshed.is_some(), "{ctx}: the refresh failed");
        assert_writer(&reader, &db, &IDS, &ctx);
    }
    assert_eq!(text_of(&reader, "a", "after").as_deref(), Some("restarted"));
    for step in acked {
        let (name, ids, process_death_only) = acknowledged(step);
        if process_death_only && loss == CrashLoss::All {
            continue;
        }
        for id in ids {
            assert!(
                text_of(&reader, name, id).is_some(),
                "{ctx}: {name}/{id} acknowledged by {step:?} is lost"
            );
        }
    }
    mutations
}

#[test]
fn a_refresh_over_a_writer_that_died_at_any_call_equals_a_fresh_open() {
    let total = died_at(None, CrashLoss::Nothing);
    assert!(total > 10, "{total} mutations");
    let start = base().0.mutations();
    for k in start..start + total {
        for fault in [SimFault::FailAt(k), SimFault::ApplyThenFail(k)] {
            for loss in [CrashLoss::Nothing, CrashLoss::All] {
                died_at(Some(fault), loss);
            }
        }
    }
}

/// Byte offsets to cut the frames appended in `before.len()..after.len()` at: every byte near each frame's start and
/// end, and a stride between.
fn cuts(before: &[u8], after: &[u8]) -> Vec<usize> {
    let container =
        Container::open_on(SimFile::from_bytes(after.to_vec()), Mode::ReadOnly).unwrap();
    let mut cuts = Vec::new();
    for frame in container
        .frames()
        .iter()
        .filter(|f| f.offset >= before.len() as u64)
    {
        let (start, end) = (frame.offset as usize, frame.end() as usize);
        for cut in start..end {
            if cut - start < 48 || end - cut <= 48 || (cut - start) % 17 == 0 {
                cuts.push(cut);
            }
        }
    }
    assert!(!cuts.is_empty(), "nothing appended");
    cuts
}

/// The images a writer leaves while appending what `after` holds past `before`: a prefix of the appended bytes,
/// with the file not yet grown or grown with zeros, under `before`'s superblock slots (a commit writes its slot only
/// after its frames are synced, SPEC §4.5).
fn torn_images(before: &[u8], after: &[u8]) -> Vec<Vec<u8>> {
    let slots = FIRST_FRAME_OFFSET as usize;
    assert!(after.len() > before.len());
    assert_eq!(
        before[slots..],
        after[slots..before.len()],
        "the writer changed bytes past the slots"
    );
    let mut images = Vec::new();
    for cut in cuts(before, after) {
        let mut image = before[..slots].to_vec();
        image.extend_from_slice(&after[slots..cut]);
        images.push(image.clone());
        image.resize(after.len(), 0);
        images.push(image);
    }
    images
}

#[test]
fn a_frame_written_in_part_never_fails_a_refresh() {
    let (file, mut db) = base();
    let (reader_file, reader) = reader_of(&file);
    let ids = ["s1", "t0", "t1", "t2", "t3", "t4"];
    db.put("a", &doc("t0", "first"), Durability::Async).unwrap();
    mirror(&file.bytes(), &reader_file);
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Incremental { records: 1, .. }
    ));
    type Write = fn(&mut Database<SimFile>);
    let writes: [(&str, Write); 3] = [
        ("an async chunk", |db| {
            db.put("a", &doc("t1", "second"), Durability::Async)
                .unwrap();
        }),
        ("a synced batch", |db| {
            let docs = [doc("t2", "third"), doc("t3", "fourth")];
            db.put_batch("a", &docs, Durability::Fsync).unwrap();
        }),
        ("a flush commit", |db| {
            db.put("a", &doc("t4", "fifth"), Durability::Buffered)
                .unwrap();
            db.commit().unwrap();
        }),
    ];
    for (what, write) in writes {
        let before = file.bytes();
        write(&mut db);
        let after = file.bytes();
        for (i, image) in torn_images(&before, &after).iter().enumerate() {
            mirror(image, &reader_file);
            let ctx = format!("{what}, image {i} of {} bytes", image.len());
            assert!(
                refresh(&reader, &reader_file, &ids, &ctx).is_some(),
                "{ctx}"
            );
        }
        mirror(&after, &reader_file);
        assert!(
            refresh(&reader, &reader_file, &ids, what).is_some(),
            "{what}"
        );
        assert_writer(&reader, &db, &ids, what);
    }
}

#[test]
fn a_power_loss_image_of_unsynced_chunks_never_fails_a_refresh() {
    let (file, mut db) = base();
    let (reader_file, reader) = reader_of(&file);
    let ids = ["s1", "u0", "u1", "u2", "u3", "u4", "u5"];
    db.put("a", &doc("u0", "synced"), Durability::Fsync)
        .unwrap();
    for i in 1..6 {
        db.put("a", &doc(&format!("u{i}"), "not synced"), Durability::Async)
            .unwrap();
    }
    let live = file.bytes();
    let mut kinds = Vec::new();
    for seed in 0..64 {
        // The reader holds the unsynced chunks, then sees what a power loss leaves.
        mirror(&live, &reader_file);
        refresh(&reader, &reader_file, &ids, &format!("seed {seed}, live")).unwrap();
        assert_writer(&reader, &db, &ids, &format!("seed {seed}, live"));
        let image = file.crash(CrashLoss::Seeded(seed)).bytes();
        mirror(&image, &reader_file);
        let ctx = format!("seed {seed}, {} of {} bytes", image.len(), live.len());
        kinds.push(refresh(&reader, &reader_file, &ids, &ctx).unwrap());
        assert!(
            text_of(&reader, "a", "u0").is_some(),
            "{ctx}: a synced put is lost"
        );
    }
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, Refreshed::Reloaded { .. })),
        "no seed lost a replayed chunk: {kinds:?}"
    );
}

/// The last `WalChunk` frame `container` found.
fn last_chunk(container: &Container<SimFile>) -> FrameRef {
    *container
        .frames()
        .iter()
        .rev()
        .find(|f| f.kind == FrameKind::WalChunk)
        .unwrap()
}

/// Invariant 3, the content-hash guard (XXH3-128): the replaced chunk has the same offset, length and `frame_seq`, so
/// only its payload tells it apart. Every WAL record ends with the CRC-32C of its own bytes, so by the CRC residue
/// property the CRC-32C of a whole chunk payload depends on its header and record lengths, not on the records'
/// contents.
#[test]
fn a_chunk_a_power_loss_replaced_at_the_same_place_reloads() {
    let (file, mut db) = base();
    db.put("a", &doc("d1", "synced"), Durability::Fsync)
        .unwrap();
    let (reader_file, reader) = reader_of(&file);
    db.put("a", &doc("y2", "lost"), Durability::Async).unwrap();
    mirror(&file.bytes(), &reader_file);
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Incremental { records: 1, .. }
    ));
    assert_eq!(text_of(&reader, "a", "y2").as_deref(), Some("lost"));
    let replayed = last_chunk(&reader.container());
    drop(db);

    // The power loss drops the unsynced chunk; the new writer logs a record of the same length in its place.
    let restarted = file.crash(CrashLoss::All);
    let mut db = Database::open_on(restarted.clone(), options()).unwrap();
    db.put("a", &doc("y3", "kept"), Durability::Async).unwrap();
    mirror(&restarted.bytes(), &reader_file);
    let found = last_chunk(&Container::open_on(reader_file.clone(), Mode::ReadOnly).unwrap());
    // The same frame as far as the prefix test sees: only the payload CRC tells them apart.
    assert_eq!(found, replayed);
    assert_ne!(
        file.bytes()[replayed.offset as usize..replayed.end() as usize],
        restarted.bytes()[found.offset as usize..found.end() as usize]
    );

    let ids = ["s1", "d1", "y2", "y3"];
    let refreshed = reader.refresh().unwrap();
    let ctx = format!("after the power loss, {refreshed:?}");
    assert_fresh(&reader, &reader_file, &ids, &ctx);
    assert_writer(&reader, &db, &ids, &ctx);
    assert_eq!(text_of(&reader, "a", "y2"), None, "{ctx}");
    assert_eq!(
        text_of(&reader, "a", "y3").as_deref(),
        Some("kept"),
        "{ctx}"
    );
    assert_eq!(refreshed, Refreshed::Reloaded { seq: db.seq() });
}

/// Invariant 3 when the lost history differs before its last chunk: two replayed `Async` chunks are lost; the new
/// writer logs, in their places, a different record of the same length and then a byte-identical copy of the last
/// one. The last chunk's CRC is unchanged, so only an earlier chunk tells the histories apart.
#[test]
fn a_power_loss_that_replaced_an_earlier_chunk_never_shows_the_lost_record() {
    let (file, mut db) = base();
    db.put("a", &doc("d1", "synced"), Durability::Fsync)
        .unwrap();
    let (reader_file, reader) = reader_of(&file);
    db.put("a", &doc("y1", "lost"), Durability::Async).unwrap();
    db.put("a", &doc("y2", "same"), Durability::Async).unwrap();
    mirror(&file.bytes(), &reader_file);
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Incremental { records: 2, .. }
    ));
    let replayed: Vec<FrameRef> = reader.container().frames().to_vec();
    drop(db);

    let restarted = file.crash(CrashLoss::All);
    let mut db = Database::open_on(restarted.clone(), options()).unwrap();
    db.put("a", &doc("y1", "kept"), Durability::Async).unwrap();
    db.put("a", &doc("y2", "same"), Durability::Async).unwrap();
    mirror(&restarted.bytes(), &reader_file);
    let found = Container::open_on(reader_file.clone(), Mode::ReadOnly).unwrap();
    // The same frames; the last one byte for byte.
    assert_eq!(found.frames(), replayed.as_slice());
    let last = last_chunk(&found);
    let range = last.offset as usize..last.end() as usize;
    assert_eq!(file.bytes()[range.clone()], restarted.bytes()[range]);

    let ids = ["s1", "d1", "y1", "y2"];
    let refreshed = reader.refresh().unwrap();
    assert_writer(
        &reader,
        &db,
        &ids,
        &format!("after the power loss, {refreshed:?}"),
    );
    assert_fresh(
        &reader,
        &reader_file,
        &ids,
        &format!("after the power loss, {refreshed:?}"),
    );
    assert_eq!(text_of(&reader, "a", "y1").as_deref(), Some("kept"));
}

/// Appends a `WalChunk` of `records` to the container in `file` (whose writer is gone), with a valid frame CRC.
fn append_chunk(file: &SimFile, records: &[WalRecord<'_>]) {
    let mut container = Container::open_on(file.clone(), Mode::ReadWrite).unwrap();
    let chunk = encode_chunk(records).unwrap();
    container
        .append(FrameKind::WalChunk, &chunk, Compression::None)
        .unwrap();
    container.sync().unwrap();
}

#[test]
fn a_replay_that_fails_part_way_changes_nothing_and_the_next_refresh_reloads() {
    for create_first in [false, true] {
        let ctx = if create_first {
            "after a created collection"
        } else {
            "at the same seq"
        };
        let (file, mut db) = base();
        db.put("a", &doc("d1", "one"), Durability::Fsync).unwrap();
        let reader = ReadOnlyDatabase::open_on(file.clone(), &AnalyzerRegistry::builtin()).unwrap();
        db.put("a", &doc("d2", "two"), Durability::Fsync).unwrap();
        assert!(matches!(
            reader.refresh().unwrap(),
            Refreshed::Incremental { records: 1, .. }
        ));
        if create_first {
            // Taken by the incremental refresh before its replay fails.
            db.create_collection(schema("b")).unwrap();
        }
        drop(db);

        // A chunk whose first record applies and whose second names a collection the manifest lacks.
        let len_before = file.bytes().len() as u64;
        let encoded = doc("bad1", "applied, then undone")
            .encode(&schema("a"), FieldSet::All)
            .unwrap();
        append_chunk(
            &file,
            &[
                WalRecord::put("a", &encoded).unwrap(),
                WalRecord::delete("zz", "x").unwrap(),
            ],
        );

        let state = published(&reader);
        let view = reader.snapshot().collection("a").unwrap().memtable();
        let unknown = |e: &DatabaseError| matches!(e, DatabaseError::Replay(ReplayError::UnknownCollection { collection, .. }) if collection == "zz");
        for attempt in 0..2 {
            let ctx = format!("{ctx}, attempt {attempt}");
            let e = reader.refresh().unwrap_err();
            assert!(unknown(&e), "{ctx}: {e:?}");
            assert_eq!(published(&reader), state, "{ctx}");
            let snapshot = reader.snapshot();
            assert!(snapshot.collection("b").is_none(), "{ctx}");
            let now = snapshot.collection("a").unwrap();
            // A view taken now: the memtable stays held, at the published state.
            assert_eq!(now.memtable().live_count(), view.live_count(), "{ctx}");
            assert_eq!(now.memtable().doc_count(), view.doc_count(), "{ctx}");
            assert_eq!(now.get("bad1").unwrap(), None, "{ctx}");
            assert!(now.get("d2").unwrap().is_some(), "{ctx}");
            // A fresh open fails the same way.
            let fresh = ReadOnlyDatabase::open_on(file.clone(), &AnalyzerRegistry::builtin());
            assert!(unknown(&fresh.unwrap_err()), "{ctx}");
        }

        // The bad chunk goes: the next refresh reloads (it never takes the partly applied memtable as current).
        file.set_len(len_before).unwrap();
        let ids = ["s1", "d1", "d2", "d3", "bad1"];
        assert!(
            matches!(reader.refresh().unwrap(), Refreshed::Reloaded { .. }),
            "{ctx}"
        );
        assert_fresh(&reader, &file, &ids, ctx);
        assert_eq!(reader.snapshot().collection("b").is_some(), create_first);
        // A new writer, then an incremental refresh: still no trace of the undone record.
        let mut db = Database::open_on(file.clone(), options()).unwrap();
        db.put("a", &doc("d3", "three"), Durability::Fsync).unwrap();
        assert!(
            matches!(
                reader.refresh().unwrap(),
                Refreshed::Incremental { records: 1, .. }
            ),
            "{ctx}"
        );
        assert_fresh(&reader, &file, &ids, ctx);
        assert_writer(&reader, &db, &ids, ctx);
        assert_eq!(text_of(&reader, "a", "bad1"), None, "{ctx}");
    }
}

/// Invariant 5. The commit's last mutating call is the `fdatasync` of its superblock slot (SPEC §4.5 step 3).
#[test]
fn a_slot_a_power_loss_dropped_is_refused_until_the_restarted_writer_commits() {
    let put = |db: &mut Database<SimFile>| {
        db.put("a", &doc("d1", "lost slot"), Durability::Fsync)
            .unwrap();
    };
    let slot_sync = {
        let (file, mut db) = base();
        put(&mut db);
        db.commit().unwrap();
        file.mutations() - 1
    };
    let (file, mut db) = base();
    let (reader_file, reader) = reader_of(&file);
    put(&mut db);
    let lost = db.seq() + 1;
    file.set_fault(Some(SimFault::FailAt(slot_sync)));
    db.commit().unwrap_err();
    drop(db);
    // The slot is written but not synced: the reader reads it.
    mirror(&file.bytes(), &reader_file);
    reader.refresh().unwrap();
    assert_eq!(reader.seq(), lost);

    // A power loss drops it; the Manifest frame it named was synced before it and is still after `log_end`.
    let restarted = file.crash(CrashLoss::All);
    mirror(&restarted.bytes(), &reader_file);
    let before = published(&reader);
    let refused = reader.refresh().unwrap_err();
    assert!(
        matches!(
            refused,
            DatabaseError::RefreshRefused {
                other_database: false,
                current,
                found,
            } if current == lost && found == lost - 1
        ),
        "{refused:?}"
    );
    assert!(!refused.is_corruption());
    assert_eq!(published(&reader), before);

    // The restarted writer's open retires that Manifest with a commit of its own: the refresh follows it.
    let db = Database::open_on(restarted.clone(), options()).unwrap();
    assert_eq!(db.seq(), lost);
    mirror(&restarted.bytes(), &reader_file);
    let refreshed = reader.refresh().unwrap();
    assert_eq!(refreshed, Refreshed::Reloaded { seq: lost });
    assert_fresh(
        &reader,
        &reader_file,
        &["s1", "d1"],
        "after the retiring commit",
    );
    assert_writer(&reader, &db, &["s1", "d1"], "after the retiring commit");
    assert_eq!(text_of(&reader, "a", "d1").as_deref(), Some("lost slot"));
}
