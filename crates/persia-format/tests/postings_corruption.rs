//! Corruption and error-reporting tests for postings blocks (ROADMAP 1.2.3, SPEC §5.2.1 "Postings blocks").
//!
//! Invariants:
//! 1. Readers built from the SPEC alone (byte offsets, the `last_docid >= prev + 128` header rule, bit-level
//!    unpacking, delta formula) agree with `read_postings_header`, `skip_postings_block`,
//!    `read_postings_block` and `read_postings_tail` on arbitrary and mutated input: same value, same error
//!    kind, same absolute error offset (`Reader::with_base_offset`), same bytes consumed.
//! 2. Reads are atomic: on any error the reader has not moved; a tail read never writes past its count.
//! 3. Skipping and decoding agree: same header, same position; a block that skip rejects is never decoded.
//! 4. Every single-bit flip in a valid block is either rejected with `Error::Corrupt` or decodes to a valid
//!    block (strictly increasing, after `prev`, header last docid == decoded last). For full blocks a flip in
//!    `last_docid` or in the packed gaps is always detected (the header pins the sum of all gaps).
//! 5. Encoders append nothing on error, even to a non-empty writer.

// Test-only helpers index, cast and unwrap on purpose (see reader_writer_props.rs).
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::cast_possible_truncation
)]

mod common;

use std::collections::BTreeSet;

use common::{Mutation, apply, corrupt, eof, mutation, placement};

use persia_format::bitpack::{BLOCK_LEN, BitWidth};
use persia_format::postings::{BlockHeader, MAX_TAIL_LEN};
use persia_format::{Corruption, EncodeError, Error, Reader, Writer};
use proptest::prelude::*;
use proptest::sample::Index;

const SENTINEL: u32 = 0xdead_beef;

/// A decoded block: its docids and the bytes consumed.
type Decoded = (Vec<u32>, usize);

/// SPEC §5.2.1 delta rule with wide arithmetic: `v_i = v_(i-1) + g_i + 1`, `v_(-1) = prev` or -1.
fn reference_undelta(gaps: &[u32], prev: Option<u32>, start: u64) -> Result<Vec<u32>, Error> {
    let mut before = prev.map_or(-1_i64, i64::from);
    let mut out = Vec::with_capacity(gaps.len());
    for &gap in gaps {
        before += i64::from(gap) + 1;
        let value = u32::try_from(before)
            .map_err(|_| corrupt(start, Corruption::ValueOverflow { bits: 32 }))?;
        out.push(value);
    }
    Ok(out)
}

/// Bit-level unpack of the SIMD-BP128 vertical layout (SPEC §5.2.1 "Bit-packed block").
fn reference_unpack(bytes: &[u8], w: usize) -> Vec<u32> {
    (0..BLOCK_LEN)
        .map(|i| {
            (0..w).fold(0, |acc, b| {
                let (lane, slot) = (i % 4, i / 4);
                let pos = slot * w + b;
                let (word, bit) = ((pos / 32) * 4 + lane, pos % 32);
                let byte = bytes[word * 4 + bit / 8];
                acc | (u32::from((byte >> (bit % 8)) & 1) << b)
            })
        })
        .collect()
}

/// SPEC §5.2.1: 128 strictly increasing docids after `prev` end at `prev + 128` or later (127 for a list's
/// first block).
fn min_last_docid(prev: Option<u32>) -> u64 {
    prev.map_or(127, |p| u64::from(p) + 128)
}

/// A full block's 5-byte header from the SPEC's byte table. `start` is the absolute offset of `bytes[0]`.
/// The `last_docid` range is checked before the width byte is read.
fn reference_header(bytes: &[u8], start: u64, prev: Option<u32>) -> Result<BlockHeader, Error> {
    let len = bytes.len();
    if len < 4 {
        return Err(eof(start, 4, len));
    }
    let last_docid = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let min = min_last_docid(prev);
    if u64::from(last_docid) < min {
        return Err(corrupt(
            start,
            Corruption::LastDocidOutOfRange { last_docid, min },
        ));
    }
    if len < 5 {
        return Err(eof(start + 4, 1, 0));
    }
    let w = bytes[4];
    if w > 32 {
        return Err(corrupt(
            start + 4,
            Corruption::InvalidBitWidth { width: w, max: 32 },
        ));
    }
    Ok(BlockHeader {
        last_docid,
        width: BitWidth::new(w).unwrap(),
    })
}

