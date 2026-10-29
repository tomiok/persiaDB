//! Upserts and deletes across segments (ROADMAP 6.4; SPEC §8.3, §8.7.2, ADR-0030).
//!
//! Invariants:
//! 1. `CollectionSnapshot::lookup` finds an id's live version: the memtable's version or delete hides every segment's,
//!    then the segments newest first, deleted documents left out (6.4.1).
//! 2. Opening a database applies each segment's committed tombstone sidecar, and rejects as corruption a sidecar that
//!    does not decode or disagrees with its manifest entry: `segment_id`, version, `doc_count`, `deleted` (6.4.2).
//! 3. A commit tombstones every live copy, in every committed segment, of each id its memtables touched (put or
//!    delete, replayed or not), in the next sidecar version of each segment (a superset): afterwards each id has at
//!    most one live copy. Writes are visible before the commit (read-your-writes); older snapshots keep their state;
//!    a reopen after losing every unsynced write shows the same documents (6.4.3).
//!    A commit that deletes a segment's last live document removes the segment instead, with no sidecar (6.4.6).
//! 4. The newest segment whose `IdMap` holds an id decides a lookup: a deleted copy there ends it (6.4.1). A newer
//!    sidecar encoding is unsupported, not corrupt; one longer than `MAX_SIDECAR_LEN` is corrupt (6.4.2).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;

use persia_engine::db::{
    Database, DatabaseError, DatabaseOptions, Located, ManualClock, ObsoleteKind, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::manifest::{
    CollectionChange, CollectionDiff, Location, Manifest, ManifestDiff, SegmentEntry, SegmentId,
    Tombstones,
};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::segment::{SegmentBuilder, SegmentMeta, SegmentWriter};
use persia_engine::tombstone::{MAX_SIDECAR_LEN, SidecarError, TombstoneSidecar};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFile};
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
    commit_sidecar_patched(file, bytes, version, deleted, |_| {});
}

/// [`commit_sidecar`], with `patch` applied to the manifest's encoding before its CRC32C trailer is recomputed: a
/// manifest no diff can produce.
fn commit_sidecar_patched(
    file: &SimFile,
    bytes: &[u8],
    version: u64,
    deleted: u32,
    patch: impl FnOnce(&mut Vec<u8>),
) {
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
    let mut encoded = next.encode().unwrap();
    patch(&mut encoded);
    let end = encoded.len() - 4;
    let crc = persia_format::checksum::crc32c(&encoded[..end]);
    encoded[end..].copy_from_slice(&crc.to_le_bytes());
    let m = c
        .append(FrameKind::Manifest, &encoded, Compression::None)
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
    commit_sidecar(&file, &sidecar.encode().unwrap(), 1, 2);
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
        commit_sidecar(&file, &sidecar.unwrap().encode().unwrap(), version, count);
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
        .encode()
        .unwrap();
    bytes[45] ^= 0x40;
    commit_sidecar(&file, &bytes, 1, 2);
    let e = Database::open_on(file, options()).unwrap_err();
    assert!(e.is_corruption(), "{e}");
    assert!(matches!(e, DatabaseError::Sidecar { .. }), "{e}");
}

/// Ids of the live documents of collection `a`, from every segment view and the memtable, with how many live copies
/// each has (a memtable version or delete hides the segments'): a commit leaves at most one (SPEC §8.3).
fn live_copies(db: &Database<SimFile>) -> std::collections::BTreeMap<String, usize> {
    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    let mut copies = std::collections::BTreeMap::new();
    let memtable = a.memtable();
    for docid in 0..memtable.doc_count() {
        if memtable.is_live(docid) {
            *copies
                .entry(memtable.document(docid).unwrap().id().to_owned())
                .or_default() += 1;
        }
    }
    for view in a.segment_views() {
        for docid in 0..view.doc_count() {
            let id = view.segment().id_of(docid).unwrap();
            // The memtable's version or delete of an id hides the segments'.
            if view.is_live(docid) && memtable.lookup(&id).is_none() {
                *copies.entry(id).or_default() += 1;
            }
        }
    }
    copies
}

