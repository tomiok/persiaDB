//! The manifest model, encoding and commit planning (ROADMAP 3.3; SPEC §8.7, ADR-0024): golden fixtures, every
//! corruption rule, apply/diff rules, and properties over random commit chains.
//!
//! Invariants:
//! 1. The golden fixtures in `tests/fixtures/manifest/v1/` decode to the manifests below, which encode to exactly
//!    those bytes. They are never regenerated once released (SPEC §15); `PERSIA_WRITE_MANIFEST_FIXTURES=1` writes
//!    missing ones.
//! 2. `decode` never panics: every strict prefix and every bit flip is `Corrupt`, every rule of SPEC §8.7.1 is
//!    `Corrupt` at the offending entry, and a version above 1 is `UnsupportedVersion`.
//! 3. Whatever `decode` accepts survives `encode` → `decode` unchanged, and re-encoding is byte-stable; outside the
//!    schema text every field has one encoding, so an accepted input re-encodes to itself.
//! 4. `apply` builds `seq + 1` and rejects every rule of SPEC §8.7.2 (every `ApplyError` variant is reached);
//!    `diff(a, b)` applied to `a` gives `b`; a diff planned on an older manifest applies to a newer one exactly when
//!    the segments it touches are unchanged; `relocate` changes only frame offsets and `wal_start`.
//! 5. A stored schema leaves nothing to defaults (SPEC §8.7.1): any JSON form that gives every option reads back the
//!    same schema, and leaving out any option is corruption.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::unnecessary_wraps
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::ops::Range;
use std::path::PathBuf;

use persia_engine::manifest::{
    ApplyError, CollectionChange, CollectionDiff, EntryProblem, Location, MAX_MANIFEST_LEN,
    Manifest, ManifestCorruption, ManifestDiff, ManifestError, ManifestHash, SegmentEntry,
    SegmentId, Tombstones, UpdateProblem,
};
use persia_engine::schema::{
    AnalyzerRef, BUILTIN_ANALYZERS, CollectionSchema, Fallback, FieldKind, FilterRelaxation,
    KeywordOptions, Level, Levels, RankFn, RankingConfig, RelaxationConfig, SchemaBuilder,
    StaticRank, TextOptions,
};
use persia_format::checksum::crc32c;
use proptest::prelude::*;
use serde_json::Value;

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

/// The sidecar version a plan reads a segment at (SPEC §8.7.2): 0 for none.
fn sidecar_version(s: &SegmentEntry) -> u64 {
    s.tombstones.map_or(0, |t| t.version)
}

/// `removed` for segments read without a sidecar.
fn without_sidecars(ids: &[u128]) -> BTreeMap<SegmentId, u64> {
    ids.iter().map(|i| (SegmentId::new(*i), 0)).collect()
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
            removed: without_sidecars(&[10, 11]),
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
        ("shapes.mf", golden_shapes()),
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
    // Ids and versions pinned but other options left to defaults (SPEC §8.7.1: every option is given).
    let json =
        r#"{"fields":{"body":{"analyzer":"standard@1","id":0,"type":"text"}},"name":"notes"}"#;
    let mut body = header(1);
    push_schema(&mut body, json);
    body.extend(0u32.to_le_bytes());
    assert!(matches!(
        corrupt(&sealed(&body)),
        (40, ManifestCorruption::Schema(_))
    ));
    // Every option in another key order is accepted: it need not be the text `to_json` writes.
    let sorted = serde_json::from_str::<Value>(&notes_json())
        .unwrap()
        .to_string();
    assert_ne!(sorted, notes_json());
    let mut body = header(1);
    push_schema(&mut body, &sorted);
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
    let removed = without_sidecars;
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
                removed: without_sidecars(&[1, 3]),
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
    // A sidecar moved by a container compaction keeps its version: only `relocate` does that, never a commit.
    let moved = Some(Tombstones {
        version: 3,
        deleted: 4,
        location: Location::Frame {
            offset: 12288,
            len: 64,
        },
    });
    let ok = [
        (None, tombstones(1, 1)),
        (tombstones(3, 4), tombstones(4, 4)),
        (tombstones(3, 4), tombstones(4, 10)),
    ];
    for (old, new) in ok {
        assert!(update(old, new).is_ok(), "{old:?} -> {new:?}");
    }
    let bad = [
        (None, tombstones(2, 1), UpdateProblem::FirstVersion),
        (tombstones(3, 4), None, UpdateProblem::DeletionsRemoved),
        (
            tombstones(3, 4),
            tombstones(2, 4),
            UpdateProblem::VersionStep,
        ),
        (
            tombstones(3, 4),
            tombstones(5, 5),
            UpdateProblem::VersionStep,
        ),
        (
            tombstones(3, 4),
            tombstones(4, 3),
            UpdateProblem::DeletionsRemoved,
        ),
        (
            tombstones(3, 4),
            tombstones(3, 5),
            UpdateProblem::SidecarChanged,
        ),
        (tombstones(3, 4), moved, UpdateProblem::SidecarChanged),
        // A no-op update: two deletes planned on one manifest would both write it (SPEC §8.7.2).
        (None, None, UpdateProblem::Unchanged),
        (tombstones(3, 4), tombstones(3, 4), UpdateProblem::Unchanged),
    ];
    for (old, new, problem) in bad {
        assert_eq!(
            update(old, new),
            Err(ApplyError::Update {
                id: SegmentId::new(1),
                problem
            }),
            "{old:?} -> {new:?}"
        );
    }
    // doc_count, size and location never change in a commit (SPEC §8.7.2; relocation is not a diff).
    let b = base();
    for entry in [
        object(1, 11),
        SegmentEntry {
            size: 1,
            ..object(1, 10)
        },
        frame(1, 10, 12288, 1032),
        SegmentEntry {
            tombstones: tombstones(1, 1),
            ..frame(1, 10, 12288, 1032)
        },
    ] {
        assert_eq!(
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
            ApplyError::Update {
                id: SegmentId::new(1),
                problem: UpdateProblem::SegmentChanged
            },
            "{entry:?}"
        );
    }
}

