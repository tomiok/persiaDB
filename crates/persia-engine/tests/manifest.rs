//! The manifest model, encoding and commit planning (ROADMAP 3.3; SPEC §8.7, ADR-0024): golden fixtures, every
//! corruption rule, apply/diff rules, and properties over random commit chains.
//!
//! Invariants:
//! 1. The golden fixtures in `tests/fixtures/manifest/v1/` decode to the manifests below, which encode to exactly
//!    those bytes. They are never regenerated once released (SPEC §15); `PERSIA_WRITE_MANIFEST_FIXTURES=1` writes
//!    missing ones.
//! 2. `decode` never panics: every strict prefix and every bit flip is `Corrupt`, every rule of SPEC §8.7.1 is
//!    `Corrupt` at the offending entry, and a version above 1 is `UnsupportedVersion`.
//! 3. Whatever `decode` accepts survives `encode` → `decode` unchanged.
//! 4. `apply` builds `seq + 1` and rejects every rule of SPEC §8.7.2; `diff(a, b)` applied to `a` gives `b`.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::unnecessary_wraps
)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use persia_engine::manifest::{
    ApplyError, CollectionChange, CollectionDiff, EntryProblem, Location, MAX_MANIFEST_LEN,
    Manifest, ManifestCorruption, ManifestDiff, ManifestError, ManifestHash, SegmentEntry,
    SegmentId, Tombstones,
};
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_format::checksum::crc32c;
use proptest::prelude::*;

// ---- Builders ----

fn text() -> FieldKind {
    FieldKind::Text(TextOptions::default())
}

fn products_v1() -> CollectionSchema {
    SchemaBuilder::new("products")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                boost: 3.0,
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field(
            "brand",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                stored: true,
            }),
        )
        .build()
        .unwrap()
}

/// An evolved schema: `body` and `price` get ids 2 and 3 after `brand` 0 and `title` 1, so a stored schema without
/// its ids would read back renumbered.
fn products_v2() -> CollectionSchema {
    let proposed = SchemaBuilder::new("products")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field(
            "brand",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                stored: true,
            }),
        )
        .field("price", FieldKind::F64 { stored: true })
        .field("body", text())
        .strict(false)
        .build()
        .unwrap();
    products_v1().evolve(&proposed).unwrap()
}

fn notes() -> CollectionSchema {
    SchemaBuilder::new("notes")
        .field("body", text())
        .build()
        .unwrap()
}

fn object(id: u128, doc_count: u32) -> SegmentEntry {
    SegmentEntry {
        id: SegmentId::new(id),
        doc_count,
        size: u64::from(doc_count) * 100,
        location: Location::Object,
        tombstones: None,
    }
}

fn frame(id: u128, doc_count: u32, offset: u64, len: u64) -> SegmentEntry {
    SegmentEntry {
        location: Location::Frame { offset, len },
        ..object(id, doc_count)
    }
}

fn create(name: &str, schema: CollectionSchema, segments: Vec<SegmentEntry>) -> ManifestDiff {
    let mut diff = ManifestDiff::default();
    diff.collections
        .insert(name.into(), CollectionDiff::Create { schema, segments });
    diff
}

fn change(name: &str, change: CollectionChange) -> ManifestDiff {
    let mut diff = ManifestDiff::default();
    diff.collections
        .insert(name.into(), CollectionDiff::Change(change));
    diff
}

/// A commit after which `seq` is `seq`: `seq - 1` empty commits first.
fn at_seq(seq: u64, mut last: ManifestDiff, fence: u64, wal_start: u64) -> Manifest {
    let mut m = Manifest::empty();
    for _ in 1..seq {
        m = m.apply(&ManifestDiff::default()).unwrap();
    }
    last.fence_token = Some(fence);
    last.wal_start = Some(wal_start);
    m.apply(&last).unwrap()
}

// ---- Golden fixtures (invariant 1) ----

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/manifest/v1")
}

/// One commit with no collections.
fn golden_empty() -> Manifest {
    at_seq(1, ManifestDiff::default(), 3, 12288)
}

/// A local container: frame locations, an evolved schema, tombstones moved by a container compaction.
fn golden_local() -> Manifest {
    let base = at_seq(
        41,
        create(
            "products",
            products_v1(),
            vec![
                frame(
                    0x0123_4567_89ab_cdef_0011_2233_4455_6677,
                    1000,
                    12288,
                    81_960,
                ),
                frame(2, 7, 94_248, 1_000),
            ],
        ),
        9,
        200_000,
    );
    let base = base
        .apply(&create("notes", notes(), vec![frame(3, 1, 95_248, 64)]))
        .unwrap();
    let mut diff = change(
        "products",
        CollectionChange {
            schema: Some(products_v2()),
            updated: vec![SegmentEntry {
                tombstones: Some(Tombstones {
                    version: 1,
                    deleted: 12,
                    location: Location::Frame {
                        offset: 96_000,
                        len: 48,
                    },
                }),
                ..frame(
                    0x0123_4567_89ab_cdef_0011_2233_4455_6677,
                    1000,
                    12288,
                    81_960,
                )
            }],
            ..CollectionChange::default()
        },
    );
    diff.fence_token = Some(10);
    diff.wal_start = Some(200_048);
    base.apply(&diff).unwrap()
}