fn ids(db: &Database<SimFile>) -> Vec<String> {
    let copies = live_copies(db);
    assert!(copies.values().all(|&n| n == 1), "{copies:?}");
    copies.into_keys().collect()
}

#[test]
fn deletes_and_upserts_of_committed_documents_are_tombstoned_by_the_commit() {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [5; 16], options()).unwrap();
    db.create_collection(schema("a")).unwrap();
    for id in ["p", "q", "r"] {
        db.put("a", &doc(id, "one"), Durability::Fsync).unwrap();
    }
    db.commit().unwrap();
    let before = db.snapshot();

    // Read-your-writes: visible at once, before any commit.
    db.delete("a", "q", Durability::Fsync).unwrap();
    db.put("a", &doc("r", "two"), Durability::Fsync).unwrap();
    db.delete("a", "never", Durability::Fsync).unwrap();
    let a = db.snapshot();
    let a = a.collection("a").unwrap();
    assert_eq!(a.get("q").unwrap(), None);
    assert_eq!(text(&a.get("r").unwrap().unwrap()), "two");

    db.commit().unwrap();
    assert_eq!(ids(&db), ["p", "r"]);
    let segments = db.manifest().collection("a").unwrap().segments().to_vec();
    assert_eq!(segments.len(), 2);
    let t = segments[0].tombstones.unwrap();
    assert_eq!((t.version, t.deleted), (1, 2));
    assert_eq!(segments[1].tombstones, None);
    let a = db.snapshot();
    let a = a.collection("a").unwrap();
    assert_eq!(text(&a.get("r").unwrap().unwrap()), "two");
    // A snapshot taken before keeps its segments without the sidecar; its memtable, flushed now, shows the writes
    // (SPEC §8.4), so it too shows each id once.
    let old = before.collection("a").unwrap();
    assert!(old.tombstones()[0].is_none());
    assert_eq!(text(&old.get("r").unwrap().unwrap()), "two");
    assert_eq!(old.get("q").unwrap(), None);

    // Deleting the last live document removes the segment instead of writing a sidecar of every docid (ROADMAP
    // 6.4.6); it and its sidecar are obsolete, held by the snapshots that still list them.
    let held = db.snapshot();
    let emptied = segments[0].id;
    let frames = db.container().frames().len();
    db.delete("a", "p", Durability::Fsync).unwrap();
    db.commit().unwrap();
    let listed = db.manifest().collection("a").unwrap().segments().to_vec();
    assert_eq!(
        listed.iter().map(|e| e.id).collect::<Vec<_>>(),
        [segments[1].id]
    );
    assert!(
        db.container().frames()[frames..]
            .iter()
            .all(|f| f.kind != FrameKind::TombstoneSidecar),
        "no sidecar is written for the removed segment"
    );
    assert_eq!(ids(&db), ["r"]);
    let obsolete: Vec<_> = db
        .obsolete()
        .map(|(o, held)| (o.kind, o.obsolete_since, held))
        .collect();
    let seq = db.seq();
    assert_eq!(
        obsolete,
        [
            (ObsoleteKind::Segment(emptied), seq, true),
            (
                ObsoleteKind::Sidecar {
                    segment: emptied,
                    version: 1
                },
                seq,
                true
            ),
        ]
    );
    assert_eq!(held.collection("a").unwrap().segments().len(), 2);
    drop(held);

    // Losing every unsynced write keeps every commit.
    drop(db);
    let db = Database::open_on(file.crash(CrashLoss::All), options()).unwrap();
    assert_eq!(ids(&db), ["r"]);
    let a = db.snapshot();
    assert_eq!(
        text(&a.collection("a").unwrap().get("r").unwrap().unwrap()),
        "two"
    );
}