#[test]
fn a_planned_merge_rebases_onto_later_commits_or_fails() {
    let b = base();
    let merge = change(
        "notes",
        CollectionChange {
            removed: without_sidecars(&[1, 2]),
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
                removed: without_sidecars(&[1]),
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
            removed: without_sidecars(&[1]),
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
                removed: without_sidecars(&[1]),
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
            c.removed = segments[..take]
                .iter()
                .map(|s| (s.id, sidecar_version(s)))
                .collect();
            c.added.push(object(fresh().get(), docs));
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

/// One step of a chain: a commit, or a container compaction (not a diff, SPEC §8.7.2 "Relocation").
#[derive(Debug, Clone)]
enum Step {
    Commit(ManifestDiff),
    Relocate {
        moves: BTreeMap<u64, u64>,
        wal_start: u64,
    },
}

/// The step `op` makes on `m`: `Move` relocates the picked segment's frame (if it has one) and lowers `wal_start`
/// by up to 99, as compaction may; every other op is a commit.
fn step_for(m: &Manifest, op: &Op, next_id: &mut u128) -> Step {
    let Op::Move {
        collection,
        pick,
        offset,
    } = op
    else {
        return Step::Commit(diff_for(m, op, next_id));
    };
    let segments = m
        .collection(NAMES[*collection])
        .map_or(&[][..], |c| c.segments());
    let mut moves = BTreeMap::new();
    if let Some(Location::Frame { offset: from, .. }) = segments
        .get(pick % segments.len().max(1))
        .map(|s| s.location)
    {
        moves.insert(from, 12288 + offset * 8);
    }
    Step::Relocate {
        moves,
        wal_start: m.wal_start().saturating_sub(offset % 100),
    }
}

fn take_step(m: &Manifest, step: &Step) -> Result<Manifest, ApplyError> {
    match step {
        Step::Commit(d) => m.apply(d),
        Step::Relocate { moves, wal_start } => m.relocate(moves, *wal_start),
    }
}

/// The manifests of a chain, starting with the empty one, and the step between each pair.
fn chain(ops: &[Op]) -> (Vec<Manifest>, Vec<Step>) {
    let mut next_id = 0;
    let mut all = vec![Manifest::empty()];
    let mut steps = Vec::new();
    for op in ops {
        let m = all.last().unwrap();
        let step = step_for(m, op, &mut next_id);
        let next = take_step(m, &step).unwrap_or_else(|e| panic!("{op:?}: {e}"));
        all.push(next);
        steps.push(step);
    }
    (all, steps)
}

fn manifests(ops: &[Op]) -> Vec<Manifest> {
    chain(ops).0
}

/// Equal apart from `seq`.
fn same_state(a: &Manifest, b: &Manifest) -> bool {
    a.fence_token() == b.fence_token()
        && a.wal_start() == b.wal_start()
        && a.collections().eq(b.collections())
}

fn moved(location: Location, moves: &BTreeMap<u64, u64>) -> Location {
    match location {
        Location::Frame { offset, len } => Location::Frame {
            offset: moves.get(&offset).copied().unwrap_or(offset),
            len,
        },
        Location::Object => location,
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(64),
        ..ProptestConfig::default()
    })]

    /// Invariants 3 and 4 over random commit chains.
    #[test]
    fn commit_chains_round_trip(ops in prop::collection::vec(op(), 0..40)) {
        let (all, steps) = chain(&ops);
        for (pair, step) in all.windows(2).zip(&steps) {
            let (a, b) = (&pair[0], &pair[1]);
            prop_assert_eq!(b.seq(), a.seq() + 1);
            let encoded = b.encode().unwrap();
            prop_assert_eq!(&Manifest::decode(&encoded).unwrap(), b);
            // Re-encoding is deterministic, so the hash of what a writer stores is stable.
            prop_assert_eq!(&b.encode().unwrap(), &encoded);
            match step {
                Step::Commit(_) => {
                    let d = a.diff(b).unwrap();
                    prop_assert_eq!(&a.apply(&d).unwrap(), b);
                }
                Step::Relocate { moves, wal_start } => {
                    // Only frame offsets and `wal_start` change (SPEC §8.7.2 "Relocation").
                    prop_assert_eq!(b.fence_token(), a.fence_token());
                    prop_assert_eq!(b.wal_start(), *wal_start);
                    prop_assert_eq!(b.collections().len(), a.collections().len());
                    for (ca, cb) in a.collections().zip(b.collections()) {
                        prop_assert_eq!(ca.schema(), cb.schema());
                        prop_assert_eq!(ca.segments().len(), cb.segments().len());
                        for (sa, sb) in ca.segments().iter().zip(cb.segments()) {
                            let mut expected = *sa;
                            expected.location = moved(sa.location, moves);
                            if let Some(t) = &mut expected.tombstones {
                                t.location = moved(t.location, moves);
                            }
                            prop_assert_eq!(&expected, sb);
                        }
                    }
                    // A relocation that moves something or lowers wal_start is no commit.
                    let is_commit = a.wal_start() <= b.wal_start() && a.collections().eq(b.collections());
                    prop_assert_eq!(a.diff(b).is_ok(), is_commit);
                }
            }
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
    assert!(matches!(
        apply_err(&m, &same),
        ApplyError::Update {
            problem: UpdateProblem::Unchanged,
            ..
        }
    ));
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

// ---- A golden fixture with every schema option (invariant 1) ----

/// Every field kind and a non-default value for every collection setting, so the stored form of each is pinned.
fn catalog(extra: Option<&str>) -> CollectionSchema {
    let mut b = SchemaBuilder::new("catalog")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("es").unwrap(),
                boost: 2.5,
                positions: true,
                k1: 1.5,
                b: 0.5,
                stored: false,
            }),
        )
        .field(
            "sku",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                stored: false,
            }),
        )
        .field("qty", FieldKind::I64 { stored: false })
        .field("price", FieldKind::F64 { stored: true })
        .field("active", FieldKind::Bool { stored: false })
        .field("added", FieldKind::Datetime { stored: true })
        .field("raw", FieldKind::Stored)
        .ranking(RankingConfig {
            static_rank: Some(StaticRank {
                field: "qty".into(),
                weight: 0.25,
                function: RankFn::Linear,
            }),
        })
        .relaxation(RelaxationConfig {
            enabled: true,
            min_results: 7,
            levels: Levels::Custom(vec![Level::DropTerms, Level::Fuzzy]),
            fallback: Fallback::None,
            fill: true,
            filters: FilterRelaxation::LastResort,
            budget_ms: 120,
        })
        .synonyms(vec![
            vec!["tv".into(), "télévision".into()],
            vec![
                "straße".into(),
                "strasse".into(),
                "a \"quoted\\ term".into(),
            ],
        ])
        .strict(false);
    if let Some(name) = extra {
        b = b.field(name, text());
    }
    b.build().unwrap()
}

/// `catalog` evolved with `aaa`, which sorts first but gets the last id (7).
fn catalog_v2() -> CollectionSchema {
    catalog(None).evolve(&catalog(Some("aaa"))).unwrap()
}

