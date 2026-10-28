//! The tombstone sidecar encoding (ROADMAP 6.4.2; SPEC §8.3, ADR-0030).
//!
//! Invariants:
//! 1. Golden files: the sidecars below encode to exactly the bytes in `tests/fixtures/tombstones/v1/` and decode back
//!    to themselves. They are never regenerated once released (SPEC §15); `PERSIA_WRITE_TOMBSTONE_FIXTURES=1` writes
//!    the missing ones.
//! 2. Round trip: any valid sidecar decodes to itself, and its encoding is stable.
//! 3. Corruption: any truncation or extension of a valid encoding is rejected as corruption, never a panic.
//!
//! Seeds: proptest's default config; `PROPTEST_RNG_SEED` and `PROPTEST_CASES` override it.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::path::PathBuf;

use persia_engine::manifest::SegmentId;
use persia_engine::tombstone::TombstoneSidecar;
use proptest::prelude::*;
use roaring::RoaringBitmap;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/tombstones/v1")
}

/// One deleted document: an array container.
fn golden_sparse() -> TombstoneSidecar {
    let id = SegmentId::new(0x0123_4567_89ab_cdef_0011_2233_4455_6677);
    TombstoneSidecar::new(id, 1, 10, RoaringBitmap::from_iter([3])).unwrap()
}

/// A run container (a bulk delete), a bitmap container and an array container, at version 7.
fn golden_mixed() -> TombstoneSidecar {
    let mut deleted = RoaringBitmap::new();
    deleted.insert_range(0..70_000);
    deleted.extend((131_072..196_608).step_by(3));
    deleted.extend([200_000, 250_001]);
    TombstoneSidecar::new(SegmentId::new(u128::MAX), 7, 300_000, deleted).unwrap()
}

#[test]
fn golden_fixtures_decode_and_reencode_byte_for_byte() {
    let write = std::env::var_os("PERSIA_WRITE_TOMBSTONE_FIXTURES").is_some();
    for (name, sidecar) in [
        ("sparse.tomb", golden_sparse()),
        ("mixed.tomb", golden_mixed()),
    ] {
        let path = fixture_dir().join(name);
        let encoded = sidecar.encode();
        if write && !path.exists() {
            std::fs::create_dir_all(fixture_dir()).unwrap();
            std::fs::write(&path, &encoded).unwrap();
        }
        let stored = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (PERSIA_WRITE_TOMBSTONE_FIXTURES=1 writes it)",
                path.display()
            )
        });
        assert_eq!(
            TombstoneSidecar::decode(&stored).unwrap(),
            sidecar,
            "{name}"
        );
        assert_eq!(encoded, stored, "{name}: encoding changed");
    }
}

#[test]
fn sparse_golden_bytes() {
    let bytes = golden_sparse().encode();
    let mut expected = b"PTMB".to_vec();
    expected.extend(1_u32.to_le_bytes()); // encoding version
    expected.extend(0x0123_4567_89ab_cdef_0011_2233_4455_6677_u128.to_le_bytes());
    expected.extend(1_u64.to_le_bytes()); // sidecar version
    expected.extend(10_u32.to_le_bytes()); // doc_count
    expected.extend(1_u32.to_le_bytes()); // deleted
    // Portable roaring: no-run cookie, one container (key 0, cardinality 1), its offset, the value 3.
    expected.extend(12_346_u32.to_le_bytes());
    expected.extend(1_u32.to_le_bytes());
    expected.extend([0, 0, 0, 0]);
    expected.extend(16_u32.to_le_bytes());
    expected.extend(3_u16.to_le_bytes());
    let crc = persia_format::checksum::crc32c(&expected);
    expected.extend(crc.to_le_bytes());
    assert_eq!(bytes, expected);
}

fn sidecar() -> impl Strategy<Value = TombstoneSidecar> {
    let docids = prop_oneof![
        prop::collection::vec(0..1_000_u32, 1..50),
        prop::collection::vec(0..300_000_u32, 1..2_000),
        (0..200_000_u32, 1..80_000_u32).prop_map(|(s, n)| (s..s + n).collect()),
    ];
    (any::<u128>(), 1..u64::MAX, docids, 0..1_000_u32).prop_map(|(id, version, docids, extra)| {
        let deleted: RoaringBitmap = docids.into_iter().collect();
        let doc_count = deleted.max().unwrap() + 1 + extra;
        TombstoneSidecar::new(SegmentId::new(id), version, doc_count, deleted).unwrap()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn round_trips(s in sidecar()) {
        let bytes = s.encode();
        let back = TombstoneSidecar::decode(&bytes).unwrap();
        prop_assert_eq!(&back, &s);
        prop_assert_eq!(back.encode(), bytes);
    }

    #[test]
    fn truncations_and_extensions_are_corruption(s in sidecar(), cut in any::<prop::sample::Index>(), extra in 1..9_usize) {
        let bytes = s.encode();
        let short = &bytes[..cut.index(bytes.len())];
        prop_assert!(TombstoneSidecar::decode(short).unwrap_err().is_corruption());
        let mut long = bytes.clone();
        long.extend(std::iter::repeat_n(0, extra));
        prop_assert!(TombstoneSidecar::decode(&long).unwrap_err().is_corruption());
    }
}
