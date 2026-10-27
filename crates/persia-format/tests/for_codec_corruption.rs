//! Corruption and error-reporting tests for FOR blocks (ROADMAP 1.2.4, SPEC §5.2.1 "Frame-of-reference (FOR)
//! block").
//!
//! Invariants:
//! 1. A reader built from the SPEC byte table agrees with `read_for_block` on valid, mutated and random input:
//!    same values, same error kind and absolute offset (`Reader::with_base_offset`), same bytes consumed.
//!    `InvalidCount` (outside 1..=1024) points at the count field and wins over a bad width,
//!    `InvalidBitWidth` points at the width byte, `NonZeroReserved` at the last packed byte, `ValueOverflow`
//!    at the block start.
//! 2. Reads are atomic: on any error the reader has not moved.
//! 3. Every single-bit flip in a valid block is detected or has exactly the predicted effect: a flip in a used
//!    packed bit changes exactly one value by that bit, a flip in `min` shifts every value, and a flip in the
//!    unused high bits of the last byte is always `NonZeroReserved`.
//! 4. Encoders accept exactly 1..=1024 keys and append nothing otherwise.

// Test-only helpers index, cast and unwrap on purpose (see reader_writer_props.rs).
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::many_single_char_names
)]

mod common;

use common::{apply, corrupt, eof, mutation, placement};
use persia_format::for_codec::{MAX_WIDTH, VALUES_PER_BLOCK};
use persia_format::{Corruption, EncodeError, Error, Reader, Writer};
use proptest::prelude::*;

/// What a FOR block decodes to: header fields, values and bytes consumed.
#[derive(Debug, PartialEq)]
struct Decoded {
    min: u64,
    count: u32,
    width: u8,
    values: Vec<u64>,
    used: usize,
}

/// Value `i`'s raw bits, LSB first from stream bit `i·w` (SPEC §5.2.1).
fn raw_at(packed: &[u8], i: usize, w: usize) -> u64 {
    (0..w).fold(0, |acc, b| {
        let bit = i * w + b;
        acc | (u64::from(packed[bit / 8] >> (bit % 8) & 1) << b)
    })
}