fn logs() -> CollectionSchema {
    SchemaBuilder::new("logs")
        .field("line", text())
        .field("level", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap()
}

/// A local container after deletes, a schema evolution and a container compaction: every schema option, a
/// collection with no fields and no segments, a sidecar at version 2 moved by `relocate`, `wal_start` lowered by it.
fn golden_shapes() -> Manifest {
    let mut first = create("catalog", catalog(None), vec![frame(20, 3, 12288, 4096)]);
    first.collections.insert(
        "empty".into(),
        CollectionDiff::Create {
            schema: SchemaBuilder::new("empty").build().unwrap(),
            segments: vec![],
        },
    );
    first.collections.insert(
        "logs".into(),
        CollectionDiff::Create {
            schema: logs(),
            segments: vec![frame(21, 5, 16384, 2048), frame(22, 1, 18432, 512)],
        },
    );
    let m = at_seq(5, first, 2, 21000);
    let sidecar = |version, deleted, offset| SegmentEntry {
        tombstones: Some(Tombstones {
            version,
            deleted,
            location: Location::Frame { offset, len: 40 },
        }),
        ..frame(21, 5, 16384, 2048)
    };
    let mut m = m;
    for (version, deleted, offset) in [(1, 2, 20480), (2, 4, 20520)] {
        m = m
            .apply(&change(
                "logs",
                CollectionChange {
                    updated: vec![sidecar(version, deleted, offset)],
                    ..CollectionChange::default()
                },
            ))
            .unwrap();
    }
    let m = m
        .apply(&change(
            "catalog",
            CollectionChange {
                schema: Some(catalog_v2()),
                ..CollectionChange::default()
            },
        ))
        .unwrap();
    // The compaction drops the superseded first sidecar and moves the second into its place.
    m.relocate(&BTreeMap::from([(20520, 20480)]), 20520)
        .unwrap()
}

#[test]
fn shapes_golden_holds_what_was_committed() {
    let m = golden_shapes();
    assert_eq!((m.seq(), m.fence_token(), m.wal_start()), (9, 2, 20520));
    let catalog = m.collection("catalog").unwrap().schema();
    assert_eq!(catalog, &catalog_v2());
    assert_eq!(catalog.field("aaa").unwrap().id().get(), 7);
    assert_eq!(catalog.field("active").unwrap().id().get(), 0);
    assert_eq!(m.collection("empty").unwrap().segments(), []);
    let logs = m.collection("logs").unwrap();
    assert_eq!(
        logs.segment(SegmentId::new(21)).unwrap().tombstones,
        Some(Tombstones {
            version: 2,
            deleted: 4,
            location: Location::Frame {
                offset: 20480,
                len: 40
            }
        })
    );
    // The stored text pins every option, a non-ASCII term unescaped and the analyzer version included.
    let bytes = m.encode().unwrap();
    let text = String::from_utf8_lossy(&bytes);
    for needle in [
        r#""analyzer":"es@1""#,
        r#""strict":false"#,
        "télévision",
        r#""budget_ms":120"#,
    ] {
        assert!(text.contains(needle), "{needle}");
    }
}

// ---- Exact offsets (invariant 2) ----

/// A sealed manifest of `(schema JSON, segment bytes)` collections, the offset of each schema text, and the offset
/// of each segment.
fn layout(collections: &[(String, Vec<Vec<u8>>)]) -> (Vec<u8>, Vec<u64>, Vec<Vec<u64>>) {
    let mut body = header(collections.len() as u32);
    let (mut texts, mut segments) = (Vec::new(), Vec::new());
    for (json, entries) in collections {
        body.extend((json.len() as u32).to_le_bytes());
        texts.push(8 + body.len() as u64);
        body.extend(json.as_bytes());
        body.extend((entries.len() as u32).to_le_bytes());
        let mut at = Vec::new();
        for e in entries {
            at.push(8 + body.len() as u64);
            body.extend(e);
        }
        segments.push(at);
    }
    (sealed(&body), texts, segments)
}

/// `segment_bytes` (object location) with a tombstone block whose location tag is `tag`.
fn tombstoned(id: u128, doc_count: u32, version: u64, deleted: u32, tag: u8) -> Vec<u8> {
    let mut s = segment_bytes(id, doc_count, 300);
    s[29] = 1;
    s.extend(version.to_le_bytes());
    s.extend(deleted.to_le_bytes());
    s.push(tag);
    s
}

#[test]
fn tombstone_location_tag_is_reported_at_the_tag() {
    for tag in [0u8, 3, 0xff] {
        // Object segment: presence at 29, version 30..38, deleted 38..42, location tag at 42.
        let (bytes, at) = with_segments(&[tombstoned(5, 10, 1, 1, tag)]);
        assert_eq!(
            corrupt(&bytes),
            (at + 42, ManifestCorruption::LocationTag { tag })
        );
        // Frame segment: the frame location takes 16 more bytes, so the tag is at 58.
        let mut s = frame_segment(12288, 32, Some((1, 1, Location::Object)));
        assert_eq!(s.len(), 59);
        s[58] = tag;
        let (bytes, at) = with_segments(&[s]);
        assert_eq!(
            corrupt(&bytes),
            (at + 58, ManifestCorruption::LocationTag { tag })
        );
    }
    // A sound tag is accepted, so the offsets above are the tag's.
    let (bytes, _) = with_segments(&[tombstoned(5, 10, 1, 1, 1)]);
    assert!(Manifest::decode(&bytes).is_ok());
}

#[test]
fn problems_in_later_collections_are_reported_at_their_own_offsets() {
    let alpha = || {
        SchemaBuilder::new("alpha")
            .build()
            .unwrap()
            .to_json()
            .unwrap()
    };
    let beta = || {
        SchemaBuilder::new("beta")
            .field("body", text())
            .build()
            .unwrap()
            .to_json()
            .unwrap()
    };
    let good = |id| segment_bytes(id, 3, 300);
    let two =
        |second: Vec<u8>| layout(&[(alpha(), vec![good(1)]), (beta(), vec![good(2), second])]);
    let entry = ManifestCorruption::Entry;
    let (b, _, at) = two(segment_bytes(3, 0, 1));
    assert_eq!(
        corrupt(&b),
        (at[1][1], entry(EntryProblem::DocCount { doc_count: 0 }))
    );
    let (b, _, at) = two(good(1));
    assert_eq!(
        corrupt(&b),
        (
            at[1][1],
            ManifestCorruption::DuplicateSegment {
                id: SegmentId::new(1)
            }
        )
    );
    // Sidecar rules are reported at the start of the entry, not of its tombstone block.
    let (b, _, at) = two(tombstoned(4, 3, 0, 1, 1));
    assert_eq!(
        corrupt(&b),
        (at[1][1], entry(EntryProblem::TombstoneVersion))
    );
    let (b, _, at) = two(tombstoned(4, 3, 1, 4, 1));
    assert_eq!(
        corrupt(&b),
        (
            at[1][1],
            entry(EntryProblem::DeletedCount {
                deleted: 4,
                doc_count: 3
            })
        )
    );
    let (b, _, at) = two(tombstoned(4, 3, 1, 1, 9));
    assert_eq!(
        corrupt(&b),
        (at[1][1] + 42, ManifestCorruption::LocationTag { tag: 9 })
    );
    let mut bad_presence = good(4);
    bad_presence[29] = 7;
    let (b, _, at) = two(bad_presence);
    assert_eq!(
        corrupt(&b),
        (at[1][1] + 29, ManifestCorruption::TombstonesTag { tag: 7 })
    );
    // A bad schema in the second collection is reported at its text.
    let missing = r#"{"name":"beta","fields":{"body":{"type":"text","id":0}}}"#.to_owned();
    let (b, texts, _) = layout(&[(alpha(), vec![good(1)]), (missing, vec![])]);
    assert!(matches!(corrupt(&b), (o, ManifestCorruption::Schema(_)) if o == texts[1]));
    // An out-of-order third collection is reported at its schema length.
    let (b, texts, _) = layout(&[(alpha(), vec![]), (beta(), vec![]), (alpha(), vec![])]);
    assert_eq!(
        corrupt(&b),
        (
            texts[2] - 4,
            ManifestCorruption::CollectionOrder {
                name: "alpha".into()
            }
        )
    );
}

/// Where each collection's schema text lies in `bytes`.
fn schema_texts(m: &Manifest, bytes: &[u8]) -> Vec<Range<usize>> {
    m.collections()
        .map(|c| {
            let json = c.schema().to_json().unwrap();
            let at = bytes
                .windows(json.len())
                .position(|w| w == json.as_bytes())
                .unwrap();
            at..at + json.len()
        })
        .collect()
}

/// The CRC guards every byte, so resealing after the damage reaches the structural checks behind it.
#[test]
fn every_resealed_body_truncation_is_unexpected_eof() {
    for (name, m) in goldens() {
        let bytes = m.encode().unwrap();
        for cut in 8..bytes.len() - 4 {
            let mut b = bytes[..cut].to_vec();
            b.extend([0; 4]);
            let (offset, reason) = corrupt(&reseal(b));
            assert_eq!(
                reason,
                ManifestCorruption::UnexpectedEof,
                "{name} cut {cut}"
            );
            assert!(
                (8..=cut as u64).contains(&offset),
                "{name} cut {cut}: offset {offset}"
            );
        }
    }
}

#[test]
fn every_resealed_bit_flip_is_rejected_or_reencodes_to_itself() {
    for (name, m) in goldens() {
        let bytes = m.encode().unwrap();
        let texts = schema_texts(&m, &bytes);
        let body_end = bytes.len() - 4;
        for i in 0..body_end {
            for bit in 0..8 {
                let mut b = bytes.clone();
                b[i] ^= 1 << bit;
                let b = reseal(b);
                let got = Manifest::decode(&b);
                let at = format!("{name} byte {i} bit {bit}");
                match i {
                    0..4 => assert_eq!(corrupt(&b), (0, ManifestCorruption::Magic), "{at}"),
                    4..8 => {
                        let version = 1u32 ^ (1 << (8 * (i - 4) + bit));
                        let expected = if version == 0 {
                            Err(ManifestError::Corrupt {
                                offset: 4,
                                reason: ManifestCorruption::ZeroVersion,
                            })
                        } else {
                            Err(ManifestError::UnsupportedVersion { version })
                        };
                        assert_eq!(got, expected, "{at}");
                    }
                    _ => match got {
                        Err(ManifestError::Corrupt { offset, .. }) => {
                            assert!((8..=body_end as u64).contains(&offset), "{at}: {offset}");
                        }
                        Err(e) => panic!("{at}: {e:?}"),
                        Ok(decoded) => {
                            let again = decoded.encode().unwrap();
                            if texts.iter().any(|t| t.contains(&i)) {
                                // The schema text may read back in another form; the rest re-encodes stably.
                                let twice = Manifest::decode(&again).unwrap();
                                assert_eq!(twice, decoded, "{at}");
                                assert_eq!(twice.encode().unwrap(), again, "{at}");
                            } else {
                                assert_eq!(
                                    again, b,
                                    "{at}: accepted bytes must be the one encoding"
                                );
                            }
                        }
                    },
                }
            }
        }
    }
}

/// SPEC §8.7: the length, the magic, the CRC trailer, then `version`, then the rest.
#[test]
fn header_checks_run_in_spec_order() {
    let good = golden_empty().encode().unwrap();
    let end = good.len() - 4;
    // The magic before the checksum.
    let mut b = good.clone();
    b[1] = 0;
    b[end] ^= 1;
    assert_eq!(corrupt(&b), (0, ManifestCorruption::Magic));
    // The checksum before the version: 0, newer and garbage versions with a bad CRC are all checksum mismatches.
    for version in [0u32, 2, u32::MAX] {
        let mut b = good.clone();
        b[4..8].copy_from_slice(&version.to_le_bytes());
        let stored = u32::from_le_bytes(good[end..].try_into().unwrap());
        assert_eq!(
            corrupt(&b),
            (
                end as u64,
                ManifestCorruption::ChecksumMismatch {
                    stored,
                    computed: crc32c(&b[..end])
                }
            ),
            "version {version}"
        );
    }
    // A newer version with a valid CRC is unsupported, whatever follows: its body is never read.
    for body in [vec![], vec![0xff; 3], header(u32::MAX)] {
        let mut b = b"PMNF".to_vec();
        b.extend(7u32.to_le_bytes());
        b.extend(&body);
        b.extend([0; 4]);
        assert_eq!(
            Manifest::decode(&reseal(b)),
            Err(ManifestError::UnsupportedVersion { version: 7 })
        );
    }
    // Too short for a magic, then too short for a version and a trailer.
    for len in 0..4 {
        assert_eq!(
            corrupt(&b"PMNF"[..len]),
            (0, ManifestCorruption::UnexpectedEof)
        );
    }
    for len in 4..12 {
        let mut b = b"PMNF".to_vec();
        b.resize(len, 0);
        assert_eq!(
            corrupt(&b),
            (len as u64, ManifestCorruption::UnexpectedEof),
            "{len}"
        );
    }
    // A sealed version 1 with no body runs out at the first field.
    assert_eq!(
        corrupt(&sealed(&[])),
        (8, ManifestCorruption::UnexpectedEof)
    );
    // Exactly the limit is not too large (it is read, and here fails the magic).
    assert_eq!(
        corrupt(&vec![0; MAX_MANIFEST_LEN]),
        (0, ManifestCorruption::Magic)
    );
}

// ---- Apply, relocate and rebase (invariant 4) ----

/// A manifest at the last `seq`, which only decoding can build.
fn at_max_seq() -> Manifest {
    let mut body = u64::MAX.to_le_bytes().to_vec();
    body.extend([0; 20]);
    Manifest::decode(&sealed(&body)).unwrap()
}

fn variant(e: &ApplyError) -> usize {
    // Exhaustive on purpose: a new variant must be added to `every_apply_error_variant_is_reachable`.
    match e {
        ApplyError::SeqOverflow => 0,
        ApplyError::FenceRegressed { .. } => 1,
        ApplyError::WalRegressed { .. } => 2,
        ApplyError::CollectionExists { .. } => 3,
        ApplyError::NoSuchCollection { .. } => 4,
        ApplyError::SchemaName { .. } => 5,
        ApplyError::Evolution { .. } => 6,
        ApplyError::NotEvolved { .. } => 7,
        ApplyError::NoSuchSegment { .. } => 8,
        ApplyError::DuplicateSegment { .. } => 9,
        ApplyError::Entry { .. } => 10,
        ApplyError::Conflict { .. } => 11,
        ApplyError::Update { .. } => 12,
        ApplyError::UnknownFrame { .. } => 13,
        ApplyError::SegmentOrder { .. } => 14,
    }
}

#[test]
fn every_apply_error_variant_is_reachable() {
    let b = base();
    let notes_change = |c| change("notes", c);
    let with_schema = |schema| {
        notes_change(CollectionChange {
            schema: Some(schema),
            ..CollectionChange::default()
        })
    };
    let mut drop = ManifestDiff::default();
    drop.collections.insert("nope".into(), CollectionDiff::Drop);
    let deleted = b
        .apply(&notes_change(CollectionChange {
            updated: vec![with_tombstones(1, 10, 1, 3)],
            ..CollectionChange::default()
        }))
        .unwrap();
    let reordered = Manifest::empty()
        .apply(&ManifestDiff {
            fence_token: Some(5),
            wal_start: Some(100),
            ..create(
                "notes",
                notes(),
                vec![object(3, 1), object(1, 10), object(2, 20)],
            )
        })
        .unwrap();
    let errors = [
        apply_err(&at_max_seq(), &ManifestDiff::default()),
        apply_err(
            &b,
            &ManifestDiff {
                fence_token: Some(4),
                ..ManifestDiff::default()
            },
        ),
        apply_err(
            &b,
            &ManifestDiff {
                wal_start: Some(99),
                ..ManifestDiff::default()
            },
        ),
        apply_err(&b, &create("notes", notes(), vec![])),
        apply_err(&b, &drop),
        apply_err(&b, &create("other", notes(), vec![])),
        apply_err(
            &b,
            &with_schema(
                SchemaBuilder::new("notes")
                    .field("body", FieldKind::Stored)
                    .build()
                    .unwrap(),
            ),
        ),
        apply_err(
            &b,
            &with_schema(
                SchemaBuilder::new("notes")
                    .field("a", text())
                    .field("body", text())
                    .build()
                    .unwrap(),
            ),
        ),
        apply_err(
            &b,
            &notes_change(CollectionChange {
                removed: without_sidecars(&[9]),
                ..CollectionChange::default()
            }),
        ),
        apply_err(
            &b,
            &notes_change(CollectionChange {
                added: vec![object(1, 10)],
                ..CollectionChange::default()
            }),
        ),
        apply_err(
            &b,
            &notes_change(CollectionChange {
                added: vec![object(3, 0)],
                ..CollectionChange::default()
            }),
        ),
        apply_err(
            &deleted,
            &notes_change(CollectionChange {
                removed: without_sidecars(&[1]),
                ..CollectionChange::default()
            }),
        ),
        apply_err(
            &b,
            &notes_change(CollectionChange {
                updated: vec![object(1, 11)],
                ..CollectionChange::default()
            }),
        ),
        b.relocate(&BTreeMap::from([(12288, 16384)]), 100)
            .unwrap_err(),
        b.diff(&reordered).unwrap_err(),
    ];
    for (i, e) in errors.iter().enumerate() {
        assert_eq!(variant(e), i, "{e:?}");
        assert_ne!(e.to_string(), "");
    }
    let seen: BTreeSet<usize> = errors.iter().map(variant).collect();
    assert_eq!(seen.len(), 15);
}

#[test]
fn seq_overflow_is_checked_first_and_leaves_the_base_alone() {
    let m = at_max_seq();
    let before = m.clone();
    // Even a diff that breaks other rules reports the overflow.
    let d = ManifestDiff {
        fence_token: Some(0),
        ..create("x", notes(), vec![])
    };
    assert_eq!(m.apply(&d), Err(ApplyError::SeqOverflow));
    assert_eq!(
        m.relocate(&BTreeMap::new(), 0),
        Err(ApplyError::SeqOverflow)
    );
    assert_eq!(m.diff(&m), Err(ApplyError::SeqOverflow));
    assert_eq!(m, before);
}

#[test]
fn fence_and_wal_may_stay_or_grow_to_the_maximum() {
    let b = base();
    for (fence, wal) in [(5, 100), (6, 100), (5, 101), (u64::MAX, u64::MAX)] {
        let next = b
            .apply(&ManifestDiff {
                fence_token: Some(fence),
                wal_start: Some(wal),
                ..ManifestDiff::default()
            })
            .unwrap();
        assert_eq!(
            (next.seq(), next.fence_token(), next.wal_start()),
            (2, fence, wal)
        );
    }
    // The fence is checked before the WAL start.
    let d = ManifestDiff {
        fence_token: Some(0),
        wal_start: Some(0),
        ..ManifestDiff::default()
    };
    assert!(matches!(
        apply_err(&b, &d),
        ApplyError::FenceRegressed { .. }
    ));
}

/// SPEC §8.7.2: a merge drops the documents deleted at the sidecar version it read; applying it over a newer
/// sidecar would bring later deletions back.
#[test]
fn a_merge_planned_before_a_delete_on_its_input_conflicts() {
    let b = base();
    let merge = change(
        "notes",
        CollectionChange {
            removed: without_sidecars(&[1, 2]),
            added: vec![object(9, 30)],
            ..CollectionChange::default()
        },
    );
    let deleted = b
        .apply(&change(
            "notes",
            CollectionChange {
                updated: vec![with_tombstones(1, 10, 1, 3)],
                ..CollectionChange::default()
            },
        ))
        .unwrap();
    assert_eq!(
        apply_err(&deleted, &merge),
        ApplyError::Conflict {
            id: SegmentId::new(1),
            planned: 0,
            current: 1
        }
    );
    // A plan naming a version the segment has not reached conflicts too.
    let ahead = change(
        "notes",
        CollectionChange {
            removed: BTreeMap::from([(SegmentId::new(1), 2)]),
            ..CollectionChange::default()
        },
    );
    assert_eq!(
        apply_err(&deleted, &ahead),
        ApplyError::Conflict {
            id: SegmentId::new(1),
            planned: 2,
            current: 1
        }
    );
    // Replanned at the version it now has, it applies.
    let replanned = change(
        "notes",
        CollectionChange {
            removed: BTreeMap::from([(SegmentId::new(1), 1), (SegmentId::new(2), 0)]),
            added: vec![object(9, 27)],
            ..CollectionChange::default()
        },
    );
    let merged = deleted.apply(&replanned).unwrap();
    assert_eq!(
        merged.collection("notes").unwrap().segments(),
        [object(9, 27)]
    );
    // `diff` names the versions removed segments had.
    assert_eq!(deleted.diff(&merged).unwrap(), replanned);
}

#[test]
fn a_diff_planned_before_a_compaction_applies_only_if_it_names_no_moved_location() {
    let b = Manifest::empty()
        .apply(&create(
            "notes",
            notes(),
            vec![frame(1, 10, 12288, 1024), frame(2, 20, 14336, 1024)],
        ))
        .unwrap();
    let delete = |id, offset| {
        change(
            "notes",
            CollectionChange {
                updated: vec![SegmentEntry {
                    tombstones: tombstones(1, 1),
                    ..frame(id, if id == 1 { 10 } else { 20 }, offset, 1024)
                }],
                ..CollectionChange::default()
            },
        )
    };
    let merge = change(
        "notes",
        CollectionChange {
            removed: without_sidecars(&[1, 2]),
            added: vec![frame(9, 30, 20480, 2048)],
            ..CollectionChange::default()
        },
    );
    let compacted = b.relocate(&BTreeMap::from([(14336, 13312)]), 0).unwrap();
    // The delete carries segment 2's old location: applying it would point the segment at a freed frame.
    assert_eq!(
        apply_err(&compacted, &delete(2, 14336)),
        ApplyError::Update {
            id: SegmentId::new(2),
            problem: UpdateProblem::SegmentChanged
        }
    );
    // Segment 1 did not move, so a delete on it still applies; so does the merge, which names no location.
    assert!(compacted.apply(&delete(1, 12288)).is_ok());
    let merged = compacted.apply(&merge).unwrap();
    assert_eq!(
        merged.collection("notes").unwrap().segments(),
        [frame(9, 30, 20480, 2048)]
    );
}

#[test]
fn relocate_moves_frame_locations_and_nothing_else() {
    let sidecar = Tombstones {
        version: 1,
        deleted: 2,
        location: Location::Frame {
            offset: 20480,
            len: 40,
        },
    };
    let b = Manifest::empty()
        .apply(&ManifestDiff {
            fence_token: Some(4),
            wal_start: Some(30000),
            ..create(
                "notes",
                notes(),
                vec![
                    SegmentEntry {
                        tombstones: Some(sidecar),
                        ..frame(1, 10, 12288, 4096)
                    },
                    object(2, 5),
                    frame(3, 1, 16384, 64),
                ],
            )
        })
        .unwrap();
    let before = b.clone();
    // Compaction closes the gap after segment 3; an identity move is allowed and changes nothing.
    let moved = b
        .relocate(&BTreeMap::from([(12288, 12288), (20480, 16448)]), 16488)
        .unwrap();
    assert_eq!(
        (moved.seq(), moved.fence_token(), moved.wal_start()),
        (2, 4, 16488)
    );
    let segments = moved.collection("notes").unwrap().segments();
    assert_eq!(
        segments,
        [
            SegmentEntry {
                tombstones: Some(Tombstones {
                    location: Location::Frame {
                        offset: 16448,
                        len: 40
                    },
                    ..sidecar
                }),
                ..frame(1, 10, 12288, 4096)
            },
            object(2, 5),
            frame(3, 1, 16384, 64),
        ]
    );
    assert_eq!(Manifest::decode(&moved.encode().unwrap()).unwrap(), moved);
    // Lowering wal_start is what makes it no commit.
    assert!(matches!(
        b.diff(&moved),
        Err(ApplyError::WalRegressed { .. })
    ));
    // Errors: an offset no location holds, a target that is no frame location; the base never changes.
    assert_eq!(
        b.relocate(&BTreeMap::from([(20488, 12288)]), 0),
        Err(ApplyError::UnknownFrame { offset: 20488 })
    );
    for to in [16385, 8192, u64::MAX - 63] {
        assert_eq!(
            b.relocate(&BTreeMap::from([(16384, to)]), 0),
            Err(ApplyError::Entry {
                id: SegmentId::new(3),
                problem: EntryProblem::FrameLocation {
                    offset: to,
                    len: 64
                }
            }),
            "{to}"
        );
    }
    assert_eq!(b, before);
}

// ---- Stored schemas (invariant 5) ----

/// A sealed seq-1 manifest whose one collection has schema text `json` and segment `object(1, 1)`.
fn stored_with_schema(json: &str) -> Vec<u8> {
    let mut body = header(1);
    push_schema(&mut body, json);
    body.extend(1u32.to_le_bytes());
    body.extend(segment_bytes(1, 1, 100));
    sealed(&body)
}

fn decode_with_schema(v: &Value) -> Result<Manifest, ManifestError> {
    Manifest::decode(&stored_with_schema(&v.to_string()))
}

fn schema_corrupt(got: &Result<Manifest, ManifestError>) -> bool {
    matches!(
        got,
        Err(ManifestError::Corrupt {
            offset: 40,
            reason: ManifestCorruption::Schema(_)
        })
    )
}

/// Every object key of `v`, as a path from the root.
fn key_paths(v: &Value) -> Vec<Vec<String>> {
    fn walk(v: &Value, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        if let Value::Object(map) = v {
            for (k, child) in map {
                prefix.push(k.clone());
                out.push(prefix.clone());
                walk(child, prefix, out);
                prefix.pop();
            }
        }
    }
    let mut out = Vec::new();
    walk(v, &mut Vec::new(), &mut out);
    out
}

fn without_key(v: &Value, path: &[String]) -> Value {
    let mut v = v.clone();
    let (last, parents) = path.split_last().unwrap();
    let mut at = &mut v;
    for k in parents {
        at = at.get_mut(k).unwrap();
    }
    at.as_object_mut().unwrap().remove(last);
    v
}

/// Removing a whole field entry leaves a smaller schema (valid if it had the last id), and an absent `ranking` is
/// how a schema without a static rank is written; every other key is an option (SPEC §8.7.1).
fn is_option(path: &[String]) -> bool {
    !(path == ["ranking"] || (path.len() == 2 && path[0] == "fields"))
}

fn catalog_manifest() -> Manifest {
    Manifest::empty()
        .apply(&create("catalog", catalog_v2(), vec![object(1, 1)]))
        .unwrap()
}

#[test]
fn a_stored_schema_leaving_any_option_to_defaults_is_corrupt() {
    let m = catalog_manifest();
    let full: Value = serde_json::from_str(&catalog_v2().to_json().unwrap()).unwrap();
    assert_eq!(decode_with_schema(&full).unwrap(), m);
    let paths = key_paths(&full);
    // Every key `to_json` writes for this schema: a new option shows up here and must be given too.
    assert_eq!(paths.len(), 59);
    for path in paths.iter().filter(|p| is_option(p)) {
        let got = decode_with_schema(&without_key(&full, path));
        assert!(schema_corrupt(&got), "without {path:?}: {got:?}");
        // The same key set to null is no value either.
        let mut nulled = full.clone();
        *nulled.pointer_mut(&format!("/{}", path.join("/"))).unwrap() = Value::Null;
        assert!(
            schema_corrupt(&decode_with_schema(&nulled)),
            "{path:?} null"
        );
    }
    // An unknown option is not silently ignored (ADR-0022), at the top or in a field.
    for pointer in ["", "/fields/title", "/relaxation"] {
        let mut v = full.clone();
        v.pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("future_option".into(), Value::Bool(true));
        assert!(schema_corrupt(&decode_with_schema(&v)), "{pointer}");
    }
}

#[test]
fn a_stored_analyzer_names_a_known_version() {
    let full: Value = serde_json::from_str(&catalog_v2().to_json().unwrap()).unwrap();
    for analyzer in [
        "es",
        "es@",
        "es@0",
        "es@01",
        "es@+1",
        "es@2",
        "@1",
        "es@1@1",
        "ES@1",
        "klingon@1",
        " es@1",
        "es@1 ",
        "",
    ] {
        let mut v = full.clone();
        v["fields"]["title"]["analyzer"] = Value::String(analyzer.into());
        assert!(schema_corrupt(&decode_with_schema(&v)), "{analyzer:?}");
    }
}

#[test]
fn stored_ids_are_read_not_recomputed() {
    let full: Value = serde_json::from_str(&catalog_v2().to_json().unwrap()).unwrap();
    // Duplicate and out-of-range ids break SPEC §6.2.1 (ids are exactly 0..field_count).
    for (field, id) in [("title", 0), ("aaa", 8), ("aaa", u64::from(u32::MAX) + 1)] {
        let mut v = full.clone();
        v["fields"][field]["id"] = Value::from(id);
        assert!(schema_corrupt(&decode_with_schema(&v)), "{field} {id}");
    }
    // Swapped ids are a different, valid schema: the reader takes them as written.
    let mut v = full.clone();
    v["fields"]["aaa"]["id"] = Value::from(0);
    v["fields"]["active"]["id"] = Value::from(7);
    let m = decode_with_schema(&v).unwrap();
    let schema = m.collection("catalog").unwrap().schema();
    assert_eq!(schema.field("aaa").unwrap().id().get(), 0);
    assert_eq!(schema.field("active").unwrap().id().get(), 7);
    // Why ids must be stored: the plain JSON form without them renumbers an evolved schema by name.
    let mut stripped = full.clone();
    for f in catalog_v2().fields() {
        stripped["fields"][f.name()]
            .as_object_mut()
            .unwrap()
            .remove("id");
    }
    let renumbered = CollectionSchema::from_json(&stripped.to_string()).unwrap();
    assert_eq!(renumbered.field("aaa").unwrap().id().get(), 0);
    assert_ne!(renumbered, catalog_v2());
    assert!(schema_corrupt(&decode_with_schema(&stripped)));
}

/// How `write_json` lays out a JSON value: key order, whitespace and string escapes.
#[derive(Debug, Clone, Copy)]
struct JsonForm {
    reverse: bool,
    pretty: bool,
    escape: bool,
}

fn write_json_string(s: &str, escape: bool, out: &mut String) {
    if escape {
        out.push('"');
        for unit in s.encode_utf16() {
            write!(out, "\\u{unit:04X}").unwrap();
        }
        out.push('"');
    } else {
        out.push_str(&serde_json::to_string(s).unwrap());
    }
}

fn write_json(v: &Value, form: JsonForm, depth: usize, out: &mut String) {
    let newline = |out: &mut String, depth: usize| {
        if form.pretty {
            out.push('\n');
            out.push_str(&"\t".repeat(depth));
        }
    };
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            if form.reverse {
                keys.reverse();
            }
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                write_json_string(k, form.escape, out);
                out.push_str(if form.pretty { " : " } else { ":" });
                write_json(&map[k.as_str()], form, depth + 1, out);
            }
            if !keys.is_empty() {
                newline(out, depth);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                write_json(item, form, depth + 1, out);
            }
            if !items.is_empty() {
                newline(out, depth);
            }
            out.push(']');
        }
        Value::String(s) => write_json_string(s, form.escape, out),
        other => out.push_str(&other.to_string()),
    }
}