/// Skipping a full block: the header, then `16·width` bytes that must be present.
fn reference_skip(
    bytes: &[u8],
    start: u64,
    prev: Option<u32>,
) -> Result<(BlockHeader, usize), Error> {
    let header = reference_header(bytes, start, prev)?;
    let packed_len = header.width.packed_len();
    if bytes.len() - 5 < packed_len {
        return Err(eof(start + 5, packed_len, bytes.len() - 5));
    }
    Ok((header, 5 + packed_len))
}

/// Decoding a full block: as skipping, then the docids must not overflow and must end at `last_docid`.
fn reference_full(
    bytes: &[u8],
    start: u64,
    prev: Option<u32>,
) -> Result<(BlockHeader, Decoded), Error> {
    let (header, used) = reference_skip(bytes, start, prev)?;
    let gaps = reference_unpack(&bytes[5..used], usize::from(header.width.get()));
    let docids = reference_undelta(&gaps, prev, start)?;
    if docids[BLOCK_LEN - 1] != header.last_docid {
        return Err(corrupt(
            start,
            Corruption::LastDocidMismatch {
                header: header.last_docid,
                decoded: docids[BLOCK_LEN - 1],
            },
        ));
    }
    Ok((header, (docids, used)))
}

/// A tail block from the SPEC's byte table: `count` (1..=127), then `count` varint gaps. Varints are parsed by
/// the crate's own (separately tested, see `varint_props.rs`) varint reader, so their errors pass through.
fn reference_tail(bytes: &[u8], start: u64, prev: Option<u32>) -> Result<Decoded, Error> {
    let Some(&raw_count) = bytes.first() else {
        return Err(eof(start, 1, 0));
    };
    let count = usize::from(raw_count);
    if !(1..=MAX_TAIL_LEN).contains(&count) {
        return Err(corrupt(
            start,
            Corruption::InvalidCount {
                count: u32::from(raw_count),
                min: 1,
                max: 127,
            },
        ));
    }
    let mut r = Reader::with_base_offset(&bytes[1..], start + 1);
    let gaps = (0..count)
        .map(|_| r.read_varint_u32())
        .collect::<Result<Vec<_>, _>>()?;
    let docids = reference_undelta(&gaps, prev, start)?;
    Ok((docids, 1 + r.position()))
}

/// Places `block` at absolute offset `base + prefix.len()` behind an already-consumed `prefix`, runs
/// `read_postings_header`, `skip_postings_block` and `read_postings_block` on it, and checks each against
/// its reference and for atomicity. Returns the decode result.
fn check_full(
    block: &[u8],
    base: u64,
    prefix: &[u8],
    prev: Option<u32>,
) -> Result<(BlockHeader, Decoded), Error> {
    let mut bytes = prefix.to_vec();
    bytes.extend(block);
    let skip = prefix.len();
    let start = base + skip as u64;
    let reader = || {
        let mut r = Reader::with_base_offset(&bytes, base);
        r.skip(skip).unwrap();
        r
    };

    let mut h = reader();
    let header = h.read_postings_header(prev);
    assert_eq!(
        header,
        reference_header(block, start, prev),
        "read_postings_header"
    );
    let expected_pos = if header.is_ok() { skip + 5 } else { skip };
    assert_eq!(h.position(), expected_pos, "header read moved wrongly");

    let mut s = reader();
    let skipped = s.skip_postings_block(prev);
    let expected_skip = reference_skip(block, start, prev);
    assert_eq!(
        skipped,
        expected_skip.clone().map(|(h, _)| h),
        "skip_postings_block"
    );
    let expected_pos = expected_skip.as_ref().map_or(skip, |(_, used)| skip + used);
    assert_eq!(s.position(), expected_pos, "skip moved wrongly");

    let mut r = reader();
    let mut out = [SENTINEL; BLOCK_LEN];
    let read = r.read_postings_block(prev, &mut out);
    let actual = match read {
        Ok(header) => {
            // Decoding lands exactly where skipping does.
            assert_eq!(
                r.position(),
                s.position(),
                "skip and read disagree on position"
            );
            Ok((header, (out.to_vec(), r.position() - skip)))
        }
        Err(e) => {
            assert_eq!(r.position(), skip, "failed read moved the reader");
            Err(e)
        }
    };
    assert_eq!(
        actual,
        reference_full(block, start, prev),
        "read_postings_block"
    );
    if let Err(e) = &skipped {
        // Anything skip rejects, decode rejects the same way.
        assert_eq!(actual.as_ref().err(), Some(e));
    }
    actual
}

