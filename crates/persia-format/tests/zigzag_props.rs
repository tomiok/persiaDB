//! Property tests for zigzag (ROADMAP 1.1.3): bijection, magnitude ordering, and signed-varint round-trip.

// Test-only: the reference unwraps on purpose (see reader_writer_props.rs).
#![allow(clippy::unwrap_used)]

use persia_format::zigzag::{decode_i32, decode_i64, encode_i32, encode_i64};
use persia_format::{Reader, Writer};
use proptest::prelude::*;

/// Reference written from the SPEC §4.1 definition, in `i128` so nothing can overflow:
/// non-negative `n` maps to `2n`, negative `n` to `-2n - 1`.
fn reference(n: i64) -> u64 {
    let n = i128::from(n);
    let z = if n >= 0 { 2 * n } else { -2 * n - 1 };
    u64::try_from(z).unwrap()
}

proptest! {
    #[test]
    fn i64_matches_reference_and_round_trips(n in any::<i64>()) {
        prop_assert_eq!(encode_i64(n), reference(n));
        prop_assert_eq!(decode_i64(encode_i64(n)), n);
    }

    #[test]
    fn i32_agrees_with_i64_and_round_trips(n in any::<i32>()) {
        prop_assert_eq!(u64::from(encode_i32(n)), encode_i64(i64::from(n)));
        prop_assert_eq!(decode_i32(encode_i32(n)), n);
    }

    #[test]
    fn every_unsigned_value_decodes_to_its_preimage(u in any::<u64>(), v in any::<u32>()) {
        prop_assert_eq!(encode_i64(decode_i64(u)), u);
        prop_assert_eq!(encode_i32(decode_i32(v)), v);
    }

    #[test]
    fn smaller_magnitude_never_encodes_larger(a in any::<i64>(), b in any::<i64>()) {
        if a.unsigned_abs() < b.unsigned_abs() {
            prop_assert!(encode_i64(a) < encode_i64(b));
        }
    }

    #[test]
    fn signed_varints_round_trip_in_a_stream(values in proptest::collection::vec(any::<(i32, i64)>(), 0..64)) {
        let mut w = Writer::new();
        for &(a, b) in &values {
            w.write_varint_i32(a);
            w.write_varint_i64(b);
        }
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes);
        for &(a, b) in &values {
            prop_assert_eq!(r.read_varint_i32(), Ok(a));
            prop_assert_eq!(r.read_varint_i64(), Ok(b));
        }
        prop_assert!(r.is_empty());
    }
}