fn any_field_kind() -> impl Strategy<Value = FieldKind> {
    let text = (
        prop::sample::select(BUILTIN_ANALYZERS),
        prop_oneof![Just(1.0), 1e-3..=1000.0f64],
        any::<bool>(),
        0.0..=10.0f64,
        0.0..=1.0f64,
        any::<bool>(),
    )
        .prop_map(|(analyzer, boost, positions, k1, b, stored)| {
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin(analyzer).unwrap(),
                boost,
                positions,
                k1,
                b,
                stored,
            })
        });
    prop_oneof![
        text,
        (any::<bool>(), any::<bool>())
            .prop_map(|(facet, stored)| FieldKind::Keyword(KeywordOptions { facet, stored })),
        any::<bool>().prop_map(|stored| FieldKind::I64 { stored }),
        any::<bool>().prop_map(|stored| FieldKind::F64 { stored }),
        any::<bool>().prop_map(|stored| FieldKind::Bool { stored }),
        any::<bool>().prop_map(|stored| FieldKind::Datetime { stored }),
        Just(FieldKind::Stored),
    ]
}

/// Valid `notes` schemas, evolved by `added` fields so ids are not in name order, with synonyms that need escaping
/// and, sometimes, a static rank and non-default relaxation.
fn any_schema() -> impl Strategy<Value = CollectionSchema> {
    (
        prop::collection::btree_map("[a-z][a-z0-9_]{0,8}", any_field_kind(), 0..6),
        prop::collection::vec("[a-z][a-z0-9_]{0,8}", 0..3),
        prop::collection::vec(
            prop::collection::btree_set("[a-zé\"\\\\ ]{0,4}[a-z]", 2..4),
            0..3,
        ),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_filter_map(
            "a valid, evolved schema",
            |(fields, added, synonyms, strict, rank, relax)| {
                let synonyms: Vec<Vec<String>> = synonyms
                    .into_iter()
                    .map(|g| g.into_iter().collect())
                    .collect();
                let mut current: Vec<(String, FieldKind)> = fields.into_iter().collect();
                let rank_field = current
                    .iter()
                    .find(|(_, k)| matches!(k, FieldKind::I64 { .. } | FieldKind::F64 { .. }))
                    .map(|(n, _)| n.clone());
                let builder = |fields: &[(String, FieldKind)]| {
                    let mut b = fields
                        .iter()
                        .fold(SchemaBuilder::new("notes"), |b, (n, k)| {
                            b.field(n.clone(), k.clone())
                        })
                        .synonyms(synonyms.clone())
                        .strict(strict);
                    if let (true, Some(field)) = (rank, &rank_field) {
                        b = b.ranking(RankingConfig {
                            static_rank: Some(StaticRank {
                                weight: 0.5,
                                ..StaticRank::new(field.clone())
                            }),
                        });
                    }
                    if relax {
                        b = b.relaxation(RelaxationConfig {
                            levels: Levels::Custom(vec![Level::Prefix]),
                            fallback: Fallback::None,
                            ..RelaxationConfig::default()
                        });
                    }
                    b
                };
                let mut schema = builder(&current).build().ok()?;
                for name in added {
                    current.push((name, FieldKind::Bool { stored: false }));
                    schema = schema.evolve(&builder(&current).build().ok()?).ok()?;
                }
                Some(schema)
            },
        )
}

// ---- Strategies for arbitrary (mostly invalid) diffs ----

fn any_location() -> impl Strategy<Value = Location> {
    prop_oneof![
        Just(Location::Object),
        (
            prop_oneof![
                Just(0u64),
                Just(12280),
                Just(12288),
                Just(12289),
                (1536..2000u64).prop_map(|k| k * 8)
            ],
            prop_oneof![
                Just(8u64),
                Just(32),
                Just(33),
                (4..100u64).prop_map(|k| k * 8)
            ],
        )
            .prop_map(|(offset, len)| Location::Frame { offset, len }),
    ]
}

fn any_entry() -> impl Strategy<Value = SegmentEntry> {
    (
        1..12u128,
        prop_oneof![Just(0u32), 1..30u32, Just(1 << 31), Just((1 << 31) + 1)],
        prop_oneof![Just(0u64), 1..5000u64],
        any_location(),
        prop::option::of((0..4u64, 0..35u32, any_location())),
    )
        .prop_map(|(id, doc_count, size, location, t)| SegmentEntry {
            id: SegmentId::new(id),
            doc_count,
            size,
            location,
            tombstones: t.map(|(version, deleted, location)| Tombstones {
                version,
                deleted,
                location,
            }),
        })
}

fn any_collection_diff() -> impl Strategy<Value = (usize, CollectionDiff)> {
    let names = NAMES.len() + 1;
    let schema = |name: &str, extra: Option<&str>| {
        let mut b = SchemaBuilder::new(name).field("body", text());
        if let Some(extra) = extra {
            b = b.field(extra, FieldKind::I64 { stored: true });
        }
        b.build().unwrap()
    };
    prop_oneof![
        (0..names, 0..names, prop::collection::vec(any_entry(), 0..3)).prop_map(
            move |(key, named, segments)| {
                let name = name_of(named);
                (
                    key,
                    CollectionDiff::Create {
                        schema: schema(name, None),
                        segments,
                    },
                )
            }
        ),
        (0..names).prop_map(|key| (key, CollectionDiff::Drop)),
        (
            0..names,
            prop::option::of(prop::sample::select(vec!["a", "zz", "body"])),
            prop::collection::btree_map((1..12u128).prop_map(SegmentId::new), 0..3u64, 0..3),
            prop::collection::vec(any_entry(), 0..3),
            prop::collection::vec(any_entry(), 0..3),
        )
            .prop_map(move |(key, extra, removed, updated, added)| {
                let name = name_of(key);
                let change = CollectionChange {
                    schema: extra.map(|e| schema(name, (e != "body").then_some(e))),
                    removed,
                    updated,
                    added,
                };
                (key, CollectionDiff::Change(change))
            }),
    ]
}

fn name_of(i: usize) -> &'static str {
    NAMES.get(i).copied().unwrap_or("nope")
}