/// A remote database: object locations, a merge, every field at an extreme.
fn golden_remote() -> Manifest {
    let base = at_seq(
        1,
        create("notes", notes(), vec![object(10, 5), object(11, 6)]),
        u64::MAX,
        u64::MAX - 1,
    );
    let merged = SegmentEntry {
        id: SegmentId::new(u128::MAX),
        doc_count: 1 << 31,
        size: u64::MAX,
        location: Location::Object,
        tombstones: Some(Tombstones {
            version: 1,
            deleted: 1 << 31,
            location: Location::Object,
        }),
    };
    base.apply(&change(
        "notes",
        CollectionChange {
            removed: [SegmentId::new(10), SegmentId::new(11)].into(),
            added: vec![merged],
            ..CollectionChange::default()
        },
    ))
    .unwrap()
}

fn goldens() -> Vec<(&'static str, Manifest)> {
    vec![
        ("empty.mf", golden_empty()),
        ("local.mf", golden_local()),
        ("remote.mf", golden_remote()),
    ]
}

#[test]
fn golden_fixtures_decode_and_reencode_byte_for_byte() {
    let write = std::env::var_os("PERSIA_WRITE_MANIFEST_FIXTURES").is_some();
    for (name, manifest) in goldens() {
        let path = fixture_dir().join(name);
        let encoded = manifest.encode().unwrap();
        if write && !path.exists() {
            std::fs::create_dir_all(fixture_dir()).unwrap();
            std::fs::write(&path, &encoded).unwrap();
        }
        let stored = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (PERSIA_WRITE_MANIFEST_FIXTURES=1 writes it)",
                path.display()
            )
        });
        assert_eq!(Manifest::decode(&stored).unwrap(), manifest, "{name}");
        assert_eq!(encoded, stored, "{name}: encoding changed");
    }
}

#[test]
fn empty_golden_bytes() {
    let bytes = golden_empty().encode().unwrap();
    let mut expected = b"PMNF".to_vec();
    expected.extend(1u32.to_le_bytes()); // version
    expected.extend(1u64.to_le_bytes()); // seq
    expected.extend(3u64.to_le_bytes()); // fence_token
    expected.extend(12288u64.to_le_bytes()); // wal_start
    expected.extend(0u32.to_le_bytes()); // collection count
    let crc = crc32c(&expected);
    expected.extend(crc.to_le_bytes());
    assert_eq!(bytes, expected);
    assert_eq!(
        ManifestHash::of(&bytes),
        ManifestHash::of(&golden_empty().encode().unwrap())
    );
    assert_ne!(
        ManifestHash::of(&bytes),
        ManifestHash::of(&golden_local().encode().unwrap())
    );
}

#[test]
fn golden_manifests_hold_what_was_committed() {
    let local = golden_local();
    assert_eq!(
        (local.seq(), local.fence_token(), local.wal_start()),
        (43, 10, 200_048)
    );
    let names: Vec<_> = local.collections().map(|c| c.name().to_owned()).collect();
    assert_eq!(names, ["notes", "products"]);
    let products = local.collection("products").unwrap();
    assert_eq!(products.schema(), &products_v2());
    assert_eq!(products.schema().field("price").unwrap().id().get(), 3);
    let ids: Vec<_> = products.segments().iter().map(|s| s.id.get()).collect();
    assert_eq!(ids, [0x0123_4567_89ab_cdef_0011_2233_4455_6677, 2]);
    assert_eq!(
        products
            .segment(SegmentId::new(0x0123_4567_89ab_cdef_0011_2233_4455_6677))
            .unwrap()
            .tombstones
            .unwrap()
            .deleted,
        12
    );
    let remote = golden_remote();
    let notes = remote.collection("notes").unwrap();
    assert_eq!(notes.segments().len(), 1);
    assert_eq!(notes.segments()[0].id.to_string(), "f".repeat(32));
}

// ---- Corruption (invariant 2) ----

/// Recomputes the CRC trailer after a deliberate edit.
fn reseal(mut bytes: Vec<u8>) -> Vec<u8> {
    let end = bytes.len() - 4;
    let crc = crc32c(&bytes[..end]);
    bytes[end..].copy_from_slice(&crc.to_le_bytes());
    bytes
}

/// A sealed manifest from a hand-written body (everything after the version).
fn sealed(body: &[u8]) -> Vec<u8> {
    let mut bytes = b"PMNF".to_vec();
    bytes.extend(1u32.to_le_bytes());
    bytes.extend(body);
    bytes.extend([0; 4]);
    reseal(bytes)
}

/// `seq`, `fence_token`, `wal_start` and a collection count.
fn header(collections: u32) -> Vec<u8> {
    let mut b = Vec::new();
    for v in [1u64, 0, 0] {
        b.extend(v.to_le_bytes());
    }
    b.extend(collections.to_le_bytes());
    b
}

fn push_schema(b: &mut Vec<u8>, json: &str) {
    b.extend((json.len() as u32).to_le_bytes());
    b.extend(json.as_bytes());
}

fn notes_json() -> String {
    notes().to_json().unwrap()
}

/// One segment, as `write_segment` lays it out, with an object location and no tombstones.
fn segment_bytes(id: u128, doc_count: u32, size: u64) -> Vec<u8> {
    let mut b = id.to_le_bytes().to_vec();
    b.extend(doc_count.to_le_bytes());
    b.extend(size.to_le_bytes());
    b.push(1); // object
    b.push(0); // no tombstones
    b
}

/// A manifest with one `notes` collection holding `segments`, and the offset of the first segment.
fn with_segments(segments: &[Vec<u8>]) -> (Vec<u8>, u64) {
    let mut body = header(1);
    push_schema(&mut body, &notes_json());
    body.extend((segments.len() as u32).to_le_bytes());
    let first = 8 + body.len() as u64;
    for s in segments {
        body.extend(s);
    }
    (sealed(&body), first)
}

fn corrupt(bytes: &[u8]) -> (u64, ManifestCorruption) {
    match Manifest::decode(bytes) {
        Err(ManifestError::Corrupt { offset, reason }) => (offset, reason),
        other => panic!("expected corruption, got {other:?}"),
    }
}

