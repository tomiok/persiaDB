//! Property tests for bit-packing (ROADMAP 1.2.2.4): agreement with a bit-by-bit reference written from
//! SPEC §5.2.1, round-trip at every width, minimal width, and arbitrary bytes decoding without panics.

// Test-only helpers index and cast on purpose (see reader_writer_props.rs).
#![allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::unwrap_used
)]

use persia_format::bitpack::{BLOCK_LEN, BitWidth};
use persia_format::{Corruption, EncodeError, Error, Reader, Writer};
use proptest::prelude::*;

/// Where bit `b` of value `i` lives, straight from the SPEC: lane `i % 4`, slot `i / 4`; within the lane's
/// bit stream the bit is at `slot * w + b`, i.e. word row `pos / 32`, bit `pos % 32`; word `row * 4 + lane`.
fn bit_location(i: usize, b: usize, w: usize) -> (usize, usize) {
    let (lane, slot) = (i % 4, i / 4);
    let pos = slot * w + b;
    ((pos / 32) * 4 + lane, pos % 32)
}

fn reference_pack(values: &[u32; BLOCK_LEN], w: usize) -> Vec<u8> {
    let mut words = vec![0_u32; 4 * w];
    for (i, &v) in values.iter().enumerate() {
        for b in 0..w {
            let (word, bit) = bit_location(i, b, w);
            words[word] |= ((v >> b) & 1) << bit;
        }
    }
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn reference_unpack(bytes: &[u8], w: usize) -> [u32; BLOCK_LEN] {
    core::array::from_fn(|i| {
        (0..w).fold(0, |acc, b| {
            let (word, bit) = bit_location(i, b, w);
            let byte = bytes[word * 4 + bit / 8];
            acc | (u32::from((byte >> (bit % 8)) & 1) << b)
        })
    })
}

/// A width plus a block whose values fit in it, biased towards the top of the range.
fn block_at_width() -> impl Strategy<Value = (u8, [u32; BLOCK_LEN])> {
    (0_u8..=32).prop_flat_map(|w| {
        let max = ((1_u64 << w) - 1) as u32;
        let value = prop_oneof![0..=max, Just(max), Just(0)];
        let block = proptest::collection::vec(value, BLOCK_LEN)
            .prop_map(|v| <[u32; BLOCK_LEN]>::try_from(v).unwrap());
        (Just(w), block)
    })
}

proptest! {
    #[test]
    fn pack_matches_reference_and_round_trips((w, values) in block_at_width()) {
        let width = BitWidth::new(w).unwrap();
        let mut writer = Writer::new();
        writer.write_packed_block(&values, width).unwrap();
        let bytes = writer.into_inner();
        prop_assert_eq!(bytes.len(), width.packed_len());
        prop_assert_eq!(&bytes, &reference_pack(&values, usize::from(w)));

        let mut out = [0; BLOCK_LEN];
        let mut r = Reader::new(&bytes);
        r.read_packed_block(width, &mut out).unwrap();
        prop_assert!(r.is_empty());
        prop_assert_eq!(out, values);
    }

    #[test]
    fn minimal_width_fits_and_one_less_does_not((_, values) in block_at_width()) {
        let width = BitWidth::of(&values);
        prop_assert!(Writer::new().write_packed_block(&values, width).is_ok());
        if let Some(narrower) = width.get().checked_sub(1).and_then(BitWidth::new) {
            prop_assert!(Writer::new().write_packed_block(&values, narrower).is_err());
        }
    }

    #[test]
    fn arbitrary_bytes_decode_like_the_reference(w in 0_u8..=32, seed in proptest::collection::vec(any::<u8>(), 512)) {
        let width = BitWidth::new(w).unwrap();
        let bytes = &seed[..width.packed_len()];
        let mut out = [0; BLOCK_LEN];
        Reader::new(bytes).read_packed_block(width, &mut out).unwrap();
        prop_assert_eq!(out, reference_unpack(bytes, usize::from(w)));
        // The layout is a bijection at a fixed width: re-packing gives back the same bytes.
        let mut writer = Writer::new();
        writer.write_packed_block(&out, width).unwrap();
        prop_assert_eq!(writer.as_slice(), bytes);
    }
}

/// Exhaustive over widths: for each width every single bit position of every value is set alone, so a
/// misplaced bit anywhere in the layout is caught.
#[test]
fn every_single_bit_lands_where_the_spec_says() {
    for w in 1..=32_usize {
        let width = BitWidth::new(w as u8).unwrap();
        for i in 0..BLOCK_LEN {
            for b in 0..w {
                let mut values = [0_u32; BLOCK_LEN];
                values[i] = 1 << b;
                let mut writer = Writer::new();
                writer.write_packed_block(&values, width).unwrap();
                let bytes = writer.into_inner();
                let (word, bit) = bit_location(i, b, w);
                let set: Vec<usize> = (0..bytes.len() * 8)
                    .filter(|&k| bytes[k / 8] >> (k % 8) & 1 == 1)
                    .collect();
                assert_eq!(set, [word * 32 + bit], "w={w} i={i} b={b}");
            }
        }
    }
}

proptest! {
    /// A width narrower than the minimal one is rejected at the first value that does not fit, and the
    /// writer keeps exactly what it held before.
    #[test]
    fn too_narrow_width_fails_at_first_wide_value_and_writes_nothing(
        (_, values) in block_at_width(),
        narrower in any::<prop::sample::Index>(),
        prefix in proptest::collection::vec(any::<u8>(), 0..8),
    ) {
        let minimal = BitWidth::of(&values).get();
        prop_assume!(minimal > 0);
        let w = narrower.index(usize::from(minimal)) as u8;
        let mask = (1_u64 << w) - 1;
        let index = values.iter().position(|&v| u64::from(v) > mask).unwrap();
        let mut writer = Writer::new();
        writer.write_bytes(&prefix);
        prop_assert_eq!(
            writer.write_packed_block(&values, BitWidth::new(w).unwrap()),
            Err(EncodeError::ValueTooWide { index, width: w })
        );
        prop_assert_eq!(writer.as_slice(), prefix.as_slice());
    }
}

/// Every possible width byte: 0..=32 is accepted and consumed, anything else is `InvalidBitWidth` at the
/// byte's absolute offset with the reader unmoved (SPEC §5.2.1).
#[test]
fn read_bit_width_accepts_exactly_0_to_32() {
    for raw in 0..=u8::MAX {
        let bytes = [0xee, raw, 0x55];
        let mut r = Reader::with_base_offset(&bytes, 1000);
        r.skip(1).unwrap();
        match r.read_bit_width() {
            Ok(width) => {
                assert!(raw <= 32);
                assert_eq!(width.get(), raw);
                assert_eq!(r.rest(), [0x55]);
            }
            Err(e) => {
                assert!(raw > 32);
                assert_eq!(
                    e,
                    Error::Corrupt {
                        offset: 1001,
                        reason: Corruption::InvalidBitWidth {
                            width: raw,
                            max: 32
                        },
                    }
                );
                assert_eq!(r.position(), 1);
            }
        }
    }
    let mut r = Reader::with_base_offset(&[], 9);
    assert_eq!(
        r.read_bit_width(),
        Err(Error::Corrupt {
            offset: 9,
            reason: Corruption::UnexpectedEof {
                needed: 1,
                available: 0
            },
        })
    );
}

/// Every width, every short length: `UnexpectedEof` at the block's absolute offset with exact sizes, and
/// neither the reader nor the output buffer changes.
#[test]
fn short_packed_blocks_fail_atomically_at_every_width_and_length() {
    let bytes = [0xa5_u8; 1 + 512];
    for w in 1..=32_u8 {
        let width = BitWidth::new(w).unwrap();
        for len in 0..width.packed_len() {
            let mut r = Reader::with_base_offset(&bytes[..=len], 70);
            r.skip(1).unwrap();
            let mut out = [7; BLOCK_LEN];
            assert_eq!(
                r.read_packed_block(width, &mut out),
                Err(Error::Corrupt {
                    offset: 71,
                    reason: Corruption::UnexpectedEof {
                        needed: width.packed_len() as u64,
                        available: len as u64,
                    },
                }),
                "w {w} len {len}"
            );
            assert_eq!(r.position(), 1);
            assert_eq!(out, [7; BLOCK_LEN]);
        }
    }
}
