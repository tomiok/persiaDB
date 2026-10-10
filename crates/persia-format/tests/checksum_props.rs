//! Property tests for CRC32C (ROADMAP 1.3.1): agreement with a bitwise reference written from the
//! Castagnoli definition, and streaming over any split equal to one-shot.

// Test-only helpers index on purpose (see reader_writer_props.rs).
#![allow(clippy::indexing_slicing)]

use persia_format::checksum::{Crc32c, crc32c, verify_crc32c};
use proptest::prelude::*;
use proptest::sample::Index;

/// Bit-at-a-time CRC32C: reflected polynomial 0x82F63B78, initial value and final XOR `0xFFFF_FFFF`.
fn reference_crc32c(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[test]
fn reference_matches_check_value() {
    assert_eq!(reference_crc32c(b"123456789"), 0xe306_9283);
}

proptest! {
    #[test]
    fn matches_reference(data in proptest::collection::vec(any::<u8>(), 0..2048)) {
        prop_assert_eq!(crc32c(&data), reference_crc32c(&data));
    }

    #[test]
    fn streaming_matches_one_shot(data in proptest::collection::vec(any::<u8>(), 0..2048),
                                  cuts in proptest::collection::vec(any::<Index>(), 0..8)) {
        let mut cuts: Vec<usize> = cuts.iter().map(|c| c.index(data.len() + 1)).collect();
        cuts.sort_unstable();
        let mut crc = Crc32c::new();
        let mut start = 0;
        for cut in cuts.into_iter().chain([data.len()]) {
            crc.update(&data[start..cut]);
            start = cut;
        }
        prop_assert_eq!(crc.finish(), crc32c(&data));
    }

    #[test]
    fn verify_accepts_own_checksum_only(data in proptest::collection::vec(any::<u8>(), 0..512),
                                        wrong: u32, offset: u64) {
        let crc = crc32c(&data);
        prop_assert_eq!(verify_crc32c(&data, crc, offset), Ok(()));
        prop_assume!(wrong != crc);
        prop_assert!(verify_crc32c(&data, wrong, offset).is_err());
    }
}