fn any_diff() -> impl Strategy<Value = ManifestDiff> {
    (
        prop::option::of(0..8u64),
        prop::option::of(0..2000u64),
        prop::collection::vec(any_collection_diff(), 0..3),
    )
        .prop_map(|(fence_token, wal_start, changes)| ManifestDiff {
            fence_token,
            wal_start,
            collections: changes
                .into_iter()
                .map(|(key, d)| (name_of(key).to_owned(), d))
                .collect(),
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(64),
        ..ProptestConfig::default()
    })]

    /// SPEC §8.7.2: a diff planned on an older manifest applies to a newer one exactly when the segments it
    /// touches are unchanged (removed ones at the sidecar version it read; updated ones as it read them, or already
    /// as it would make them).
    #[test]
    fn a_diff_planned_on_an_older_manifest_applies_iff_its_segments_are_unchanged(
        ops in prop::collection::vec(op(), 1..30),
        at in any::<prop::sample::Index>(),
        planned in op().prop_filter("a segment commit", |o| matches!(o, Op::Flush { .. } | Op::Delete { .. } | Op::Merge { .. })),
    ) {
        let all = manifests(&ops);
        let old = at.get(&all);
        let new = all.last().unwrap();
        let mut next_id = 1 << 64;
        let d = diff_for(old, &planned, &mut next_id);
        let (name, diff) = d.collections.iter().next().unwrap();
        // The collection did not exist yet: a create, not a rebase.
        let CollectionDiff::Change(c) = diff else { return Ok(()) };
        let before = old.collection(name).unwrap();
        let unchanged = new.collection(name).is_some_and(|now| {
            c.removed.iter().all(|(id, v)| now.segment(*id).is_some_and(|s| sidecar_version(s) == *v))
                && c.updated.iter().all(|u| {
                    now.segment(u.id).is_some_and(|s| Some(s) == before.segment(u.id))
                })
        });
        let got = new.apply(&d);
        prop_assert_eq!(got.is_ok(), unchanged, "{:?}", got);
        if let Err(e) = got {
            prop_assert!(matches!(
                e,
                ApplyError::NoSuchSegment { .. } | ApplyError::Conflict { .. } | ApplyError::Update { .. }
            ), "{:?}", e);
        }
    }

    /// Any diff, valid or not: the base never changes, and what `apply` accepts is a manifest `decode` accepts,
    /// that `diff` recovers, and that keeps every surviving segment's contents, collection and deletions.
    #[test]
    fn apply_accepts_only_valid_transitions(
        ops in prop::collection::vec(op(), 0..10),
        d in any_diff(),
    ) {
        let base = manifests(&ops).pop().unwrap();
        let before = base.clone();
        let got = base.apply(&d);
        prop_assert_eq!(&base, &before);
        let Ok(next) = got else { return Ok(()) };
        prop_assert_eq!(next.seq(), base.seq() + 1);
        prop_assert!(next.fence_token() >= base.fence_token());
        prop_assert!(next.wal_start() >= base.wal_start());
        prop_assert_eq!(&Manifest::decode(&next.encode().unwrap()).unwrap(), &next);
        let recovered = base.diff(&next).unwrap();
        prop_assert_eq!(&base.apply(&recovered).unwrap(), &next);
        for c in next.collections() {
            for s in c.segments() {
                let old = base.collections().find_map(|o| o.segment(s.id).map(|e| (o.name(), e)));
                match old {
                    Some((collection, o)) => {
                        prop_assert_eq!(collection, c.name());
                        prop_assert_eq!((o.doc_count, o.size, o.location), (s.doc_count, s.size, s.location));
                        if let Some(t) = o.tombstones {
                            let n = s.tombstones.unwrap();
                            prop_assert!(n.version >= t.version && n.deleted >= t.deleted);
                        }
                    }
                    None => prop_assert!(s.tombstones.is_none_or(|t| t.version == 1)),
                }
            }
        }
    }

    /// `diff` between any two manifests of a chain, if it exists, is a single commit leading from one to the other.
    #[test]
    fn diff_between_any_two_manifests_is_sound(
        ops in prop::collection::vec(op(), 0..25),
        i in any::<prop::sample::Index>(),
        j in any::<prop::sample::Index>(),
    ) {
        let all = manifests(&ops);
        let (a, c) = (i.get(&all), j.get(&all));
        prop_assert_eq!(a.diff(a).unwrap(), ManifestDiff::default());
        if let Ok(d) = a.diff(c) {
            let applied = a.apply(&d).unwrap();
            prop_assert_eq!(applied.seq(), a.seq() + 1);
            prop_assert!(same_state(&applied, c));
        }
    }

    /// Invariants 3 and 5: the schema text may take any JSON form that gives every option; it reads back the same
    /// schema and re-encodes to the canonical bytes. Leaving out any one option is corruption.
    #[test]
    fn any_json_form_with_every_option_reads_back(
        schema in any_schema(),
        reverse in any::<bool>(),
        pretty in any::<bool>(),
        escape in any::<bool>(),
        pick in any::<prop::sample::Index>(),
    ) {
        let m = Manifest::empty().apply(&create("notes", schema.clone(), vec![object(1, 1)])).unwrap();
        let canonical = m.encode().unwrap();
        let value: Value = serde_json::from_str(&schema.to_json().unwrap()).unwrap();
        let mut text = String::new();
        write_json(&value, JsonForm { reverse, pretty, escape }, 0, &mut text);
        let decoded = Manifest::decode(&stored_with_schema(&text)).unwrap();
        prop_assert_eq!(&decoded, &m);
        prop_assert_eq!(decoded.encode().unwrap(), canonical);
        let options: Vec<_> = key_paths(&value).into_iter().filter(|p| is_option(p)).collect();
        let path = pick.get(&options);
        let mut stripped = String::new();
        write_json(&without_key(&value, path), JsonForm { reverse, pretty, escape }, 0, &mut stripped);
        let got = Manifest::decode(&stored_with_schema(&stripped));
        prop_assert!(schema_corrupt(&got), "without {:?}: {:?}", path, got);
    }
}