/// Runs `read_postings_tail` on `block` placed as in [`check_full`] and checks it against its reference.
fn check_tail(block: &[u8], base: u64, prefix: &[u8], prev: Option<u32>) -> Result<Decoded, Error> {
    let mut bytes = prefix.to_vec();
    bytes.extend(block);
    let skip = prefix.len();
    let mut r = Reader::with_base_offset(&bytes, base);
    r.skip(skip).unwrap();
    let mut out = [SENTINEL; BLOCK_LEN];
    let actual = match r.read_postings_tail(prev, &mut out) {
        Ok(count) => {
            assert!(
                out[count..].iter().all(|&x| x == SENTINEL),
                "tail read wrote past its count"
            );
            Ok((out[..count].to_vec(), r.position() - skip))
        }
        Err(e) => {
            assert_eq!(r.position(), skip, "failed read moved the reader");
            Err(e)
        }
    };
    assert_eq!(actual, reference_tail(block, base + skip as u64, prev));
    actual
}

/// What every accepted block must satisfy, whatever its bytes.
fn assert_valid_docids(docids: &[u32], prev: Option<u32>) {
    assert!(
        docids.windows(2).all(|w| w[0] < w[1]),
        "not strictly increasing: {docids:?}"
    );
    assert!(
        prev.is_none_or(|p| docids[0] > p),
        "first docid {} does not follow prev {prev:?}",
        docids[0]
    );
}

fn encode_full(docids: &[u32; BLOCK_LEN], prev: Option<u32>) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_postings_block(docids, prev).unwrap();
    w.into_inner()
}

fn encode_tail(docids: &[u32], prev: Option<u32>) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_postings_tail(docids, prev).unwrap();
    w.into_inner()
}

/// Strictly increasing docids of length `n`, from sparse to dense, including widths 0 and 32.
fn sorted_docids(n: usize) -> impl Strategy<Value = Vec<u32>> {
    prop_oneof![
        proptest::collection::btree_set(any::<u32>(), n)
            .prop_map(|s: BTreeSet<u32>| s.into_iter().collect()),
        // Small gaps from a small start: low widths.
        (0_u32..1000, proptest::collection::vec(0_u32..8, n)).prop_map(|(start, gaps)| {
            gaps.iter()
                .scan(start, |next, &g| {
                    let v = *next + g;
                    *next = v + 1;
                    Some(v)
                })
                .collect()
        }),
        // Dense run: all gaps 0 except possibly the first; `last_docid` is at its minimum after `first - 1`.
        (0..=u32::MAX - n as u32).prop_map(move |s| (s..s + n as u32).collect()),
        // One small docid, then a run ending at u32::MAX: a gap near 2^32 (width 32).
        (0..u32::MAX - n as u32).prop_map(move |x| {
            let run = u64::from(u32::MAX) + 2 - n as u64..=u64::from(u32::MAX);
            std::iter::once(x).chain(run.map(|v| v as u32)).collect()
        }),
    ]
}