#[test]
fn hand_written_body_matches_the_encoder() {
    let (bytes, _) = with_segments(&[segment_bytes(5, 3, 300)]);
    let m = Manifest::empty()
        .apply(&create("notes", notes(), vec![object(5, 3)]))
        .unwrap();
    assert_eq!(bytes, m.encode().unwrap());
}

#[test]
fn every_strict_prefix_is_corrupt() {
    for (name, m) in goldens() {
        let bytes = m.encode().unwrap();
        for len in 0..bytes.len() {
            let e = Manifest::decode(&bytes[..len]).unwrap_err();
            assert!(e.is_corruption(), "{name} prefix {len}: {e:?}");
        }
    }
}

#[test]
fn every_bit_flip_is_corrupt() {
    let bytes = golden_local().encode().unwrap();
    for i in 0..bytes.len() {
        for bit in 0..8 {
            let mut b = bytes.clone();
            b[i] ^= 1 << bit;
            let e = Manifest::decode(&b).unwrap_err();
            assert!(e.is_corruption(), "byte {i} bit {bit}: {e:?}");
        }
    }
}

#[test]
fn header_rules() {
    let good = golden_empty().encode().unwrap();
    let mut b = good.clone();
    b[0] = b'X';
    assert_eq!(corrupt(&b), (0, ManifestCorruption::Magic));
    let mut b = good.clone();
    b[good.len() - 1] ^= 1;
    assert!(matches!(
        corrupt(&b),
        (36, ManifestCorruption::ChecksumMismatch { .. })
    ));
    let mut b = good.clone();
    b[4..8].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(corrupt(&reseal(b)), (4, ManifestCorruption::ZeroVersion));
    let mut b = good.clone();
    b[4..8].copy_from_slice(&2u32.to_le_bytes());
    let e = Manifest::decode(&reseal(b)).unwrap_err();
    assert_eq!(e, ManifestError::UnsupportedVersion { version: 2 });
    assert!(!e.is_corruption());
    // A version above 1 with a bad checksum is corruption: the trailer is checked first.
    let mut b = good.clone();
    b[4..8].copy_from_slice(&2u32.to_le_bytes());
    assert!(Manifest::decode(&b).unwrap_err().is_corruption());
    assert_eq!(
        corrupt(b"PMNF\x01\x00\x00\x00"),
        (8, ManifestCorruption::UnexpectedEof)
    );
    assert_eq!(corrupt(b"PMN"), (0, ManifestCorruption::UnexpectedEof));
}

#[test]
fn trailing_bytes_and_counts_past_the_end() {
    let mut body = header(0);
    body.push(0);
    assert_eq!(
        corrupt(&sealed(&body)),
        (36, ManifestCorruption::TrailingBytes)
    );
    // A count larger than the input holds runs out of input, without allocating for it.
    assert_eq!(
        corrupt(&sealed(&header(u32::MAX))),
        (36, ManifestCorruption::UnexpectedEof)
    );
    let mut body = header(1);
    push_schema(&mut body, &notes_json());
    body.extend(u32::MAX.to_le_bytes());
    assert!(matches!(
        corrupt(&sealed(&body)),
        (_, ManifestCorruption::UnexpectedEof)
    ));
    // A schema length past the end.
    let mut body = header(1);
    body.extend(u32::MAX.to_le_bytes());
    assert_eq!(
        corrupt(&sealed(&body)),
        (40, ManifestCorruption::UnexpectedEof)
    );
}

#[test]
fn too_large_is_rejected_before_reading() {
    let bytes = vec![0; MAX_MANIFEST_LEN + 1];
    assert_eq!(
        corrupt(&bytes),
        (
            0,
            ManifestCorruption::TooLarge {
                len: MAX_MANIFEST_LEN + 1
            }
        )
    );
}

#[test]
fn schema_rules() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("not UTF-8", vec![0xff, 0xfe]),
        ("not JSON", b"{".to_vec()),
        (
            "invalid definition",
            br#"{"name":"Notes","fields":{}}"#.to_vec(),
        ),
        (
            "missing ids",
            br#"{"name":"notes","fields":{"body":{"type":"text","analyzer":"standard@1"}}}"#
                .to_vec(),
        ),
        (
            "unpinned analyzer",
            br#"{"name":"notes","fields":{"body":{"type":"text","id":0,"analyzer":"standard"}}}"#
                .to_vec(),
        ),
        (
            "default analyzer",
            br#"{"name":"notes","fields":{"body":{"type":"text","id":0}}}"#.to_vec(),
        ),
    ];
    for (what, json) in cases {
        let mut body = header(1);
        body.extend((json.len() as u32).to_le_bytes());
        body.extend(&json);
        body.extend(0u32.to_le_bytes());
        assert!(
            matches!(corrupt(&sealed(&body)), (40, ManifestCorruption::Schema(_))),
            "{what}"
        );
    }
    // Pinned ids and versions in any key order are accepted, and need not be what `to_json` writes.
    let json =
        r#"{"fields":{"body":{"analyzer":"standard@1","id":0,"type":"text"}},"name":"notes"}"#;
    let mut body = header(1);
    push_schema(&mut body, json);
    body.extend(0u32.to_le_bytes());
    let m = Manifest::decode(&sealed(&body)).unwrap();
    assert_eq!(m.collection("notes").unwrap().schema(), &notes());
    assert_eq!(Manifest::decode(&m.encode().unwrap()).unwrap(), m);
    // Schemas without fields have no ids to give.
    let empty = SchemaBuilder::new("empty").build().unwrap();
    let m = Manifest::empty()
        .apply(&create("empty", empty, vec![]))
        .unwrap();
    assert_eq!(Manifest::decode(&m.encode().unwrap()).unwrap(), m);
}