// ---- The encode limit (invariant 2; SPEC §8.7: at most 64 MiB) ----

/// About 4 s per encode in a debug build, so nightly only. Builds 32 collections whose escaped synonyms (`\` takes two
/// bytes in JSON) make each schema text about 2 MiB, then tunes the last one so the manifest is exactly
/// `MAX_MANIFEST_LEN` bytes: that encodes and decodes, one byte more is `TooLarge`.
#[cfg(feature = "slow")]
#[test]
fn encode_limit_is_exact() {
    use persia_engine::manifest::ManifestEncodeError;
    use persia_engine::schema::MAX_SYNONYM_BYTES;

    let full_group = || vec!["\\".repeat(256), "\\".repeat(255)];
    let full: Vec<CollectionSchema> = (0..31)
        .map(|i| {
            SchemaBuilder::new(format!("c{i:02}"))
                .synonyms(vec![full_group(); 2052])
                .build()
                .unwrap()
        })
        .collect();
    // magic, version, seq, fence, wal, count, trailer; then per collection two u32 and the text.
    let fixed: usize = 40
        + full
            .iter()
            .map(|s| 8 + s.to_json().unwrap().len())
            .sum::<usize>();
    let budget = MAX_MANIFEST_LEN - fixed - 8;
    // `extra` escaped bytes beyond the minimal last group ["a","b"]; each JSON byte of `a` or `b` adds one.
    let last = |groups: usize, extra: usize| {
        let ea = extra.min(510);
        let eb = extra - ea;
        let term = |c: &str, e: usize| {
            format!(
                "{c}{}{}",
                "\\".repeat(e / 2),
                if e % 2 == 1 { c } else { "" }
            )
        };
        let mut synonyms = vec![full_group(); groups];
        synonyms.push(vec![term("a", ea), term("b", eb)]);
        let total: usize = synonyms.iter().flatten().map(String::len).sum();
        assert!(total <= MAX_SYNONYM_BYTES);
        SchemaBuilder::new("c31")
            .synonyms(synonyms)
            .build()
            .unwrap()
    };
    let mut groups = budget / 1029;
    let extra = loop {
        let base = last(groups, 0).to_json().unwrap().len();
        match budget.checked_sub(base) {
            Some(extra) if extra < 1020 => break extra,
            Some(_) => groups += 1,
            None => groups -= 1,
        }
    };
    let manifest = |extra| {
        let mut diff = ManifestDiff::default();
        for schema in full.iter().cloned().chain([last(groups, extra)]) {
            let name = schema.name().to_owned();
            diff.collections.insert(
                name,
                CollectionDiff::Create {
                    schema,
                    segments: vec![],
                },
            );
        }
        Manifest::empty().apply(&diff).unwrap()
    };
    let exact = manifest(extra);
    let bytes = exact.encode().unwrap();
    assert_eq!(bytes.len(), MAX_MANIFEST_LEN);
    assert_eq!(Manifest::decode(&bytes).unwrap(), exact);
    assert_eq!(
        manifest(extra + 1).encode(),
        Err(ManifestEncodeError::TooLarge {
            len: MAX_MANIFEST_LEN + 1
        })
    );
}

#[test]
fn two_deletes_planned_on_one_manifest_cannot_both_apply() {
    let b = base();
    let delete = |deleted| {
        change(
            "notes",
            CollectionChange {
                updated: vec![with_tombstones(1, 10, 1, deleted)],
                ..CollectionChange::default()
            },
        )
    };
    let first = b.apply(&delete(2)).unwrap();
    // The same sidecar version again, whatever its docids: stale, never a no-op.
    for deleted in [2, 3] {
        assert!(matches!(
            apply_err(&first, &delete(deleted)),
            ApplyError::Update { .. }
        ));
    }
}
