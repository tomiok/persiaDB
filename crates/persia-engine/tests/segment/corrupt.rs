//! Corrupt segments (SPEC §5, CLAUDE.md "Safety & errors"): every byte of a segment is covered by a CRC or a layout
//! rule, so flipping any one of them is reported as corruption by open or by reading everything back, and no input
//! makes the reader panic.

use persia_engine::segment::WriteOptions;
use proptest::prelude::*;

use super::read::read_back;
use super::{build, fixture_path, golden_docs, schema};

/// Bit flips at every `stride`-th byte (every byte with the `slow` feature) of the golden segment.
#[test]
fn every_flipped_byte_is_detected() {
    let schema = schema();
    let bytes = std::fs::read(fixture_path()).unwrap();
    read_back(&schema, &bytes).unwrap();
    let stride = if cfg!(feature = "slow") { 1 } else { 7 };
    for i in (0..bytes.len()).step_by(stride) {
        let mut b = bytes.clone();
        b[i] ^= 1 << (i % 8);
        match read_back(&schema, &b) {
            Ok(_) => panic!("flip at byte {i} of {} went unnoticed", bytes.len()),
            Err(e) => assert!(e.is_corruption(), "byte {i}: {e}"),
        }
    }
}

#[test]
fn every_truncation_is_detected() {
    let schema = schema();
    let bytes = build(
        &schema,
        &golden_docs(&schema)[..40],
        WriteOptions {
            block_size: 512,
            ..WriteOptions::default()
        },
    );
    for len in 0..bytes.len() {
        let err = read_back(&schema, &bytes[..len]).unwrap_err();
        assert!(err.is_corruption(), "truncated to {len}: {err}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 2048 } else { 128 }, ..ProptestConfig::default() })]

    /// Random overwrites of a small segment, CRCs left as they are: errors, never panics.
    #[test]
    fn random_damage_never_panics(
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
    ) {
        static BYTES: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        let schema = schema();
        let bytes = BYTES.get_or_init(|| build(&schema, &golden_docs(&schema)[..60], WriteOptions { block_size: 256, ..WriteOptions::default() }));
        let mut b = bytes.clone();
        for (at, v) in edits {
            let i = at.index(b.len());
            b[i] = v;
        }
        if let Err(e) = read_back(&schema, &b) {
            prop_assert!(e.is_corruption(), "{e}");
        } else {
            prop_assert_eq!(&b, bytes, "damage went unnoticed");
        }
    }
}
