//! Upserts and deletes across segments (ROADMAP 6.4; SPEC §8.3, §8.7.2, ADR-0030).
//!
//! Invariants:
//! 1. `CollectionSnapshot::lookup` finds an id's live version: the memtable's version or delete hides every segment's,
//!    then the segments newest first, deleted documents left out (6.4.1).
//! 2. Opening a database applies each segment's committed tombstone sidecar, and rejects as corruption a sidecar that
//!    does not decode or disagrees with its manifest entry: `segment_id`, version, `doc_count`, `deleted` (6.4.2).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_engine::db::{
    Database, DatabaseError, DatabaseOptions, Located, ManualClock, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::manifest::{
    CollectionChange, CollectionDiff, Location, Manifest, ManifestDiff, SegmentId, Tombstones,
};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::tombstone::TombstoneSidecar;
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::SimFile;
use persia_storage::container::{Commit, Container, Mode};
use roaring::RoaringBitmap;

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

fn text(d: &Document) -> &str {
    let body = schema("x").field("body").unwrap().id();
    match d.get(body) {
        Some(Value::String(s)) => s,
        other => panic!("{other:?}"),
    }
}

fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    }
}

fn database() -> Database<SimFile> {
    let mut db = Database::create_on(SimFile::new(), [3; 16], options()).unwrap();
    db.create_collection(schema("a")).unwrap();
    db
}

#[test]
fn lookup_prefers_the_memtable_then_the_newest_segment() {
    let mut db = database();
    db.put("a", &doc("x", "one"), Durability::Fsync).unwrap();
    db.put("a", &doc("y", "one"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("a", &doc("x", "two"), Durability::Fsync).unwrap();
    db.commit().unwrap();

    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    let view = a.memtable();
    let x = a.lookup(&view, "x").unwrap().unwrap();
    assert!(
        matches!(x, Located::Segment { index: 1, docid: 0 }),
        "{x:?}"
    );
    assert_eq!(text(&a.get("x").unwrap().unwrap()), "two");
    assert_eq!(
        a.lookup(&view, "y").unwrap(),
        Some(Located::Segment { index: 0, docid: 1 })
    );
    assert_eq!(a.lookup(&view, "never").unwrap(), None);

    // The memtable's version and its delete hide the segments'.
    db.put("a", &doc("x", "three"), Durability::Fsync).unwrap();
    db.delete("a", "never", Durability::Fsync).unwrap();
    let a = db.snapshot();
    let a = a.collection("a").unwrap();
    let view = a.memtable();
    assert_eq!(a.lookup(&view, "x").unwrap(), Some(Located::Memtable(0)));
    assert_eq!(text(&a.get("x").unwrap().unwrap()), "three");
    assert_eq!(a.lookup(&view, "never").unwrap(), None);
    // A view taken before a write does not see it.
    let before = a.memtable();
    db.put("a", &doc("z", "four"), Durability::Fsync).unwrap();
    assert_eq!(a.lookup(&before, "z").unwrap(), None);
    assert_eq!(text(&a.get("z").unwrap().unwrap()), "four");
}

/// Appends `bytes` as a sidecar frame to a closed database's container and commits a manifest that names it as
/// version `version`, with `deleted` documents, of the first segment of collection `a`.
fn commit_sidecar(file: &SimFile, bytes: &[u8], version: u64, deleted: u32) {
    let mut c = Container::open_on(file.clone(), Mode::ReadWrite).unwrap();
    let sb = *c.superblock();
    let frame = c.read_frame(sb.manifest_off, usize::MAX).unwrap();
    let manifest = Manifest::decode(&frame.payload).unwrap();
    let f = c
        .append(FrameKind::TombstoneSidecar, bytes, Compression::None)
        .unwrap();
    c.sync().unwrap();
    let mut entry = manifest.collection("a").unwrap().segments()[0];
    entry.tombstones = Some(Tombstones {
        version,
        deleted,
        location: Location::Frame {
            offset: f.offset,
            len: f.len,
        },
    });
    let mut diff = ManifestDiff {
        wal_start: Some(c.end()),
        ..ManifestDiff::default()
    };
    diff.collections.insert(
        "a".into(),
        CollectionDiff::Change(CollectionChange {
            updated: vec![entry],
            ..CollectionChange::default()
        }),
    );
    let next = manifest.apply(&diff).unwrap();
    let wal_start = c.end();
    let m = c
        .append(
            FrameKind::Manifest,
            &next.encode().unwrap(),
            Compression::None,
        )
        .unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start,
        fence_token: next.fence_token(),
    })
    .unwrap();
}

/// A closed database with one segment of `x0`..`x4`, and the segment's id.
fn one_segment() -> (SimFile, SegmentId) {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [4; 16], options()).unwrap();
    db.create_collection(schema("a")).unwrap();
    for i in 0..5 {
        db.put("a", &doc(&format!("x{i}"), "one"), Durability::Fsync)
            .unwrap();
    }
    db.commit().unwrap();
    let id = db.manifest().collection("a").unwrap().segments()[0].id;
    (file, id)
}

fn deleted(docids: &[u32]) -> RoaringBitmap {
    docids.iter().copied().collect()
}

#[test]
fn open_applies_committed_sidecars() {
    let (file, id) = one_segment();
    let sidecar = TombstoneSidecar::new(id, 1, 5, deleted(&[1, 3])).unwrap();
    commit_sidecar(&file, &sidecar.encode(), 1, 2);
    let db = Database::open_on(file, options()).unwrap();
    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    let live: Vec<bool> = (0..5)
        .map(|i| a.get(&format!("x{i}")).unwrap().is_some())
        .collect();
    assert_eq!(live, [true, false, true, false, true]);
    assert_eq!(a.tombstones()[0].as_ref().unwrap().len(), 2);
    let view = a.segment_views().next().unwrap();
    assert!(!view.is_live(1) && view.is_live(2));
}

#[test]
fn open_rejects_sidecars_that_disagree_with_the_manifest() {
    let (_, id) = one_segment();
    let other = SegmentId::new(id.get() ^ 1);
    let cases = [
        (
            TombstoneSidecar::new(other, 1, 5, deleted(&[1, 3])),
            1,
            2,
            "segment_id",
        ),
        (
            TombstoneSidecar::new(id, 2, 5, deleted(&[1, 3])),
            1,
            2,
            "version",
        ),
        (
            TombstoneSidecar::new(id, 1, 6, deleted(&[1, 3])),
            1,
            2,
            "doc_count",
        ),
        (
            TombstoneSidecar::new(id, 1, 5, deleted(&[1, 3])),
            1,
            1,
            "deleted",
        ),
    ];
    for (sidecar, version, count, field) in cases {
        let (file, _) = one_segment();
        commit_sidecar(&file, &sidecar.unwrap().encode(), version, count);
        let e = Database::open_on(file, options()).unwrap_err();
        assert!(e.is_corruption(), "{field}: {e}");
        assert!(
            matches!(&e, DatabaseError::SidecarMismatch { field: f, .. } if *f == field),
            "{field}: {e}"
        );
    }

    // Damaged bytes are a corrupt sidecar.
    let (file, id) = one_segment();
    let mut bytes = TombstoneSidecar::new(id, 1, 5, deleted(&[1, 3]))
        .unwrap()
        .encode();
    bytes[45] ^= 0x40;
    commit_sidecar(&file, &bytes, 1, 2);
    let e = Database::open_on(file, options()).unwrap_err();
    assert!(e.is_corruption(), "{e}");
    assert!(matches!(e, DatabaseError::Sidecar { .. }), "{e}");
}