/// A valid `prev` for a block starting at `first`.
fn prev_before(first: u32) -> impl Strategy<Value = Option<u32>> {
    prop_oneof![
        Just(None),
        (0..=first).prop_map(move |p| first.checked_sub(1).map(|_| p.min(first - 1))),
        Just(first.checked_sub(1)),
    ]
}

proptest! {
    #[test]
    fn full_block_readers_match_spec_reference(
        (docids, prev) in sorted_docids(BLOCK_LEN)
            .prop_flat_map(|d| { let first = d[0]; (Just(d), prev_before(first)) }),
        m in mutation(),
        (base, prefix) in placement(),
        read_prev in any::<bool>(),
    ) {
        let docids: [u32; BLOCK_LEN] = docids.try_into().unwrap();
        let block = apply(encode_full(&docids, prev), &m);
        // Mostly decode with the right prev; sometimes with a later one, which can trip the header rule.
        let prev = if read_prev { prev } else { prev.map_or(Some(0), |p| p.checked_add(1)) };
        let actual = check_full(&block, base, &prefix, prev);
        if let Ok((header, (decoded, _))) = &actual {
            assert_valid_docids(decoded, prev);
            prop_assert_eq!(header.last_docid, decoded[BLOCK_LEN - 1]);
        }
        if matches!(m, Mutation::Keep) && read_prev {
            let (_, (decoded, used)) = actual.unwrap();
            prop_assert_eq!(decoded.as_slice(), &docids[..]);
            prop_assert_eq!(used, block.len());
        }
    }

    #[test]
    fn tail_block_reader_matches_spec_reference(
        (docids, prev) in (1..=MAX_TAIL_LEN)
            .prop_flat_map(sorted_docids)
            .prop_flat_map(|d| { let first = d[0]; (Just(d), prev_before(first)) }),
        m in mutation(),
        (base, prefix) in placement(),
    ) {
        let block = apply(encode_tail(&docids, prev), &m);
        let actual = check_tail(&block, base, &prefix, prev);
        if let Ok((decoded, _)) = &actual {
            assert_valid_docids(decoded, prev);
        }
        if matches!(m, Mutation::Keep) {
            let (decoded, used) = actual.unwrap();
            prop_assert_eq!(&decoded, &docids);
            prop_assert_eq!(used, block.len());
        }
    }

    /// Invariant 5: a failed encode leaves earlier output untouched.
    #[test]
    fn failed_encodes_append_nothing(
        prefix in proptest::collection::vec(any::<u8>(), 0..16),
        docids in sorted_docids(BLOCK_LEN),
        at in any::<Index>(),
        tail_len in 0_usize..=BLOCK_LEN,
    ) {
        // Break the order at one index: a duplicate, or a value not above prev.
        let i = at.index(BLOCK_LEN);
        let mut bad = docids.clone();
        let prev = if i == 0 { Some(bad[0]) } else { bad[i] = bad[i - 1]; None };
        let mut w = Writer::new();
        w.write_bytes(&prefix);

        let bad_full: [u32; BLOCK_LEN] = bad.clone().try_into().unwrap();
        prop_assert_eq!(
            w.write_postings_block(&bad_full, prev),
            Err(EncodeError::NotStrictlyIncreasing { index: i })
        );
        prop_assert_eq!(w.as_slice(), prefix.as_slice());

        let tail = &bad[..tail_len];
        let result = w.write_postings_tail(tail, prev);
        if !(1..=MAX_TAIL_LEN).contains(&tail_len) {
            prop_assert_eq!(result, Err(EncodeError::InvalidLen { len: tail_len, min: 1, max: 127 }));
        } else if i < tail_len {
            prop_assert_eq!(result, Err(EncodeError::NotStrictlyIncreasing { index: i }));
        } else {
            prop_assert!(result.is_ok());
            return Ok(());
        }
        prop_assert_eq!(w.as_slice(), prefix.as_slice());
    }
}

