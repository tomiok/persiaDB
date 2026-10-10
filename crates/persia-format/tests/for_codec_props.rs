//! Property tests for FOR blocks and order-preserving keys (ROADMAP 1.2.4): round-trip and random access for
//! clustered and full-range keys, agreement with a bit-by-bit reference written from SPEC §5.2.1, minimal
//! width, key order, and arbitrary bytes never panicking.

// Test-only helpers index and unwrap on purpose (see reader_writer_props.rs).
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::cast_possible_truncation
)]

use std::cmp::Ordering;

use persia_format::for_codec::{f64_to_key, i64_to_key, key_to_f64, key_to_i64};
use persia_format::{Reader, Writer};
use proptest::prelude::*;

/// Key sets: clustered (small widths), full-range, near the top (overflow edge), constant.
fn keys() -> impl Strategy<Value = Vec<u64>> {
    let n = 0_usize..300;
    prop_oneof![
        (
            any::<u64>(),
            proptest::collection::vec(0_u64..5000, n.clone())
        )
            .prop_map(|(base, d)| d.into_iter().map(|x| base.saturating_add(x)).collect()),
        proptest::collection::vec(any::<u64>(), n.clone()),
        proptest::collection::vec((0_u64..64).prop_map(|d| u64::MAX - d), n.clone()),
        (any::<u64>(), n).prop_map(|(k, n)| vec![k; n]),
    ]
}

fn encode(keys: &[u64]) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_for_block(keys).unwrap();
    w.into_inner()
}

/// Bit-by-bit reference decoder: header fields at fixed offsets, value i at stream bits [i·w, (i+1)·w).
fn reference_decode(bytes: &[u8]) -> Vec<u64> {
    let min = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let w = usize::from(bytes[12]);
    let packed = &bytes[13..];
    (0..count)
        .map(|i| {
            let raw = (0..w).fold(0_u64, |acc, b| {
                let bit = i * w + b;
                acc | (u64::from(packed[bit / 8] >> (bit % 8) & 1) << b)
            });
            min + raw
        })
        .collect()
}

proptest! {
    #[test]
    fn blocks_round_trip_with_random_access(keys in keys()) {
        let bytes = encode(&keys);
        let mut r = Reader::new(&bytes);
        let block = r.read_for_block().unwrap();
        prop_assert!(r.is_empty());
        prop_assert_eq!(block.len(), keys.len());
        prop_assert_eq!(block.iter().collect::<Vec<_>>(), keys.clone());
        prop_assert_eq!(reference_decode(&bytes), keys.clone());
        for (i, &k) in keys.iter().enumerate() {
            prop_assert_eq!(block.get(i), Some(k));
        }
        prop_assert_eq!(block.get(keys.len()), None);
    }

    #[test]
    fn header_uses_true_min_and_minimal_width(keys in keys()) {
        let bytes = encode(&keys);
        let min = keys.iter().copied().min().unwrap_or(0);
        let range = keys.iter().copied().max().unwrap_or(0) - min;
        prop_assert_eq!(&bytes[0..8], &min.to_le_bytes());
        prop_assert_eq!(u32::from(bytes[12]), range.bit_width());
        prop_assert_eq!(bytes.len(), 13 + (keys.len() as u64 * u64::from(bytes[12])).div_ceil(8) as usize);
    }

    #[test]
    fn i64_keys_order_and_round_trip(a in any::<i64>(), b in any::<i64>()) {
        prop_assert_eq!(i64_to_key(a).cmp(&i64_to_key(b)), a.cmp(&b));
        prop_assert_eq!(key_to_i64(i64_to_key(a)), a);
    }

    #[test]
    fn f64_keys_follow_total_cmp_and_round_trip(a in any::<u64>(), b in any::<f64>()) {
        let a = f64::from_bits(a); // every bit pattern, NaNs included
        prop_assert_eq!(f64_to_key(a).cmp(&f64_to_key(b)), a.total_cmp(&b));
        prop_assert_eq!(key_to_f64(f64_to_key(a)).to_bits(), a.to_bits());
        if f64_to_key(a).cmp(&f64_to_key(b)) == Ordering::Equal {
            prop_assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn every_key_decodes_to_its_preimage(k in any::<u64>()) {
        prop_assert_eq!(i64_to_key(key_to_i64(k)), k);
        prop_assert_eq!(f64_to_key(key_to_f64(k)), k);
    }

    #[test]
    fn arbitrary_bytes_never_panic_and_accepted_blocks_are_sane(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let mut r = Reader::new(&bytes);
        if let Ok(block) = r.read_for_block() {
            // Width 0 lets 13 bytes claim 2^32 - 1 values (callers bound `count`): only spot-check those.
            if block.width() == 0 {
                prop_assert!(block.get(block.len().saturating_sub(1)).is_none_or(|v| v == block.min()));
                return Ok(());
            }
            let values: Vec<u64> = block.iter().collect();
            prop_assert_eq!(values.len(), block.len());
            prop_assert!(values.iter().all(|&v| v >= block.min()));
            prop_assert_eq!(values, reference_decode(&bytes));
        } else {
            prop_assert_eq!(r.position(), 0);
        }
    }
}
