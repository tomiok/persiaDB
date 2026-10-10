//! Corruption and error-reporting tests for postings blocks (ROADMAP 1.2.3, SPEC §5.2.1 "Postings blocks").
//!
//! Invariants:
//! 1. A reader built from the SPEC alone (byte offsets, bit-level unpacking, delta formula) agrees with
//!    `read_postings_block` / `read_postings_tail` on arbitrary and mutated input: same value, same error kind,
//!    same absolute error offset (`Reader::with_base_offset`), same bytes consumed.
//! 2. Reads are atomic: on any error the reader has not moved; a tail read never writes past `count`.
//! 3. `skip_postings_block` and `read_postings_block` agree: same header, same position, same header errors.
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
use persia_format::postings::{BlockHeader, MAX_TAIL_LEN, TailHeader};
use persia_format::{Corruption, EncodeError, Error, Reader, Writer};
use proptest::prelude::*;
use proptest::sample::Index;

const SENTINEL: u32 = 0xdead_beef;

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

/// A full block read from the SPEC's byte table; returns the header, the docids and the bytes consumed.
/// `start` is the absolute offset of `bytes[0]`.
fn reference_full(
    bytes: &[u8],
    start: u64,
    prev: Option<u32>,
) -> Result<(BlockHeader, Vec<u32>, usize), Error> {
    let len = bytes.len();
    if len < 4 {
        return Err(eof(start, 4, len));
    }
    let last_docid = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if len < 8 {
        return Err(eof(start + 4, 4, len - 4));
    }
    let max_tf = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if len < 9 {
        return Err(eof(start + 8, 1, 0));
    }
    let w = bytes[8];
    if w > 32 {
        return Err(corrupt(
            start + 8,
            Corruption::InvalidBitWidth { width: w, max: 32 },
        ));
    }
    let packed_len = 16 * usize::from(w);
    if len - 9 < packed_len {
        return Err(eof(start + 9, packed_len, len - 9));
    }
    let gaps = reference_unpack(&bytes[9..9 + packed_len], usize::from(w));
    let docids = reference_undelta(&gaps, prev, start)?;
    if docids[BLOCK_LEN - 1] != last_docid {
        return Err(corrupt(
            start,
            Corruption::LastDocidMismatch {
                header: last_docid,
                decoded: docids[BLOCK_LEN - 1],
            },
        ));
    }
    let header = BlockHeader {
        last_docid,
        max_tf,
        width: BitWidth::new(w).unwrap(),
    };
    Ok((header, docids, 9 + packed_len))
}

/// A tail block read from the SPEC's byte table. Varints are parsed by the crate's own (separately
/// tested, see `varint_props.rs`) varint reader, so their errors are passed through as-is.
fn reference_tail(
    bytes: &[u8],
    start: u64,
    prev: Option<u32>,
) -> Result<(TailHeader, Vec<u32>, usize), Error> {
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
    let max_tf = r.read_varint_u32()?;
    let gaps = (0..count)
        .map(|_| r.read_varint_u32())
        .collect::<Result<Vec<_>, _>>()?;
    let docids = reference_undelta(&gaps, prev, start)?;
    Ok((TailHeader { count, max_tf }, docids, 1 + r.position()))
}

/// Reads a full block with the crate at absolute offset `base + skip`, checking atomicity and that
/// `skip_postings_block` agrees; returns what `reference_full` returns.
fn library_full(
    bytes: &[u8],
    base: u64,
    skip: usize,
    prev: Option<u32>,
) -> Result<(BlockHeader, Vec<u32>, usize), Error> {
    let mut r = Reader::with_base_offset(bytes, base);
    r.skip(skip).unwrap();
    let mut out = [SENTINEL; BLOCK_LEN];
    let read = r.read_postings_block(prev, &mut out);

    let mut s = Reader::with_base_offset(bytes, base);
    s.skip(skip).unwrap();
    let skipped = s.skip_postings_block();
    match (&read, &skipped) {
        (Ok(h), Ok(sh)) => {
            assert_eq!(h, sh, "skip and read disagree on the header");
            assert_eq!(
                r.position(),
                s.position(),
                "skip and read disagree on position"
            );
        }
        (Err(e), Err(se)) => {
            assert_eq!(e, se, "a header error must be the same for skip and read");
        }
        (Err(_), Ok(_)) => {} // content errors (overflow, last-docid mismatch) are invisible to skip
        (Ok(_), Err(se)) => assert!(
            read.is_err(),
            "read accepted a block that skip rejects: {se}"
        ),
    }
    if skipped.is_err() {
        assert_eq!(s.position(), skip, "failed skip moved the reader");
    }

    match read {
        Ok(header) => Ok((header, out.to_vec(), r.position() - skip)),
        Err(e) => {
            assert_eq!(r.position(), skip, "failed read moved the reader");
            Err(e)
        }
    }
}