/// Valid full blocks covering widths 0, small, 25-ish and 32, with and without `prev`, and `last_docid` at
/// exactly its minimum.
fn sample_full_blocks() -> Vec<([u32; BLOCK_LEN], Option<u32>)> {
    vec![
        (core::array::from_fn(|i| i as u32), None), // width 0, last 127 = minimum
        (
            core::array::from_fn(|i| 1000 + 7 * i as u32 + (i as u32 % 3)),
            Some(999),
        ), // width 3
        (core::array::from_fn(|i| (i as u32) * 33_554_431), None), // width 25
        (
            core::array::from_fn(|i| if i == 0 { 5 } else { u32::MAX - 127 + i as u32 }),
            Some(1),
        ), // width 32: a gap near 2^32
        (
            core::array::from_fn(|i| u32::MAX - 127 + i as u32),
            Some(u32::MAX - 200),
        ), // ends at u32::MAX, width 7
        (
            core::array::from_fn(|i| u32::MAX - 127 + i as u32),
            Some(u32::MAX - 128),
        ), // width 0, last u32::MAX = minimum
    ]
}

/// Invariant 4, exhaustive over every bit of every sample full block.
#[test]
fn every_bit_flip_in_a_full_block_is_detected_or_harmless() {
    let base = 4096;
    for (docids, prev) in sample_full_blocks() {
        let bytes = encode_full(&docids, prev);
        let width = usize::from(bytes[4]);
        assert_eq!(bytes.len(), 5 + 16 * width);
        let last = docids[BLOCK_LEN - 1];
        for bit in 0..bytes.len() * 8 {
            let mut flipped = bytes.clone();
            flipped[bit / 8] ^= 1 << (bit % 8);
            let result = check_full(&flipped, base, &[], prev);
            match bit / 8 {
                // last_docid: below the header minimum, or no longer what the content decodes to.
                0..4 => {
                    let flipped_last = last ^ (1 << bit);
                    let min = min_last_docid(prev);
                    let reason = if u64::from(flipped_last) < min {
                        Corruption::LastDocidOutOfRange {
                            last_docid: flipped_last,
                            min,
                        }
                    } else {
                        Corruption::LastDocidMismatch {
                            header: flipped_last,
                            decoded: last,
                        }
                    };
                    assert_eq!(
                        result,
                        Err(corrupt(base, reason)),
                        "width {width} bit {bit}"
                    );
                }
                // Width: either rejected or a structurally valid block.
                4 => {
                    if let Ok((header, (decoded, _))) = result {
                        assert_valid_docids(&decoded, prev);
                        assert_eq!(header.last_docid, decoded[BLOCK_LEN - 1]);
                    }
                }
                // A packed gap changed by ±2^k shifts every later docid, the last one included.
                _ => assert!(
                    matches!(
                        result,
                        Err(Error::Corrupt {
                            offset: 4096,
                            reason: Corruption::LastDocidMismatch { .. }
                                | Corruption::ValueOverflow { bits: 32 }
                        })
                    ),
                    "width {width} bit {bit}: {result:?}"
                ),
            }
        }
    }
}

/// Invariant 4 for tails, exhaustive over every bit: always the reference's answer, never a panic, and
/// every accepted block is valid.
#[test]
fn every_bit_flip_in_a_tail_block_is_detected_or_harmless() {
    let samples: Vec<(Vec<u32>, Option<u32>)> = vec![
        (vec![0], None),
        (vec![u32::MAX], None),
        (vec![u32::MAX], Some(u32::MAX - 1)),
        (vec![5, 6, 10, 300, 70_000, 1 << 28], Some(3)),
        ((0..127).map(|i| i * 1000).collect(), None),
        ((0..127).map(|i| u32::MAX - 126 + i).collect(), Some(0)),
    ];
    for (docids, prev) in samples {
        let bytes = encode_tail(&docids, prev);
        for bit in 0..bytes.len() * 8 {
            let mut flipped = bytes.clone();
            flipped[bit / 8] ^= 1 << (bit % 8);
            if let Ok((decoded, used)) = check_tail(&flipped, 77, &[], prev) {
                assert_valid_docids(&decoded, prev);
                assert!(used <= flipped.len());
            }
        }
    }
}

