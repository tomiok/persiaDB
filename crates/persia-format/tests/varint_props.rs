//! Property tests for varints (ROADMAP 1.1.2.2): round-trip, canonicality, and agreement with a naive
//! reference decoder. Golden vectors live in `src/varint.rs` unit tests.

// Test-only helpers index and unwrap on purpose (see reader_writer_props.rs).
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]

use persia_format::varint::encoded_len;
use persia_format::{Corruption, Error, Reader, Writer};
use proptest::prelude::*;

/// Values clustered around every 7-bit length boundary, plus uniform ones.
fn interesting_u64() -> impl Strategy<Value = u64> {
    prop_oneof![
        any::<u64>(),
        (0_u32..64, -2_i64..=2)
            .prop_map(|(shift, delta)| (1_u64 << shift).wrapping_add_signed(delta)),
        (1_u32..=9).prop_map(|n| (1_u64 << (7 * n)) - 1),
        Just(u64::MAX),
        Just(0),
    ]
}

fn encode(value: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_varint_u64(value);
    w.into_inner()
}

/// Naive reference: accumulate groups in a u128 with no limits, then classify.
fn reference(bytes: &[u8], bits: u32) -> Result<(u64, usize), Corruption> {
    let mut value: u128 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if i >= 10 {
            break;
        }
        value |= u128::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            let len = i + 1;
            let max_len = bits.div_ceil(7) as usize;
            if len > max_len || value >> bits != 0 {
                return Err(Corruption::VarintOverflow { bits });
            }
            if b == 0 && len > 1 {
                return Err(Corruption::OverlongVarint);
            }
            return Ok((value as u64, len));
        }
        if i + 1 == bits.div_ceil(7) as usize {
            return Err(Corruption::VarintOverflow { bits });
        }
    }
    let available = bytes.len() as u64;
    Err(Corruption::UnexpectedEof {
        needed: available + 1,
        available,
    })
}

proptest! {
    #[test]
    fn u64_round_trip(value in interesting_u64(), base in 0_u64..(1 << 40)) {
        let bytes = encode(value);
        prop_assert_eq!(bytes.len(), encoded_len(value));
        let mut r = Reader::with_base_offset(&bytes, base);
        prop_assert_eq!(r.read_varint_u64(), Ok(value));
        prop_assert!(r.is_empty());
    }

    #[test]
    fn u32_round_trip(value in any::<u32>()) {
        let mut w = Writer::new();
        w.write_varint_u32(value);
        let bytes = w.into_inner();
        prop_assert_eq!(&bytes, &encode(u64::from(value)), "u32 and u64 encodings agree");
        prop_assert_eq!(Reader::new(&bytes).read_varint_u32(), Ok(value));
    }

    /// Canonicality: any byte string the decoder accepts is exactly the encoding of the decoded value,
    /// so every value has one valid encoding.
    #[test]
    fn accepted_bytes_are_the_canonical_encoding(bytes in prop::collection::vec(any::<u8>(), 0..14)) {
        let mut r = Reader::new(&bytes);
        if let Ok(value) = r.read_varint_u64() {
            let canonical = encode(value);
            prop_assert_eq!(&bytes[..r.position()], canonical.as_slice());
        } else {
            prop_assert_eq!(r.position(), 0, "failed reads do not move the cursor");
        }
    }

    /// The real decoder agrees with the naive reference on arbitrary input, for both widths,
    /// including the error kind and the error offset (the varint's start).
    #[test]
    fn decoder_matches_reference(
        bytes in prop::collection::vec(prop_oneof![3 => 0x80_u8..=0xff, 1 => any::<u8>()], 0..14),
        base in 0_u64..(1 << 40),
    ) {
        for bits in [32, 64] {
            let mut r = Reader::with_base_offset(&bytes, base);
            let got = if bits == 32 { r.read_varint_u32().map(u64::from) } else { r.read_varint_u64() };
            let expected = reference(&bytes, bits);
            match (&got, &expected) {
                (Ok(v), Ok((ev, len))) => {
                    prop_assert_eq!(v, ev);
                    prop_assert_eq!(r.position(), *len);
                }
                (Err(Error::Corrupt { offset, reason }), Err(er)) => {
                    prop_assert_eq!(*offset, base);
                    prop_assert_eq!(reason, er);
                    prop_assert_eq!(r.position(), 0);
                }
                _ => prop_assert!(false, "bits={} got={:?} expected={:?}", bits, got, expected),
            }
        }
    }

    /// A stream of varints decodes back in order; truncating it anywhere yields a clean prefix then EOF.
    #[test]
    fn streams_and_truncation(values in prop::collection::vec(interesting_u64(), 1..20), cut in any::<prop::sample::Index>()) {
        let mut w = Writer::new();
        for v in &values {
            w.write_varint_u64(*v);
        }
        let bytes = w.into_inner();
        let cut = cut.index(bytes.len() + 1);
        let mut r = Reader::new(&bytes[..cut]);
        for v in &values {
            match r.read_varint_u64() {
                Ok(got) => prop_assert_eq!(got, *v),
                Err(Error::Corrupt { reason: Corruption::UnexpectedEof { .. }, .. }) => break,
                Err(e) => prop_assert!(false, "unexpected {:?}", e),
            }
        }
    }
}