#[test]
fn collections_are_strictly_ordered() {
    for (first, second) in [("notes", "notes"), ("zeta", "alpha")] {
        let mut body = header(2);
        let mut second_at = 0;
        for name in [first, second] {
            if name == second {
                second_at = 8 + body.len() as u64;
            }
            let json = SchemaBuilder::new(name).build().unwrap().to_json().unwrap();
            push_schema(&mut body, &json);
            body.extend(0u32.to_le_bytes());
        }
        assert_eq!(
            corrupt(&sealed(&body)),
            (
                second_at,
                ManifestCorruption::CollectionOrder {
                    name: second.into()
                }
            )
        );
    }
}

#[test]
fn segment_rules() {
    let seg = segment_bytes(5, 3, 300);
    let entry = |p| ManifestCorruption::Entry(p);
    // doc_count
    for doc_count in [0, (1 << 31) + 1, u32::MAX] {
        let (bytes, at) = with_segments(&[segment_bytes(5, doc_count, 1)]);
        assert_eq!(
            corrupt(&bytes),
            (at, entry(EntryProblem::DocCount { doc_count }))
        );
    }
    let (bytes, at) = with_segments(&[segment_bytes(5, 3, 0)]);
    assert_eq!(corrupt(&bytes), (at, entry(EntryProblem::EmptySegment)));
    // Duplicate ids, within a collection and across collections.
    let (bytes, at) = with_segments(&[seg.clone(), seg.clone()]);
    assert_eq!(
        corrupt(&bytes),
        (
            at + seg.len() as u64,
            ManifestCorruption::DuplicateSegment {
                id: SegmentId::new(5)
            }
        )
    );
    let mut body = header(2);
    for name in ["alpha", "beta"] {
        let json = SchemaBuilder::new(name).build().unwrap().to_json().unwrap();
        push_schema(&mut body, &json);
        body.extend(1u32.to_le_bytes());
        body.extend(&seg);
    }
    assert!(matches!(
        corrupt(&sealed(&body)),
        (_, ManifestCorruption::DuplicateSegment { .. })
    ));
    // Location tags.
    for tag in [0u8, 3, 0xff] {
        let mut s = seg.clone();
        s[28] = tag;
        let (bytes, at) = with_segments(&[s]);
        assert_eq!(
            corrupt(&bytes),
            (at + 28, ManifestCorruption::LocationTag { tag })
        );
    }
    // Tombstone presence byte.
    for tag in [2u8, 0xff] {
        let mut s = seg.clone();
        s[29] = tag;
        let (bytes, at) = with_segments(&[s]);
        assert_eq!(
            corrupt(&bytes),
            (at + 29, ManifestCorruption::TombstonesTag { tag })
        );
    }
}

/// A segment with a frame location and optional tombstones `(version, deleted, location)`.
fn frame_segment(offset: u64, len: u64, tombstones: Option<(u64, u32, Location)>) -> Vec<u8> {
    let mut b = 5u128.to_le_bytes().to_vec();
    b.extend(10u32.to_le_bytes());
    b.extend(1000u64.to_le_bytes());
    b.push(2);
    b.extend(offset.to_le_bytes());
    b.extend(len.to_le_bytes());
    match tombstones {
        None => b.push(0),
        Some((version, deleted, location)) => {
            b.push(1);
            b.extend(version.to_le_bytes());
            b.extend(deleted.to_le_bytes());
            match location {
                Location::Object => b.push(1),
                Location::Frame { offset, len } => {
                    b.push(2);
                    b.extend(offset.to_le_bytes());
                    b.extend(len.to_le_bytes());
                }
            }
        }
    }
    b
}

#[test]
fn frame_and_tombstone_rules() {
    let entry = |p| ManifestCorruption::Entry(p);
    for (offset, len) in [
        (12280, 32),
        (12289, 32),
        (12288, 31),
        (12288, 24),
        (12288, 36),
        (u64::MAX - 7, 32),
        (0, 32),
    ] {
        let (bytes, at) = with_segments(&[frame_segment(offset, len, None)]);
        assert_eq!(
            corrupt(&bytes),
            (at, entry(EntryProblem::FrameLocation { offset, len })),
            "{offset}+{len}"
        );
    }
    let ok = with_segments(&[frame_segment(12288, 32, None)]).0;
    assert!(Manifest::decode(&ok).is_ok());
    let ok = with_segments(&[frame_segment(u64::MAX - 39, 32, None)]).0;
    assert!(Manifest::decode(&ok).is_ok());

    let cases = [
        (
            Some((0, 1, Location::Object)),
            EntryProblem::TombstoneVersion,
        ),
        (
            Some((1, 0, Location::Object)),
            EntryProblem::DeletedCount {
                deleted: 0,
                doc_count: 10,
            },
        ),
        (
            Some((1, 11, Location::Object)),
            EntryProblem::DeletedCount {
                deleted: 11,
                doc_count: 10,
            },
        ),
        (
            Some((
                1,
                1,
                Location::Frame {
                    offset: 12288,
                    len: 8,
                },
            )),
            EntryProblem::FrameLocation {
                offset: 12288,
                len: 8,
            },
        ),
    ];
    for (tombstones, problem) in cases {
        let (bytes, at) = with_segments(&[frame_segment(12288, 32, tombstones)]);
        assert_eq!(corrupt(&bytes), (at, entry(problem)), "{tombstones:?}");
    }
    let ok = with_segments(&[frame_segment(
        12288,
        32,
        Some((u64::MAX, 10, Location::Object)),
    )])
    .0;
    assert!(Manifest::decode(&ok).is_ok());
}