fn library_tail(
    bytes: &[u8],
    base: u64,
    skip: usize,
    prev: Option<u32>,
) -> Result<(TailHeader, Vec<u32>, usize), Error> {
    let mut r = Reader::with_base_offset(bytes, base);
    r.skip(skip).unwrap();
    let mut out = [SENTINEL; BLOCK_LEN];
    match r.read_postings_tail(prev, &mut out) {
        Ok(tail) => {
            assert!(
                out[tail.count..].iter().all(|&x| x == SENTINEL),
                "tail read wrote past count"
            );
            Ok((tail, out[..tail.count].to_vec(), r.position() - skip))
        }
        Err(e) => {
            assert_eq!(r.position(), skip, "failed read moved the reader");
            Err(e)
        }
    }
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

fn encode_full(docids: &[u32; BLOCK_LEN], prev: Option<u32>, max_tf: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_postings_block(docids, prev, max_tf).unwrap();
    w.into_inner()
}

fn encode_tail(docids: &[u32], prev: Option<u32>, max_tf: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_postings_tail(docids, prev, max_tf).unwrap();
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
        // Dense run: all gaps 0 except possibly the first.
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
    fn full_block_reader_matches_spec_reference(
        (docids, prev) in sorted_docids(BLOCK_LEN)
            .prop_flat_map(|d| { let first = d[0]; (Just(d), prev_before(first)) }),
        max_tf in any::<u32>(),
        m in mutation(),
        (base, prefix) in placement(),
        read_prev in any::<bool>(),
    ) {
        let docids: [u32; BLOCK_LEN] = docids.try_into().unwrap();
        let block = apply(encode_full(&docids, prev, max_tf), &m);
        // Mostly decode with the right prev; sometimes with a wrong one, which must also be handled.
        let prev = if read_prev { prev } else { prev.map_or(Some(0), |p| p.checked_add(1)) };
        let mut bytes = prefix.clone();
        bytes.extend(&block);
        let start = base + prefix.len() as u64;

        let expected = reference_full(&block, start, prev);
        let actual = library_full(&bytes, base, prefix.len(), prev);
        prop_assert_eq!(&actual, &expected);
        if let Ok((header, decoded, _)) = &actual {
            assert_valid_docids(decoded, prev);
            prop_assert_eq!(header.last_docid, decoded[BLOCK_LEN - 1]);
        }
        if matches!(m, Mutation::Keep) && read_prev {
            let (header, decoded, used) = actual.unwrap();
            prop_assert_eq!(decoded.as_slice(), &docids[..]);
            prop_assert_eq!(header.max_tf, max_tf);
            prop_assert_eq!(used, block.len());
        }
    }

    #[test]
    fn tail_block_reader_matches_spec_reference(
        (docids, prev) in (1..=MAX_TAIL_LEN)
            .prop_flat_map(sorted_docids)
            .prop_flat_map(|d| { let first = d[0]; (Just(d), prev_before(first)) }),
        max_tf in any::<u32>(),
        m in mutation(),
        (base, prefix) in placement(),
    ) {
        let block = apply(encode_tail(&docids, prev, max_tf), &m);
        let mut bytes = prefix.clone();
        bytes.extend(&block);
        let start = base + prefix.len() as u64;

        let expected = reference_tail(&block, start, prev);
        let actual = library_tail(&bytes, base, prefix.len(), prev);
        prop_assert_eq!(&actual, &expected);
        if let Ok((tail, decoded, _)) = &actual {
            assert_valid_docids(decoded, prev);
            prop_assert_eq!(tail.count, decoded.len());
        }
        if matches!(m, Mutation::Keep) {
            let (tail, decoded, used) = actual.unwrap();
            prop_assert_eq!(&decoded, &docids);
            prop_assert_eq!(tail.max_tf, max_tf);
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
            w.write_postings_block(&bad_full, prev, 1),
            Err(EncodeError::NotStrictlyIncreasing { index: i })
        );
        prop_assert_eq!(w.as_slice(), prefix.as_slice());

        let tail = &bad[..tail_len];
        let result = w.write_postings_tail(tail, prev, 1);
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

/// Valid full blocks covering widths 0, small, 25-ish and 32, with and without `prev`.
fn sample_full_blocks() -> Vec<([u32; BLOCK_LEN], Option<u32>)> {
    vec![
        (core::array::from_fn(|i| i as u32), None), // width 0
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
    ]
}

/// Invariant 4, exhaustive over every bit of every sample full block.
#[test]
fn every_bit_flip_in_a_full_block_is_detected_or_harmless() {
    for (docids, prev) in sample_full_blocks() {
        let bytes = encode_full(&docids, prev, 0x0102_0304);
        let width = usize::from(bytes[8]);
        assert_eq!(bytes.len(), 9 + 16 * width);
        for bit in 0..bytes.len() * 8 {
            let mut flipped = bytes.clone();
            flipped[bit / 8] ^= 1 << (bit % 8);
            let base = 4096;
            let result = library_full(&flipped, base, 0, prev);
            assert_eq!(
                result,
                reference_full(&flipped, base, prev),
                "width {width} bit {bit}"
            );
            match bit / 8 {
                // last_docid: the decoded content no longer matches the header.
                0..4 => assert_eq!(
                    result,
                    Err(corrupt(
                        base,
                        Corruption::LastDocidMismatch {
                            header: docids[BLOCK_LEN - 1] ^ (1 << bit),
                            decoded: docids[BLOCK_LEN - 1],
                        }
                    )),
                    "width {width} bit {bit}"
                ),
                // max_tf is not self-checking (the Frame checksum covers it, SPEC §4.4): decodes, docids intact.
                4..8 => {
                    let (header, decoded, _) = result.unwrap();
                    assert_eq!(header.max_tf, 0x0102_0304 ^ (1 << (bit - 32)));
                    assert_eq!(decoded, docids);
                }
                // Width: either rejected or a structurally valid block.
                8 => {
                    if let Ok((header, decoded, _)) = result {
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
        let bytes = encode_tail(&docids, prev, u32::MAX);
        for bit in 0..bytes.len() * 8 {
            let mut flipped = bytes.clone();
            flipped[bit / 8] ^= 1 << (bit % 8);
            let result = library_tail(&flipped, 77, 0, prev);
            assert_eq!(
                result,
                reference_tail(&flipped, 77, prev),
                "{docids:?} bit {bit}"
            );
            if let Ok((_, decoded, used)) = result {
                assert_valid_docids(&decoded, prev);
                assert!(used <= flipped.len());
            }
        }
    }
}

/// Invariant 1 at the edges: a full block of widths 32 and 0 at the top of the docid space round-trips.
#[test]
fn full_blocks_round_trip_at_the_u32_edges() {
    for (docids, prev) in sample_full_blocks() {
        let bytes = encode_full(&docids, prev, u32::MAX);
        let (header, decoded, used) = library_full(&bytes, 0, 0, prev).unwrap();
        assert_eq!(decoded, docids);
        assert_eq!(header.max_tf, u32::MAX);
        assert_eq!(header.last_docid, docids[BLOCK_LEN - 1]);
        assert_eq!(used, bytes.len());
    }
    let widths: Vec<u8> = sample_full_blocks()
        .iter()
        .map(|(d, p)| encode_full(d, *p, 0)[8])
        .collect();
    assert_eq!(widths, [0, 3, 25, 32, 7]);
}

/// Golden tail at the u32 edges (SPEC §5.2.1, §4.1 varints): a single docid `u32::MAX` from the start of a
/// list is gap `u32::MAX`, a 5-byte varint, as is `max_tf = u32::MAX`.
#[test]
fn golden_tail_at_u32_max() {
    assert_eq!(
        encode_tail(&[u32::MAX], None, u32::MAX),
        [
            1, 0xff, 0xff, 0xff, 0xff, 0x0f, 0xff, 0xff, 0xff, 0xff, 0x0f
        ]
    );
    // After prev u32::MAX - 1 the gap is 0.
    assert_eq!(encode_tail(&[u32::MAX], Some(u32::MAX - 1), 0), [1, 0, 0]);
    // Nothing can follow u32::MAX.
    let mut w = Writer::new();
    assert_eq!(
        w.write_postings_tail(&[u32::MAX], Some(u32::MAX), 0),
        Err(EncodeError::NotStrictlyIncreasing { index: 0 })
    );
    assert!(w.is_empty());
}