#[test]
fn a_replayed_delete_is_tombstoned_by_the_next_commit() {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [6; 16], options()).unwrap();
    db.create_collection(schema("a")).unwrap();
    db.put("a", &doc("p", "one"), Durability::Fsync).unwrap();
    db.put("a", &doc("q", "one"), Durability::Fsync).unwrap();
    // Stays live, so the segment keeps a sidecar rather than being removed (ROADMAP 6.4.6).
    db.put("a", &doc("r", "one"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.delete("a", "p", Durability::Fsync).unwrap();
    db.put("a", &doc("q", "two"), Durability::Fsync).unwrap();
    drop(db);

    let mut db = Database::open_on(file.crash(CrashLoss::All), options()).unwrap();
    assert_eq!(ids(&db), ["q", "r"]);
    db.commit().unwrap();
    assert_eq!(ids(&db), ["q", "r"]);
    let t = db.manifest().collection("a").unwrap().segments()[0]
        .tombstones
        .unwrap();
    assert_eq!((t.version, t.deleted), (1, 2));
    drop(db);
    let db = Database::open_on(file.crash(CrashLoss::All), options()).unwrap();
    assert_eq!(ids(&db), ["q", "r"]);
}

/// Builds a segment of documents `ids` with id `segment_id`.
fn segment_bytes(segment_id: u128, ids: &[&str]) -> Vec<u8> {
    let schema = schema("a");
    let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    for id in ids {
        b.add(&doc(id, "old")).unwrap();
    }
    let meta = SegmentMeta {
        segment_id,
        created_at: 0,
        merge_generation: 0,
    };
    SegmentWriter::new(&b, meta).finish().unwrap()
}

/// Builds before ROADMAP 6.4.3 could leave an id live in two segments (ADR-0029 Consequences): a commit that touches
/// it deletes every copy, not only the newest (ADR-0030).
#[test]
fn a_commit_deletes_every_older_copy_of_an_id() {
    let file = SimFile::new();
    {
        let mut c = Container::create_on(file.clone(), [7; 16]).unwrap();
        let mut entries = Vec::new();
        for (id, docs) in [(1, &["x", "y"][..]), (2, &["x"][..])] {
            let bytes = segment_bytes(id, docs);
            let f = c
                .append(FrameKind::Segment, &bytes, Compression::None)
                .unwrap();
            entries.push(SegmentEntry {
                id: SegmentId::new(id),
                doc_count: u32::try_from(docs.len()).unwrap(),
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
                schema: schema("a"),
                segments: entries,
            },
        );
        let m = Manifest::empty().apply(&diff).unwrap();
        let wal_start = c.end();
        let frame = c
            .append(FrameKind::Manifest, &m.encode().unwrap(), Compression::None)
            .unwrap();
        c.commit(Commit {
            manifest: frame,
            wal_start,
            fence_token: 0,
        })
        .unwrap();
    }
    let mut db = Database::open_on(file, options()).unwrap();
    assert_eq!(live_copies(&db)["x"], 2);
    db.put("a", &doc("x", "new"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    assert_eq!(ids(&db), ["x", "y"]);
    let segments = db.manifest().collection("a").unwrap().segments().to_vec();
    assert_eq!(segments[0].tombstones.map(|t| t.deleted), Some(1));
    // Segment 2 held only `x`: removed rather than fully deleted (ROADMAP 6.4.6).
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].id, SegmentId::new(1));
    assert_ne!(segments[1].id, SegmentId::new(2));
    assert_eq!(segments[1].tombstones, None);
    let a = db.snapshot();
    assert_eq!(
        text(&a.collection("a").unwrap().get("x").unwrap().unwrap()),
        "new"
    );
}

#[test]
fn a_newer_sidecar_encoding_is_unsupported_not_corrupt() {
    let (file, id) = one_segment();
    let mut bytes = TombstoneSidecar::new(id, 1, 5, deleted(&[1, 3]))
        .unwrap()
        .encode()
        .unwrap();
    bytes[4..8].copy_from_slice(&2_u32.to_le_bytes());
    let end = bytes.len() - 4;
    let crc = persia_format::checksum::crc32c(&bytes[..end]);
    bytes[end..].copy_from_slice(&crc.to_le_bytes());
    commit_sidecar(&file, &bytes, 1, 2);
    let e = Database::open_on(file, options()).unwrap_err();
    assert!(!e.is_corruption(), "{e}");
    assert!(
        matches!(
            e,
            DatabaseError::Sidecar {
                source: SidecarError::UnsupportedVersion { version: 2 },
                ..
            }
        ),
        "{e}"
    );
}

#[test]
fn an_oversized_sidecar_is_corrupt() {
    let bytes = vec![0; MAX_SIDECAR_LEN + 1];
    assert!(
        TombstoneSidecar::decode(&bytes)
            .unwrap_err()
            .is_corruption()
    );
}

/// The newest segment holding an id decides a lookup: a deleted copy there ends it, even if an older segment still
/// holds the id live (which a commit never leaves, ADR-0030).
#[test]
fn a_deleted_newest_copy_ends_the_lookup() {
    let file = SimFile::new();
    {
        let mut c = Container::create_on(file.clone(), [8; 16]).unwrap();
        let mut entries = Vec::new();
        for id in [1, 2] {
            let bytes = segment_bytes(id, &["x"]);
            let f = c
                .append(FrameKind::Segment, &bytes, Compression::None)
                .unwrap();
            entries.push(SegmentEntry {
                id: SegmentId::new(id),
                doc_count: 1,
                size: bytes.len() as u64,
                location: Location::Frame {
                    offset: f.offset,
                    len: f.len,
                },
                tombstones: None,
            });
        }
        let sidecar = TombstoneSidecar::new(SegmentId::new(2), 1, 1, deleted(&[0])).unwrap();
        let f = c
            .append(
                FrameKind::TombstoneSidecar,
                &sidecar.encode().unwrap(),
                Compression::None,
            )
            .unwrap();
        entries[1].tombstones = Some(Tombstones {
            version: 1,
            deleted: 1,
            location: Location::Frame {
                offset: f.offset,
                len: f.len,
            },
        });
        c.sync().unwrap();
        let mut diff = ManifestDiff {
            wal_start: Some(c.end()),
            ..ManifestDiff::default()
        };
        diff.collections.insert(
            "a".into(),
            CollectionDiff::Create {
                schema: schema("a"),
                segments: entries,
            },
        );
        let m = Manifest::empty().apply(&diff).unwrap();
        let wal_start = c.end();
        let frame = c
            .append(FrameKind::Manifest, &m.encode().unwrap(), Compression::None)
            .unwrap();
        c.commit(Commit {
            manifest: frame,
            wal_start,
            fence_token: 0,
        })
        .unwrap();
    }
    let db = Database::open_on(file, options()).unwrap();
    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    assert_eq!(a.lookup(&a.memtable(), "x").unwrap(), None);
    assert_eq!(a.get("x").unwrap(), None);
}

/// A commit that fails while it is planned (here: no sidecar version after `u64::MAX`) writes nothing and does not
/// poison the database (SPEC §8.2, ADR-0030 item 4).
#[test]
fn a_commit_that_cannot_be_planned_writes_nothing() {
    let (file, id) = one_segment();
    let sidecar = TombstoneSidecar::new(id, u64::MAX, 5, deleted(&[1, 3])).unwrap();
    // The manifest says version 1 (what a diff allows); patch its stored version to u64::MAX.
    commit_sidecar_patched(&file, &sidecar.encode().unwrap(), 1, 2, |m| {
        let mut entry = vec![1];
        entry.extend(1_u64.to_le_bytes());
        entry.extend(2_u32.to_le_bytes());
        let at = m.windows(entry.len()).position(|w| w == entry).unwrap();
        m[at + 1..at + 9].copy_from_slice(&u64::MAX.to_le_bytes());
    });
    let mut db = Database::open_on(file, options()).unwrap();
    let (seq, end) = (db.seq(), db.container().end());
    db.delete("a", "x0", Durability::Fsync).unwrap();
    let wal_end = db.container().end();
    let e = db.commit().unwrap_err();
    assert!(matches!(e, DatabaseError::Sidecar { .. }), "{e}");
    assert_eq!((db.seq(), db.container().end()), (seq, wal_end));
    assert!(wal_end > end);
    // Not poisoned: writes still work.
    db.put("a", &doc("fresh", "new"), Durability::Fsync)
        .unwrap();
    assert_eq!(
        text(
            &db.snapshot()
                .collection("a")
                .unwrap()
                .get("fresh")
                .unwrap()
                .unwrap()
        ),
        "new"
    );
}