// ---- Apply and diff (invariant 4) ----

fn base() -> Manifest {
    let mut d = create("notes", notes(), vec![object(1, 10), object(2, 20)]);
    d.fence_token = Some(5);
    d.wal_start = Some(100);
    Manifest::empty().apply(&d).unwrap()
}

fn tombstones(version: u64, deleted: u32) -> Option<Tombstones> {
    Some(Tombstones {
        version,
        deleted,
        location: Location::Object,
    })
}

fn with_tombstones(id: u128, doc_count: u32, version: u64, deleted: u32) -> SegmentEntry {
    SegmentEntry {
        tombstones: tombstones(version, deleted),
        ..object(id, doc_count)
    }
}

fn apply_err(m: &Manifest, d: &ManifestDiff) -> ApplyError {
    m.apply(d).unwrap_err()
}

#[test]
fn apply_advances_seq_and_keeps_unset_fields() {
    let b = base();
    assert_eq!((b.seq(), b.fence_token(), b.wal_start()), (1, 5, 100));
    let next = b.apply(&ManifestDiff::default()).unwrap();
    assert_eq!(
        (next.seq(), next.fence_token(), next.wal_start()),
        (2, 5, 100)
    );
    assert_eq!(next.collection("notes"), b.collection("notes"));
}

#[test]
fn apply_rejects_regressions() {
    let b = base();
    let d = ManifestDiff {
        fence_token: Some(4),
        ..ManifestDiff::default()
    };
    assert_eq!(
        apply_err(&b, &d),
        ApplyError::FenceRegressed {
            current: 5,
            proposed: 4
        }
    );
    let d = ManifestDiff {
        wal_start: Some(99),
        ..ManifestDiff::default()
    };
    assert_eq!(
        apply_err(&b, &d),
        ApplyError::WalRegressed {
            current: 100,
            proposed: 99
        }
    );
}

#[test]
fn apply_collection_rules() {
    let b = base();
    assert_eq!(
        apply_err(&b, &create("notes", notes(), vec![])),
        ApplyError::CollectionExists {
            name: "notes".into()
        }
    );
    assert_eq!(
        apply_err(&b, &create("other", notes(), vec![])),
        ApplyError::SchemaName {
            collection: "other".into(),
            schema: "notes".into()
        }
    );
    let mut drop = ManifestDiff::default();
    drop.collections.insert("nope".into(), CollectionDiff::Drop);
    assert_eq!(
        apply_err(&b, &drop),
        ApplyError::NoSuchCollection {
            name: "nope".into()
        }
    );
    assert_eq!(
        apply_err(&b, &change("nope", CollectionChange::default())),
        ApplyError::NoSuchCollection {
            name: "nope".into()
        }
    );
    let mut drop = ManifestDiff::default();
    drop.collections
        .insert("notes".into(), CollectionDiff::Drop);
    let dropped = b.apply(&drop).unwrap();
    assert_eq!(dropped.collections().len(), 0);
    // Ids stay taken for the whole commit: a segment of a collection dropped in the same diff is still in the base.
    let mut d = create(
        "other",
        SchemaBuilder::new("other").build().unwrap(),
        vec![object(1, 1)],
    );
    d.collections.insert("notes".into(), CollectionDiff::Drop);
    assert_eq!(
        apply_err(&b, &d),
        ApplyError::DuplicateSegment {
            id: SegmentId::new(1)
        }
    );
}

#[test]
fn apply_schema_rules() {
    let b = base();
    let renamed = SchemaBuilder::new("other").build().unwrap();
    assert!(matches!(
        apply_err(
            &b,
            &change(
                "notes",
                CollectionChange {
                    schema: Some(renamed),
                    ..CollectionChange::default()
                }
            )
        ),
        ApplyError::SchemaName { .. }
    ));
    let retyped = SchemaBuilder::new("notes")
        .field("body", FieldKind::Stored)
        .build()
        .unwrap();
    assert!(matches!(
        apply_err(
            &b,
            &change(
                "notes",
                CollectionChange {
                    schema: Some(retyped),
                    ..CollectionChange::default()
                }
            )
        ),
        ApplyError::Evolution { .. }
    ));
    // Adding "a" before "body" in byte order renumbers "body" unless evolved.
    let renumbered = SchemaBuilder::new("notes")
        .field("a", text())
        .field("body", text())
        .build()
        .unwrap();
    assert_eq!(
        apply_err(
            &b,
            &change(
                "notes",
                CollectionChange {
                    schema: Some(renumbered.clone()),
                    ..CollectionChange::default()
                }
            )
        ),
        ApplyError::NotEvolved {
            collection: "notes".into()
        }
    );
    let evolved = notes().evolve(&renumbered).unwrap();
    let next = b
        .apply(&change(
            "notes",
            CollectionChange {
                schema: Some(evolved.clone()),
                ..CollectionChange::default()
            },
        ))
        .unwrap();
    assert_eq!(next.collection("notes").unwrap().schema(), &evolved);
}