/// The SPEC's byte table, checked field by field. `start` is the absolute offset of `bytes[0]`.
fn reference_for(bytes: &[u8], start: u64) -> Result<Decoded, Error> {
    let len = bytes.len();
    if len < 8 {
        return Err(eof(start, 8, len));
    }
    let min = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    if len < 12 {
        return Err(eof(start + 8, 4, len - 8));
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if !(1..=1024).contains(&count) {
        return Err(corrupt(
            start + 8,
            Corruption::InvalidCount {
                count,
                min: 1,
                max: 1024,
            },
        ));
    }
    if len < 13 {
        return Err(eof(start + 12, 1, 0));
    }
    let width = bytes[12];
    if width > 64 {
        return Err(corrupt(
            start + 12,
            Corruption::InvalidBitWidth { width, max: 64 },
        ));
    }
    let bits = u64::from(count) * u64::from(width);
    let packed_len = bits.div_ceil(8) as usize;
    if len - 13 < packed_len {
        return Err(eof(start + 13, packed_len, len - 13));
    }
    let packed = &bytes[13..13 + packed_len];
    if bits % 8 != 0 {
        let last = packed[packed_len - 1];
        if last >> (bits % 8) != 0 {
            return Err(corrupt(
                start + 13 + packed_len as u64 - 1,
                Corruption::NonZeroReserved { value: last },
            ));
        }
    }
    let values = (0..count as usize)
        .map(|i| {
            u64::try_from(u128::from(min) + u128::from(raw_at(packed, i, usize::from(width))))
                .map_err(|_| corrupt(start, Corruption::ValueOverflow { bits: 64 }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Decoded {
        min,
        count,
        width,
        values,
        used: 13 + packed_len,
    })
}

/// Reads with the crate at absolute offset `base + skip`, checking atomicity and that `get`, `iter` and `decode_into`
/// agree.
fn library_for(bytes: &[u8], base: u64, skip: usize) -> Result<Decoded, Error> {
    let mut r = Reader::with_base_offset(bytes, base);
    r.skip(skip).unwrap();
    match r.read_for_block() {
        Ok(block) => {
            let len = block.len();
            assert_eq!(block.is_empty(), len == 0);
            assert_eq!(block.get(len), None);
            assert_eq!(block.get(usize::MAX), None);
            let values: Vec<u64> = block.iter().collect();
            assert_eq!(values.len(), len);
            let mut decoded = Vec::new();
            block.decode_into(&mut decoded);
            assert_eq!(decoded, values, "decode_into disagrees with iter");
            for (i, &v) in values.iter().enumerate() {
                assert_eq!(block.get(i), Some(v), "get({i}) disagrees with iter");
                assert!(v >= block.min());
            }

            Ok(Decoded {
                min: block.min(),
                count: u32::try_from(len).unwrap(),
                width: block.width(),
                values,
                used: r.position() - skip,
            })
        }
        Err(e) => {
            assert_eq!(r.position(), skip, "failed read moved the reader");
            Err(e)
        }
    }
}

fn encode(keys: &[u64]) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_for_block(keys).unwrap();
    w.into_inner()
}

/// Keys whose FOR block has exactly width `w` (for two or more keys): `min` and `min + 2^w - 1` both occur.
/// The minimum is 0, arbitrary, or as high as the width allows (the overflow edge).
fn keys_at_width() -> impl Strategy<Value = (u8, Vec<u64>)> {
    (
        0..=MAX_WIDTH,
        prop_oneof![9 => 2_usize..40, 1 => Just(VALUES_PER_BLOCK)],
    )
        .prop_flat_map(|(w, n)| {
            let mask = if w == 0 {
                0
            } else {
                u64::MAX >> (64 - u32::from(w))
            };
            let min = prop_oneof![Just(0), 0..=u64::MAX - mask, Just(u64::MAX - mask)];
            let raw = prop_oneof![0..=mask, Just(mask), Just(0)];
            (
                Just(w),
                min,
                proptest::collection::vec(raw, n),
                any::<proptest::sample::Index>(),
            )
                .prop_map(move |(w, min, mut raws, at)| {
                    // Pin the extremes at random places so the width is exactly w.
                    let i = at.index(raws.len());
                    raws[i] = mask;
                    let j = (i + 1) % raws.len();
                    raws[j] = 0;
                    (w, raws.into_iter().map(|r| min + r).collect())
                })
        })
}

proptest! {
    #[test]
    fn round_trips_at_every_width_including_the_top_of_u64((w, keys) in keys_at_width()) {
        let bytes = encode(&keys);
        prop_assert_eq!(bytes[12], w);
        let decoded = library_for(&bytes, 0, 0).unwrap();
        prop_assert_eq!(decoded.used, bytes.len());
        prop_assert_eq!(decoded.min, *keys.iter().min().unwrap());
        prop_assert_eq!(decoded.count as usize, keys.len());
        prop_assert_eq!(decoded.values, keys);
    }

    #[test]
    fn reader_matches_spec_reference_on_mutated_blocks(
        (_, keys) in keys_at_width(),
        m in mutation(),
        (base, prefix) in placement(),
    ) {
        let block = apply(encode(&keys), &m);
        let mut bytes = prefix.clone();
        bytes.extend(&block);
        let start = base + prefix.len() as u64;
        prop_assert_eq!(library_for(&bytes, base, prefix.len()), reference_for(&block, start));
    }

    /// Random headers: the width byte and count are uniform, so every error path is reached.
    #[test]
    fn reader_matches_spec_reference_on_random_headers(
        min in prop_oneof![any::<u64>(), (0_u64..1 << 16).prop_map(|d| u64::MAX - d)],
        count in prop_oneof![0_u32..64, 1000_u32..=1030, any::<u32>()],
        width in prop_oneof![0_u8..=MAX_WIDTH, any::<u8>()],
        packed in proptest::collection::vec(any::<u8>(), 0..600),
        (base, prefix) in placement(),
    ) {
        let mut block = Vec::new();
        block.extend(min.to_le_bytes());
        block.extend(count.to_le_bytes());
        block.push(width);
        block.extend(&packed);
        let mut bytes = prefix.clone();
        bytes.extend(&block);
        let start = base + prefix.len() as u64;
        prop_assert_eq!(library_for(&bytes, base, prefix.len()), reference_for(&block, start));
    }
}

/// Valid blocks covering every partial-last-byte shape, full width, and a minimum at the overflow edge.
fn sample_blocks() -> Vec<Vec<u64>> {
    vec![
        vec![10, 12, 11],                                  // w 2, 6 bits: 2 unused
        vec![0, 1, 2, 3, 4, 5, 7],                         // w 3, 21 bits: 3 unused
        (0..7).map(|i| 1000 + i * 1171).collect(),         // w 13, 91 bits: 5 unused
        vec![0, u64::MAX >> 1, 5],                         // w 63, 189 bits: 3 unused
        vec![u64::MAX, 0, 1 << 63],                        // w 64: no unused bits
        (0..5).map(|i| u64::MAX - 100 + i * 25).collect(), // w 7 near the top: flips can overflow
        vec![42; 5],                                       // w 0
    ]
}

/// Invariant 3, exhaustive over every bit of every sample block.
#[test]
fn every_bit_flip_is_detected_or_has_the_predicted_effect() {
    let base = 1 << 20;
    for keys in sample_blocks() {
        let bytes = encode(&keys);
        let min = *keys.iter().min().unwrap();
        let w = usize::from(bytes[12]);
        let bits = keys.len() * w;
        for bit in 0..bytes.len() * 8 {
            let mut flipped = bytes.clone();
            flipped[bit / 8] ^= 1 << (bit % 8);
            let result = library_for(&flipped, base, 0);
            assert_eq!(
                result,
                reference_for(&flipped, base),
                "keys {keys:?} bit {bit}"
            );
            let overflow = Err(corrupt(base, Corruption::ValueOverflow { bits: 64 }));
            match bit {
                // min: every value shifts by the same amount, or the block overflows.
                0..64 => {
                    let new_min = min ^ (1 << bit);
                    let expected: Option<Vec<u64>> = keys
                        .iter()
                        .map(|&k| (k - min).checked_add(new_min))
                        .collect();
                    match (expected, &result) {
                        (Some(e), Ok(d)) => assert_eq!(d.values, e, "bit {bit}"),
                        (None, r) => assert_eq!(r, &overflow, "bit {bit}"),
                        (Some(_), Err(e)) => panic!("bit {bit}: unexpected {e}"),
                    }
                }
                // count / width: whatever happens, the reference above already agreed.
                64..104 => {}
                _ => {
                    let k = bit - 104; // bit in the packed stream
                    if k < bits {
                        // A used bit: exactly one value changes, by that bit.
                        let (i, b) = (k / w, k % w);
                        let mut expected = keys.clone();
                        let raw = (expected[i] - min) ^ (1 << b);
                        match raw.checked_add(min) {
                            Some(v) => {
                                expected[i] = v;
                                let d = result.unwrap();
                                assert_eq!(d.values, expected, "bit {bit}");
                            }
                            None => assert_eq!(result, overflow, "bit {bit}"),
                        }
                    } else {
                        // An unused high bit of the last byte.
                        assert_eq!(
                            result,
                            Err(corrupt(
                                base + bytes.len() as u64 - 1,
                                Corruption::NonZeroReserved {
                                    value: flipped[bytes.len() - 1]
                                }
                            )),
                            "keys {keys:?} bit {bit}"
                        );
                    }
                }
            }
        }
    }
}

/// Invariant 1 at the overflow edge, for every width and every partial-last-byte length: the largest raw
/// value that fits is accepted and one more is `ValueOverflow` at the block start.
#[test]
fn overflow_is_detected_at_every_width() {
    for w in 1..=u32::from(MAX_WIDTH) {
        let mask = u64::MAX >> (64 - w);
        for count in 1..=9_usize {
            for (min, ok) in [(u64::MAX - mask, true), (u64::MAX - mask + 1, false)] {
                // Value count-1 has raw `mask`, the others 0.
                let mut packed = vec![0_u8; (count * w as usize).div_ceil(8)];
                for b in 0..w as usize {
                    let bit = (count - 1) * w as usize + b;
                    packed[bit / 8] |= 1 << (bit % 8);
                }
                let mut bytes = Vec::new();
                bytes.extend(min.to_le_bytes());
                bytes.extend((count as u32).to_le_bytes());
                bytes.push(w as u8);
                bytes.extend(&packed);
                let result = library_for(&bytes, 300, 0);
                assert_eq!(result, reference_for(&bytes, 300), "w {w} count {count}");
                if ok {
                    let values = result.unwrap().values;
                    assert_eq!(values[count - 1], u64::MAX, "w {w} count {count}");
                    assert!(values[..count - 1].iter().all(|&v| v == min));
                } else {
                    assert_eq!(
                        result,
                        Err(corrupt(300, Corruption::ValueOverflow { bits: 64 })),
                        "w {w} count {count}"
                    );
                }
            }
        }
    }
}

/// SPEC §5.2.1: `count` must be 1..=1024. Outside that it is `InvalidCount` at the count field (the
/// block start plus 8), checked before the width byte, so it wins over a bad or missing width. At both
/// bounds the block is read normally.
#[test]
fn count_is_bounded_and_checked_before_the_width() {
    let base = 1 << 40;
    for count in [0_u32, 1, 2, 1023, 1024, 1025, 4096, u32::MAX] {
        let in_range = (1..=1024).contains(&count);
        for width in [None, Some(65_u8), Some(0)] {
            let mut block = Vec::new();
            block.extend(7_u64.to_le_bytes());
            block.extend(count.to_le_bytes());
            block.extend(width);
            let result = library_for(&block, base, 0);
            assert_eq!(
                result,
                reference_for(&block, base),
                "count {count} width {width:?}"
            );
            let expected = match (in_range, width) {
                (false, _) => Err(corrupt(
                    base + 8,
                    Corruption::InvalidCount {
                        count,
                        min: 1,
                        max: 1024,
                    },
                )),
                (true, None) => Err(eof(base + 12, 1, 0)),
                (true, Some(65)) => Err(corrupt(
                    base + 12,
                    Corruption::InvalidBitWidth { width: 65, max: 64 },
                )),
                (true, Some(_)) => Ok(Decoded {
                    min: 7,
                    count,
                    width: 0,
                    values: vec![7; count as usize],
                    used: 13,
                }),
            };
            assert_eq!(result, expected, "count {count} width {width:?}");
        }
    }
}

/// The largest legal block at width 64 claims 8192 packed bytes: missing bytes are short input at the packed
/// data with exact sizes, not an allocation or an overflow.
#[test]
fn largest_claimed_lengths_are_short_input() {
    for width in [1_u8, 33, 64] {
        let mut bytes = Vec::new();
        bytes.extend(0_u64.to_le_bytes());
        bytes.extend(1024_u32.to_le_bytes());
        bytes.push(width);
        bytes.extend([0; 7]);
        assert_eq!(
            library_for(&bytes, 1 << 40, 0),
            Err(corrupt(
                (1 << 40) + 13,
                Corruption::UnexpectedEof {
                    needed: 128 * u64::from(width),
                    available: 7
                }
            ))
        );
    }
}

/// Invariant 4: the encoder takes 1..=1024 keys; anything else is `InvalidLen` and a writer that already
/// holds bytes keeps exactly those.
#[test]
fn encoder_accepts_exactly_1_to_1024_keys() {
    let prefix = [0xaa, 0xbb, 0xcc];
    for len in [0, 1, 1023, 1024, 1025, 5000] {
        let keys: Vec<u64> = (0..len as u64).map(|i| i * 3).collect();
        let mut w = Writer::new();
        w.write_bytes(&prefix);
        let result = w.write_for_block(&keys);
        if (1..=VALUES_PER_BLOCK).contains(&len) {
            assert_eq!(result, Ok(()));
            let decoded = library_for(w.as_slice(), 0, prefix.len()).unwrap();
            assert_eq!(decoded.values, keys);
            assert_eq!(prefix.len() + decoded.used, w.as_slice().len());
        } else {
            assert_eq!(
                result,
                Err(EncodeError::InvalidLen {
                    len,
                    min: 1,
                    max: 1024
                })
            );
            assert_eq!(w.as_slice(), prefix);
        }
    }
}

/// Golden: width 3 crosses byte boundaries (SPEC §5.2.1, LSB first). Keys 0..=7 pack to the classic
/// `88 c6 fa`; dropping the last key leaves 21 bits, so the high 3 bits of the last byte are the unused ones.
#[test]
fn golden_width_3_crosses_bytes() {
    let header = |count: u8| [0, 0, 0, 0, 0, 0, 0, 0, count, 0, 0, 0, 3];
    let mut eight = header(8).to_vec();
    eight.extend([0x88, 0xc6, 0xfa]);
    assert_eq!(encode(&(0..8).collect::<Vec<_>>()), eight);
    let mut seven = header(7).to_vec();
    seven.extend([0x88, 0xc6, 0x1a]);
    assert_eq!(encode(&(0..7).collect::<Vec<_>>()), seven);
    // Any of the 3 unused bits set is corrupt at the last byte.
    for unused in 5..8 {
        let mut bad = seven.clone();
        bad[15] |= 1 << unused;
        assert_eq!(
            Reader::new(&bad).read_for_block().map(|b| b.len()),
            Err(corrupt(15, Corruption::NonZeroReserved { value: bad[15] }))
        );
    }
}

/// Golden: width 64 is the keys minus min as plain little-endian u64s.
#[test]
fn golden_width_64_is_plain_little_endian() {
    let keys = [0_u64, u64::MAX, 0x0102_0304_0506_0708];
    let bytes = encode(&keys);
    let mut expected = Vec::new();
    expected.extend(0_u64.to_le_bytes());
    expected.extend(3_u32.to_le_bytes());
    expected.push(64);
    for k in keys {
        expected.extend(k.to_le_bytes());
    }
    assert_eq!(bytes, expected);
}