/// The header rule is checked first (SPEC §5.2.1): a `last_docid` below `prev + 128` (127 for a list's
/// first block) is `LastDocidOutOfRange` at the block start even when the width byte is invalid or missing;
/// at exactly the minimum, the width byte is what fails.
#[test]
fn last_docid_range_is_checked_before_the_width() {
    let prevs = [
        None,
        Some(0),
        Some(1000),
        Some(u32::MAX - 128),
        Some(u32::MAX - 127),
        Some(u32::MAX),
    ];
    for prev in prevs {
        let min = min_last_docid(prev);
        let below = u32::try_from(min - 1).unwrap_or(u32::MAX);
        let at_min = u32::try_from(min).ok();
        for tail in [&[][..], &[33][..], &[0][..]] {
            let mut block = below.to_le_bytes().to_vec();
            block.extend(tail);
            assert_eq!(
                check_full(&block, 512, &[0xee], prev).map(|(h, _)| h),
                Err(corrupt(
                    513,
                    Corruption::LastDocidOutOfRange {
                        last_docid: below,
                        min
                    }
                )),
                "{prev:?} {tail:?}"
            );
            if let Some(last) = at_min {
                let mut block = last.to_le_bytes().to_vec();
                block.extend(tail);
                let expected = match tail {
                    [] => eof(517, 1, 0),
                    [33] => corrupt(517, Corruption::InvalidBitWidth { width: 33, max: 32 }),
                    // Width 0: the header and the skip succeed; decoding checks the content.
                    _ => {
                        let mut r = Reader::new(&block);
                        assert_eq!(
                            r.skip_postings_block(prev),
                            Ok(BlockHeader {
                                last_docid: last,
                                width: BitWidth::new(0).unwrap()
                            })
                        );
                        assert_eq!(r.position(), 5);
                        continue;
                    }
                };
                assert_eq!(
                    check_full(&block, 512, &[0xee], prev).map(|(h, _)| h),
                    Err(expected)
                );
            }
        }
    }
}

/// Invariant 1 at the edges: full blocks of widths 0 to 32 at the top of the docid space round-trip.
#[test]
fn full_blocks_round_trip_at_the_u32_edges() {
    for (docids, prev) in sample_full_blocks() {
        let bytes = encode_full(&docids, prev);
        let (header, (decoded, used)) = check_full(&bytes, 0, &[], prev).unwrap();
        assert_eq!(decoded, docids);
        assert_eq!(header.last_docid, docids[BLOCK_LEN - 1]);
        assert_eq!(used, bytes.len());
    }
    let widths: Vec<u8> = sample_full_blocks()
        .iter()
        .map(|(d, p)| encode_full(d, *p)[4])
        .collect();
    assert_eq!(widths, [0, 3, 25, 32, 7, 0]);
}

/// Golden blocks at the u32 edges (SPEC §5.2.1, §4.1 varints).
#[test]
fn golden_blocks_at_u32_max() {
    // A single docid u32::MAX from the start of a list is gap u32::MAX: a 5-byte varint.
    assert_eq!(
        encode_tail(&[u32::MAX], None),
        [1, 0xff, 0xff, 0xff, 0xff, 0x0f]
    );
    // After prev u32::MAX - 1 the gap is 0.
    assert_eq!(encode_tail(&[u32::MAX], Some(u32::MAX - 1)), [1, 0]);
    // The last 128 docids after prev u32::MAX - 128: a 5-byte header, width 0, nothing packed.
    let top: [u32; BLOCK_LEN] = core::array::from_fn(|i| u32::MAX - 127 + i as u32);
    assert_eq!(
        encode_full(&top, Some(u32::MAX - 128)),
        [0xff, 0xff, 0xff, 0xff, 0]
    );
    // Nothing can follow u32::MAX.
    let mut w = Writer::new();
    assert_eq!(
        w.write_postings_tail(&[u32::MAX], Some(u32::MAX)),
        Err(EncodeError::NotStrictlyIncreasing { index: 0 })
    );
    assert!(w.is_empty());
}