#[test]
fn apply_segment_rules() {
    let b = base();
    let ch = |c: CollectionChange| change("notes", c);
    let removed = |ids: &[u128]| {
        ids.iter()
            .map(|i| SegmentId::new(*i))
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        apply_err(
            &b,
            &ch(CollectionChange {
                removed: removed(&[9]),
                ..CollectionChange::default()
            })
        ),
        ApplyError::NoSuchSegment {
            collection: "notes".into(),
            id: SegmentId::new(9)
        }
    );
    assert_eq!(
        apply_err(
            &b,
            &ch(CollectionChange {
                updated: vec![object(9, 1)],
                ..CollectionChange::default()
            })
        ),
        ApplyError::NoSuchSegment {
            collection: "notes".into(),
            id: SegmentId::new(9)
        }
    );
    let dup = |c| apply_err(&b, &ch(c));
    for c in [
        CollectionChange {
            added: vec![object(1, 10)],
            ..CollectionChange::default()
        },
        CollectionChange {
            removed: removed(&[1]),
            added: vec![object(1, 10)],
            ..CollectionChange::default()
        },
        CollectionChange {
            added: vec![object(3, 1), object(3, 1)],
            ..CollectionChange::default()
        },
        CollectionChange {
            removed: removed(&[1]),
            updated: vec![object(1, 10)],
            ..CollectionChange::default()
        },
        CollectionChange {
            updated: vec![object(1, 10), object(1, 10)],
            ..CollectionChange::default()
        },
    ] {
        assert!(matches!(dup(c), ApplyError::DuplicateSegment { .. }));
    }
    // Ids are unique across collections too.
    let mut d = create(
        "other",
        SchemaBuilder::new("other").build().unwrap(),
        vec![object(3, 1)],
    );
    d.collections.insert(
        "notes".into(),
        CollectionDiff::Change(CollectionChange {
            added: vec![object(3, 1)],
            ..CollectionChange::default()
        }),
    );
    assert_eq!(
        apply_err(&b, &d),
        ApplyError::DuplicateSegment {
            id: SegmentId::new(3)
        }
    );
    // Entries are checked.
    assert_eq!(
        apply_err(
            &b,
            &ch(CollectionChange {
                added: vec![object(3, 0)],
                ..CollectionChange::default()
            })
        ),
        ApplyError::Entry {
            id: SegmentId::new(3),
            problem: EntryProblem::DocCount { doc_count: 0 }
        }
    );
    assert!(matches!(
        apply_err(
            &b,
            &create(
                "x",
                SchemaBuilder::new("x").build().unwrap(),
                vec![frame(7, 1, 1, 32)]
            )
        ),
        ApplyError::Entry { .. }
    ));
    // A new segment's sidecar starts at version 1.
    assert!(matches!(
        apply_err(
            &b,
            &ch(CollectionChange {
                added: vec![with_tombstones(3, 5, 2, 1)],
                ..CollectionChange::default()
            })
        ),
        ApplyError::Update { .. }
    ));
}

#[test]
fn removed_kept_and_added_segments_keep_commit_order() {
    let b = base()
        .apply(&change(
            "notes",
            CollectionChange {
                added: vec![object(3, 30)],
                ..CollectionChange::default()
            },
        ))
        .unwrap();
    let next = b
        .apply(&change(
            "notes",
            CollectionChange {
                removed: [SegmentId::new(1), SegmentId::new(3)].into(),
                updated: vec![with_tombstones(2, 20, 1, 4)],
                added: vec![object(5, 39), object(4, 1)],
                ..CollectionChange::default()
            },
        ))
        .unwrap();
    let ids: Vec<_> = next
        .collection("notes")
        .unwrap()
        .segments()
        .iter()
        .map(|s| s.id.get())
        .collect();
    assert_eq!(ids, [2, 5, 4]);
    assert_eq!(
        b.collection("notes").unwrap().segments().len(),
        3,
        "base unchanged"
    );
}

#[test]
fn tombstone_transitions() {
    let update = |old: Option<Tombstones>, new: Option<Tombstones>| {
        // Reach `old` the way commits do: version 1, 2, ... up to it.
        let mut b = Manifest::empty()
            .apply(&create("notes", notes(), vec![object(1, 10)]))
            .unwrap();
        if let Some(t) = old {
            for version in 1..=t.version {
                let step = Tombstones { version, ..t };
                let entry = SegmentEntry {
                    tombstones: Some(step),
                    ..object(1, 10)
                };
                b = b
                    .apply(&change(
                        "notes",
                        CollectionChange {
                            updated: vec![entry],
                            ..CollectionChange::default()
                        },
                    ))
                    .unwrap();
            }
        }
        b.apply(&change(
            "notes",
            CollectionChange {
                updated: vec![SegmentEntry {
                    tombstones: new,
                    ..object(1, 10)
                }],
                ..CollectionChange::default()
            },
        ))
    };
    let moved = Some(Tombstones {
        version: 3,
        deleted: 4,
        location: Location::Frame {
            offset: 12288,
            len: 64,
        },
    });
    let ok = [
        (None, None),
        (None, tombstones(1, 1)),
        (tombstones(3, 4), tombstones(4, 4)),
        (tombstones(3, 4), tombstones(4, 10)),
        (tombstones(3, 4), tombstones(3, 4)),
        (tombstones(3, 4), moved),
    ];
    for (old, new) in ok {
        assert!(update(old, new).is_ok(), "{old:?} -> {new:?}");
    }
    let bad = [
        (None, tombstones(2, 1)),
        (tombstones(3, 4), None),
        (tombstones(3, 4), tombstones(2, 4)),
        (tombstones(3, 4), tombstones(5, 5)),
        (tombstones(3, 4), tombstones(4, 3)),
        (tombstones(3, 4), tombstones(3, 5)),
    ];
    for (old, new) in bad {
        assert!(
            matches!(update(old, new), Err(ApplyError::Update { .. })),
            "{old:?} -> {new:?}"
        );
    }
    // doc_count and size never change; the location may (container compaction, SPEC §4.6).
    let b = base();
    for entry in [
        object(1, 11),
        SegmentEntry {
            size: 1,
            ..object(1, 10)
        },
    ] {
        assert!(matches!(
            apply_err(
                &b,
                &change(
                    "notes",
                    CollectionChange {
                        updated: vec![entry],
                        ..CollectionChange::default()
                    }
                )
            ),
            ApplyError::Update { .. }
        ));
    }
    assert!(
        b.apply(&change(
            "notes",
            CollectionChange {
                updated: vec![frame(1, 10, 12288, 1032)],
                ..CollectionChange::default()
            }
        ))
        .is_ok()
    );
}

#[test]
fn a_planned_merge_rebases_onto_later_commits_or_fails() {
    let b = base();
    let merge = change(
        "notes",
        CollectionChange {
            removed: [SegmentId::new(1), SegmentId::new(2)].into(),
            added: vec![object(9, 30)],
            ..CollectionChange::default()
        },
    );
    // A flush landed meanwhile: the merge still applies, after it.
    let flushed = b
        .apply(&change(
            "notes",
            CollectionChange {
                added: vec![object(3, 1)],
                ..CollectionChange::default()
            },
        ))
        .unwrap();
    let merged = flushed.apply(&merge).unwrap();
    let ids: Vec<_> = merged
        .collection("notes")
        .unwrap()
        .segments()
        .iter()
        .map(|s| s.id.get())
        .collect();
    assert_eq!(ids, [3, 9]);
    // Another merge took segment 1 meanwhile: this one no longer applies.
    let other = b
        .apply(&change(
            "notes",
            CollectionChange {
                removed: [SegmentId::new(1)].into(),
                added: vec![object(8, 10)],
                ..CollectionChange::default()
            },
        ))
        .unwrap();
    assert!(matches!(
        apply_err(&other, &merge),
        ApplyError::NoSuchSegment { .. }
    ));
}

#[test]
fn diff_recovers_each_kind_of_change() {
    let b = base();
    let mut d = change(
        "notes",
        CollectionChange {
            schema: Some(
                notes()
                    .evolve(
                        &SchemaBuilder::new("notes")
                            .field("body", text())
                            .field("tag", text())
                            .build()
                            .unwrap(),
                    )
                    .unwrap(),
            ),
            removed: [SegmentId::new(1)].into(),
            updated: vec![with_tombstones(2, 20, 1, 3)],
            added: vec![object(3, 1)],
        },
    );
    d.collections.insert(
        "more".into(),
        CollectionDiff::Create {
            schema: SchemaBuilder::new("more").build().unwrap(),
            segments: vec![object(4, 1)],
        },
    );
    d.fence_token = Some(6);
    d.wal_start = Some(200);
    let next = b.apply(&d).unwrap();
    assert_eq!(b.diff(&next).unwrap(), d);
    assert_eq!(next.diff(&next).unwrap(), ManifestDiff::default());
    let mut drop = ManifestDiff::default();
    drop.collections
        .insert("notes".into(), CollectionDiff::Drop);
    assert_eq!(b.diff(&b.apply(&drop).unwrap()).unwrap(), drop);
}

#[test]
fn diff_rejects_what_no_commit_produces() {
    let b = base();
    // Backwards: segments come back, the WAL regresses, a sidecar is removed.
    let next = b
        .apply(&change(
            "notes",
            CollectionChange {
                removed: [SegmentId::new(1)].into(),
                ..CollectionChange::default()
            },
        ))
        .unwrap();
    assert!(next.diff(&b).is_err());
    // An added segment placed before a kept one.
    let reordered = Manifest::empty()
        .apply(&create(
            "notes",
            notes(),
            vec![object(3, 1), object(1, 10), object(2, 20)],
        ))
        .unwrap();
    let reordered = reordered
        .apply(&ManifestDiff {
            fence_token: Some(5),
            wal_start: Some(100),
            ..ManifestDiff::default()
        })
        .unwrap();
    // Segment 3 is new relative to `b`, but sits first.
    assert_eq!(
        b.diff(&reordered),
        Err(ApplyError::SegmentOrder {
            collection: "notes".into()
        })
    );
}

// ---- Properties (invariants 2–4) ----

#[derive(Debug, Clone)]
enum Op {
    Flush {
        collection: usize,
        docs: u32,
        frame: bool,
    },
    Delete {
        collection: usize,
        pick: usize,
        more: u32,
    },
    Merge {
        collection: usize,
        take: usize,
    },
    Move {
        collection: usize,
        pick: usize,
        offset: u64,
    },
    AddField {
        collection: usize,
        name: String,
    },
    Advance {
        fence: u64,
        wal: u64,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..3usize, 1..1000u32, any::<bool>()).prop_map(|(collection, docs, frame)| Op::Flush {
            collection,
            docs,
            frame
        }),
        (0..3usize, any::<usize>(), 0..50u32).prop_map(|(collection, pick, more)| Op::Delete {
            collection,
            pick,
            more
        }),
        (0..3usize, 2..5usize).prop_map(|(collection, take)| Op::Merge { collection, take }),
        (0..3usize, any::<usize>(), 0..1_000_000u64).prop_map(|(collection, pick, offset)| {
            Op::Move {
                collection,
                pick,
                offset,
            }
        }),
        (0..3usize, "[a-z][a-z0-9_]{0,6}")
            .prop_map(|(collection, name)| Op::AddField { collection, name }),
        (0..3u64, 0..1000u64).prop_map(|(fence, wal)| Op::Advance { fence, wal }),
    ]
}

const NAMES: [&str; 3] = ["alpha", "beta", "gamma"];

fn location(frame: bool, offset: u64, len: u64) -> Location {
    if frame {
        Location::Frame {
            offset: 12288 + offset * 8,
            len: 32 + len * 8,
        }
    } else {
        Location::Object
    }
}

/// The diff `op` makes on `m`; `next_id` hands out fresh segment ids.
fn diff_for(m: &Manifest, op: &Op, next_id: &mut u128) -> ManifestDiff {
    let mut fresh = || {
        *next_id += 1;
        SegmentId::new(*next_id)
    };
    let collection = match op {
        Op::Advance { fence, wal } => {
            return ManifestDiff {
                fence_token: Some(m.fence_token() + fence),
                wal_start: Some(m.wal_start() + wal),
                ..ManifestDiff::default()
            };
        }
        Op::Flush { collection, .. }
        | Op::Delete { collection, .. }
        | Op::Merge { collection, .. }
        | Op::Move { collection, .. }
        | Op::AddField { collection, .. } => NAMES[*collection],
    };
    let Some(entry) = m.collection(collection) else {
        let schema = SchemaBuilder::new(collection)
            .field("body", text())
            .build()
            .unwrap();
        return create(collection, schema, vec![object(fresh().get(), 1)]);
    };
    let segments = entry.segments();
    let mut c = CollectionChange::default();
    match op {
        Op::Flush { docs, frame, .. } => c.added.push(SegmentEntry {
            id: fresh(),
            doc_count: *docs,
            size: u64::from(*docs) * 64,
            location: location(*frame, u64::from(*docs), u64::from(*docs)),
            tombstones: None,
        }),
        Op::Delete { pick, more, .. } if !segments.is_empty() => {
            let mut s = segments[pick % segments.len()];
            let (version, deleted) = s.tombstones.map_or((0, 0), |t| (t.version, t.deleted));
            s.tombstones = Some(Tombstones {
                version: version + 1,
                deleted: (deleted + more).clamp(1, s.doc_count),
                location: Location::Object,
            });
            c.updated.push(s);
        }
        Op::Merge { take, .. } if segments.len() >= 2 => {
            let take = (*take).min(segments.len());
            let docs: u32 = segments[..take].iter().map(|s| s.doc_count).sum();
            c.removed = segments[..take].iter().map(|s| s.id).collect();
            c.added.push(object(fresh().get(), docs));
        }
        Op::Move { pick, offset, .. } if !segments.is_empty() => {
            let mut s = segments[pick % segments.len()];
            s.location = location(true, *offset, 4);
            c.updated.push(s);
        }
        Op::AddField { name, .. } => {
            let mut b = SchemaBuilder::new(collection);
            for f in entry.schema().fields() {
                b = b.field(f.name(), f.kind().clone());
            }
            if let Ok(proposed) = b
                .field(name.clone(), FieldKind::I64 { stored: true })
                .build()
            {
                c.schema = Some(entry.schema().evolve(&proposed).unwrap());
            }
        }
        _ => {}
    }
    change(collection, c)
}

fn manifests(ops: &[Op]) -> Vec<Manifest> {
    let mut next_id = 0;
    let mut all = vec![Manifest::empty()];
    for op in ops {
        let m = all.last().unwrap();
        let d = diff_for(m, op, &mut next_id);
        let next = m.apply(&d).unwrap_or_else(|e| panic!("{op:?}: {e}"));
        all.push(next);
    }
    all
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(64),
        ..ProptestConfig::default()
    })]

    /// Invariants 3 and 4 over random commit chains.
    #[test]
    fn commit_chains_round_trip(ops in prop::collection::vec(op(), 0..40)) {
        let all = manifests(&ops);
        for pair in all.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            prop_assert_eq!(b.seq(), a.seq() + 1);
            let encoded = b.encode().unwrap();
            prop_assert_eq!(&Manifest::decode(&encoded).unwrap(), b);
            let d = a.diff(b).unwrap();
            prop_assert_eq!(&a.apply(&d).unwrap(), b);
        }
    }

    /// Invariant 2: damage to a real manifest is an error or a valid manifest, never a panic.
    #[test]
    fn damaged_manifests_never_panic(
        ops in prop::collection::vec(op(), 0..12),
        edits in prop::collection::vec((any::<usize>(), any::<u8>()), 1..4),
        reseal_it in any::<bool>(),
    ) {
        let m = manifests(&ops).pop().unwrap();
        let mut bytes = m.encode().unwrap();
        for (at, value) in edits {
            let i = at % bytes.len();
            bytes[i] = value;
        }
        if reseal_it {
            bytes = reseal(bytes);
        }
        if let Ok(decoded) = Manifest::decode(&bytes) {
            prop_assert_eq!(Manifest::decode(&decoded.encode().unwrap()).unwrap(), decoded);
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        let _ = Manifest::decode(&bytes);
        let mut sealed_bytes = b"PMNF\x01\x00\x00\x00".to_vec();
        sealed_bytes.extend(&bytes);
        sealed_bytes.extend([0; 4]);
        let _ = Manifest::decode(&reseal(sealed_bytes));
    }
}

#[test]
fn a_sidecar_at_the_last_version_cannot_be_rewritten() {
    let (bytes, _) = with_segments(&[frame_segment(
        12288,
        32,
        Some((u64::MAX, 4, Location::Object)),
    )]);
    let m = Manifest::decode(&bytes).unwrap();
    let mut entry = m.collection("notes").unwrap().segments()[0];
    entry.tombstones = tombstones(u64::MAX, 4);
    let same = change(
        "notes",
        CollectionChange {
            updated: vec![entry],
            ..CollectionChange::default()
        },
    );
    assert!(m.apply(&same).is_ok());
    entry.tombstones = tombstones(u64::MAX, 5);
    let more = change(
        "notes",
        CollectionChange {
            updated: vec![entry],
            ..CollectionChange::default()
        },
    );
    assert!(matches!(apply_err(&m, &more), ApplyError::Update { .. }));
}
