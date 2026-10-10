//! Postings blocks: delta-coded docids, 128 per bit-packed full block, then a varint tail block
//! (SPEC §5.2.1 "Postings blocks", ADR-0004).
//!
//! ```text
//! full block                                  tail block
//! 0  4  last_docid u32 (absolute)             0  1  count  u8 (1..=127)
//! 4  1  width      u8  (0..=32)               1  .. count gaps, varint u32 each
//! 5  16·width  bit-packed gaps
//! ```
//!
//! Docids are delta coded across the whole list: each block is decoded relative to the previous block's last
//! docid (`prev`, `None` for the first block). Block-max data (`max_tf`, `min_norm`) lives in the Skip
//! section, not here (SPEC §5.3). Decoding writes into a caller-provided buffer and never allocates. Reads
//! are atomic: on error the reader has not moved.

use crate::bitpack::{BLOCK_LEN, BitWidth};
use crate::delta;
use crate::error::{Corruption, EncodeError, Error, Result, to_u64};
use crate::reader::Reader;
use crate::writer::Writer;

/// Most docids a tail block can hold; a list of `n` docids has a tail of `n % 128`.
pub const MAX_TAIL_LEN: usize = BLOCK_LEN - 1;

/// The fixed-size header of a full block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    /// The block's last (largest) docid: blocks ending before a target can be skipped unread.
    pub last_docid: u32,
    /// Bit width of the packed gaps.
    pub width: BitWidth,
}

impl BlockHeader {
    /// Bytes of a header.
    pub const LEN: usize = 5;
}

impl Writer {
    /// Appends a full block of 128 strictly increasing docids that follow `prev`.
    ///
    /// # Errors
    /// [`EncodeError::NotStrictlyIncreasing`]; nothing is written then.
    pub fn write_postings_block(
        &mut self,
        docids: &[u32; BLOCK_LEN],
        prev: Option<u32>,
    ) -> Result<(), EncodeError> {
        let mut gaps = *docids;
        delta::encode_in_place(&mut gaps, prev)?;
        let width = BitWidth::of(&gaps);
        let [.., last_docid] = *docids;
        self.write_u32(last_docid);
        self.write_bit_width(width);
        // The width is minimal for these gaps, so packing cannot fail.
        self.write_packed_block(&gaps, width)
    }

    /// Appends a tail block of 1..=127 strictly increasing docids that follow `prev`.
    ///
    /// # Errors
    /// [`EncodeError::InvalidLen`] for an empty or full-size tail, or
    /// [`EncodeError::NotStrictlyIncreasing`]; nothing is written then.
    pub fn write_postings_tail(
        &mut self,
        docids: &[u32],
        prev: Option<u32>,
    ) -> Result<(), EncodeError> {
        let count = u8::try_from(docids.len())
            .ok()
            .filter(|&n| (1..=MAX_TAIL_LEN).contains(&usize::from(n)))
            .ok_or(EncodeError::InvalidLen {
                len: docids.len(),
                min: 1,
                max: MAX_TAIL_LEN,
            })?;
        let mut gaps = [0_u32; MAX_TAIL_LEN];
        let gaps = gaps.get_mut(..docids.len()).unwrap_or_default();
        gaps.copy_from_slice(docids);
        delta::encode_in_place(gaps, prev)?;
        self.write_u8(count);
        for &gap in &*gaps {
            self.write_varint_u32(gap);
        }
        Ok(())
    }
}

impl Reader<'_> {
    /// Reads the header of a full block following `prev`, leaving the reader at its packed gaps.
    ///
    /// `last_docid` is checked against `prev`: 128 strictly increasing docids after `prev` end at
    /// `prev + 128` or later (at 127 or later for the first block). This keeps a list increasing however
    /// blocks are mixed between skipping and decoding.
    ///
    /// # Errors
    /// [`Error::Corrupt`] on short input, an invalid width, or [`Corruption::LastDocidOutOfRange`] (at the
    /// block start); the position is then unchanged.
    pub fn read_postings_header(&mut self, prev: Option<u32>) -> Result<BlockHeader> {
        let start = self.offset();
        let mut r = self.clone();
        let last_docid = r.read_u32()?;
        // first_after(prev) <= 2^32, plus 127: no overflow.
        let min = delta::first_after(prev).wrapping_add(to_u64(MAX_TAIL_LEN));
        if u64::from(last_docid) < min {
            return Err(Error::Corrupt {
                offset: start,
                reason: Corruption::LastDocidOutOfRange { last_docid, min },
            });
        }
        let width = r.read_bit_width()?;
        *self = r;
        Ok(BlockHeader { last_docid, width })
    }

    /// Skips a full block following `prev` without unpacking it, returning its header: the next block's
    /// `prev` is `header.last_docid`.
    ///
    /// # Errors
    /// As [`Reader::read_postings_header`], or short input for the packed gaps; the position is then unchanged.
    pub fn skip_postings_block(&mut self, prev: Option<u32>) -> Result<BlockHeader> {
        let mut r = self.clone();
        let header = r.read_postings_header(prev)?;
        r.skip(header.width.packed_len())?;
        *self = r;
        Ok(header)
    }

    /// Decodes a full block of docids following `prev` into `out`.
    ///
    /// # Errors
    /// As [`Reader::skip_postings_block`], or a docid past `u32::MAX` ([`Corruption::ValueOverflow`]) or a
    /// header that disagrees with the content ([`Corruption::LastDocidMismatch`]); those two point at the
    /// block start. The position is then unchanged; `out` may hold partial data.
    pub fn read_postings_block(
        &mut self,
        prev: Option<u32>,
        out: &mut [u32; BLOCK_LEN],
    ) -> Result<BlockHeader> {
        let start = self.offset();
        let corrupt = |reason| Error::Corrupt {
            offset: start,
            reason,
        };
        let mut r = self.clone();
        let header = r.read_postings_header(prev)?;
        r.read_packed_block(header.width, out)?;
        delta::decode_in_place(out, prev).map_err(corrupt)?;
        let [.., decoded] = *out;
        if decoded != header.last_docid {
            return Err(corrupt(Corruption::LastDocidMismatch {
                header: header.last_docid,
                decoded,
            }));
        }
        *self = r;
        Ok(header)
    }

    /// Decodes a tail block of docids following `prev` into the first slots of `out`, returning how many.
    ///
    /// The count is 1..=127; the caller checks it equals the term's `df % 128` (SPEC §5.2.1).
    ///
    /// # Errors
    /// [`Error::Corrupt`] on short input, a malformed varint, a count outside 1..=127
    /// ([`Corruption::InvalidCount`]) or a docid past `u32::MAX` ([`Corruption::ValueOverflow`]); those two
    /// point at the block start. The position is then unchanged; `out` may hold partial data.
    pub fn read_postings_tail(
        &mut self,
        prev: Option<u32>,
        out: &mut [u32; BLOCK_LEN],
    ) -> Result<usize> {
        let start = self.offset();
        let mut r = self.clone();
        let raw_count = r.read_u8()?;
        let count = usize::from(raw_count);
        if !(1..=MAX_TAIL_LEN).contains(&count) {
            return Err(Error::Corrupt {
                offset: start,
                reason: Corruption::InvalidCount {
                    count: u32::from(raw_count),
                    min: 1,
                    max: u32::try_from(MAX_TAIL_LEN).unwrap_or(u32::MAX), // 127
                },
            });
        }
        let docids = out.get_mut(..count).unwrap_or_default();
        for slot in &mut *docids {
            *slot = r.read_varint_u32()?;
        }
        delta::decode_in_place(docids, prev).map_err(|reason| Error::Corrupt {
            offset: start,
            reason,
        })?;
        *self = r;
        Ok(count)
    }
}

/// The index of the first docid at or after `from` that is `>= target`, or `docids.len()` if there is none.
///
/// `docids` must be sorted (decoded blocks are). Gallops from `from` and then binary-searches, so short
/// forward skips, the common case in conjunctions, cost O(log distance).
pub fn advance_to(docids: &[u32], from: usize, target: u32) -> usize {
    let Some(rest) = docids.get(from..) else {
        return docids.len();
    };
    // Gallop: probe 1, 2, 4, ... until a docid >= target or the end. Every probe before the last was < target.
    let mut bound = 1_usize;
    while rest.get(bound).is_some_and(|&d| d < target) {
        bound = bound.saturating_mul(2);
    }
    // The answer is in [bound / 2, bound]: everything before bound / 2 was probed or passed as < target, and
    // rest[bound] (if it exists) is >= target, so searching [bound / 2, bound) and landing on bound is enough.
    let lo = bound >> 1;
    let hi = bound.min(rest.len());
    let window = rest.get(lo..hi).unwrap_or_default();
    // lo + partition point <= rest.len(), and from + rest.len() == docids.len(): neither sum saturates.
    from.saturating_add(lo)
        .saturating_add(window.partition_point(|&d| d < target))
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test docids are built from small indices
mod tests {
    use super::*;

    fn full(docids: &[u32; BLOCK_LEN], prev: Option<u32>) -> Vec<u8> {
        let mut w = Writer::new();
        w.write_postings_block(docids, prev).unwrap();
        w.into_inner()
    }

    fn tail(docids: &[u32], prev: Option<u32>) -> Vec<u8> {
        let mut w = Writer::new();
        w.write_postings_tail(docids, prev).unwrap();
        w.into_inner()
    }

    fn corrupt(offset: u64, reason: Corruption) -> Error {
        Error::Corrupt { offset, reason }
    }

    /// Golden full block (SPEC §5.2.1): docids 0..128 from the start of a list are 128 zero gaps, so the
    /// block is just its 5-byte header with width 0.
    #[test]
    fn golden_dense_full_block() {
        let docids: [u32; BLOCK_LEN] = core::array::from_fn(|i| i as u32);
        assert_eq!(full(&docids, None), [0x7f, 0, 0, 0, 0]);
    }

    /// Golden full block with packed gaps: even docids 2, 4, ..., 256 after prev 1 have gaps
    /// [0, 1, 1, ...] (the first is 2 - 1 - 1), width 1. Lane 0 holds gaps 0, 4, 8, ...: a 0 then ones.
    #[test]
    fn golden_full_block_with_gaps() {
        let docids: [u32; BLOCK_LEN] = core::array::from_fn(|i| 2 * i as u32 + 2);
        let mut expected = vec![0x00, 0x01, 0, 0, 1]; // last 256, width 1
        expected.extend([0xfe, 0xff, 0xff, 0xff]); // lane 0: first gap 0
        expected.extend([0xff; 12]); // lanes 1..3: all ones
        assert_eq!(full(&docids, Some(1)), expected);
    }

    /// Golden tail block: docids [5, 6, 10] after prev 3 are gaps [1, 0, 3].
    #[test]
    fn golden_tail_block() {
        assert_eq!(tail(&[5, 6, 10], Some(3)), [3, 1, 0, 3]);
        assert_eq!(tail(&[300], None), [1, 0xac, 0x02]);
    }

    /// Golden multi-block list: two dense full blocks and a tail, chained through `prev`. Docids 0..256 and
    /// then 300: the second block's header says 255, and the tail's only gap is 300 - 255 - 1 = 44.
    #[test]
    fn golden_list_of_two_blocks_and_a_tail() {
        let first: [u32; BLOCK_LEN] = core::array::from_fn(|i| i as u32);
        let second: [u32; BLOCK_LEN] = core::array::from_fn(|i| 128 + i as u32);
        let mut w = Writer::new();
        w.write_postings_block(&first, None).unwrap();
        w.write_postings_block(&second, Some(127)).unwrap();
        w.write_postings_tail(&[300], Some(255)).unwrap();
        let bytes = w.into_inner();
        assert_eq!(bytes, [0x7f, 0, 0, 0, 0, 0xff, 0, 0, 0, 0, 1, 44]);

        let mut r = Reader::new(&bytes);
        let mut out = [0; BLOCK_LEN];
        assert_eq!(r.skip_postings_block(None).map(|h| h.last_docid), Ok(127));
        assert_eq!(
            r.read_postings_block(Some(127), &mut out)
                .map(|h| h.last_docid),
            Ok(255)
        );
        assert_eq!(out, second);
        assert_eq!(r.read_postings_tail(Some(255), &mut out), Ok(1));
        assert_eq!(out[0], 300);
        assert!(r.is_empty());
    }

    #[test]
    fn full_block_round_trip_and_skip() {
        let docids: [u32; BLOCK_LEN] =
            core::array::from_fn(|i| 1000 + 7 * i as u32 + (i as u32 % 3));
        let mut bytes = full(&docids, Some(999));
        bytes.extend(tail(&[docids[127] + 1], Some(docids[127])));

        let mut out = [0; BLOCK_LEN];
        let mut r = Reader::new(&bytes);
        let header = r.read_postings_block(Some(999), &mut out).unwrap();
        assert_eq!(out, docids);
        assert_eq!(header.last_docid, docids[127]);
        assert_eq!(header.width.get(), 3); // docid steps of 5 or 8 are gaps of 4 or 7
        assert_eq!(
            r.read_postings_tail(Some(header.last_docid), &mut out),
            Ok(1)
        );
        assert_eq!(out[0], docids[127] + 1);
        assert!(r.is_empty());

        let mut r = Reader::new(&bytes);
        assert_eq!(r.skip_postings_block(Some(999)), Ok(header));
        assert_eq!(r.position(), bytes.len() - 2);
    }

    #[test]
    fn encoder_rejects_bad_input_without_writing() {
        let mut w = Writer::new();
        let mut docids: [u32; BLOCK_LEN] = core::array::from_fn(|i| i as u32);
        docids[50] = 49;
        assert_eq!(
            w.write_postings_block(&docids, None),
            Err(EncodeError::NotStrictlyIncreasing { index: 50 })
        );
        assert_eq!(
            w.write_postings_block(&core::array::from_fn(|i| i as u32), Some(0)),
            Err(EncodeError::NotStrictlyIncreasing { index: 0 })
        );
        let too_long = [0; BLOCK_LEN];
        for bad in [&[][..], &too_long[..]] {
            assert_eq!(
                w.write_postings_tail(bad, None),
                Err(EncodeError::InvalidLen {
                    len: bad.len(),
                    min: 1,
                    max: 127
                })
            );
        }
        assert_eq!(
            w.write_postings_tail(&[4, 4], None),
            Err(EncodeError::NotStrictlyIncreasing { index: 1 })
        );
        assert!(w.is_empty());
    }

    #[test]
    fn last_docid_mismatch_is_corrupt_at_block_start() {
        // Gaps of 1 from the start: docids 0, 2, ..., 254. A header claiming 253 is in range but wrong.
        let docids: [u32; BLOCK_LEN] = core::array::from_fn(|i| 2 * i as u32);
        let mut bytes = vec![0xee];
        bytes.extend(full(&docids, None));
        bytes[1] = 253;
        let mut r = Reader::with_base_offset(&bytes, 50);
        r.skip(1).unwrap();
        let mut out = [0; BLOCK_LEN];
        assert_eq!(
            r.read_postings_block(None, &mut out),
            Err(corrupt(
                51,
                Corruption::LastDocidMismatch {
                    header: 253,
                    decoded: 254
                }
            ))
        );
        assert_eq!(r.position(), 1);
    }

    #[test]
    fn last_docid_too_small_for_prev_is_rejected_by_skip_and_read() {
        let mut out = [0; BLOCK_LEN];
        let cases = [
            (None, 126_u32, 127_u64),
            (Some(0), 127, 128),
            (Some(1000), 1000, 1128),
            (Some(1000), 1127, 1128),
            (Some(u32::MAX - 127), u32::MAX, u64::from(u32::MAX) + 1),
            (Some(u32::MAX), u32::MAX, u64::from(u32::MAX) + 128),
        ];
        for (prev, last_docid, min) in cases {
            let mut bytes = last_docid.to_le_bytes().to_vec();
            bytes.push(0); // width 0
            let err = Err(corrupt(
                9,
                Corruption::LastDocidOutOfRange { last_docid, min },
            ));
            let mut r = Reader::with_base_offset(&bytes, 9);
            assert_eq!(r.skip_postings_block(prev), err, "{prev:?} {last_docid}");
            assert_eq!(
                r.read_postings_block(prev, &mut out).map(drop),
                err.map(drop)
            );
            assert_eq!(r.position(), 0);
        }
        // The smallest valid value is accepted.
        let mut r = Reader::new(&[0x7f, 0, 0, 0, 0]);
        assert_eq!(r.skip_postings_block(None).map(|h| h.last_docid), Ok(127));
    }

    #[test]
    fn overflow_is_corrupt_at_block_start() {
        // Width-1 block of all-ones gaps after prev u32::MAX - 200: in-range header, but docids step by 2
        // and run past u32::MAX.
        let mut bytes = vec![0xff, 0xff, 0xff, 0xff, 1];
        bytes.extend([0xff; 16]);
        let mut out = [0; BLOCK_LEN];
        let mut r = Reader::with_base_offset(&bytes, 8);
        assert_eq!(
            r.read_postings_block(Some(u32::MAX - 200), &mut out),
            Err(corrupt(8, Corruption::ValueOverflow { bits: 32 }))
        );
        assert_eq!(r.position(), 0);
        let mut r = Reader::new(&[2, 0xff, 0xff, 0xff, 0xff, 0x0f, 0]);
        assert_eq!(
            r.read_postings_tail(None, &mut out),
            Err(corrupt(0, Corruption::ValueOverflow { bits: 32 }))
        );
    }

    #[test]
    fn invalid_width_and_count_are_corrupt() {
        let mut out = [0; BLOCK_LEN];
        let bytes = [0x7f, 0, 0, 0, 33];
        assert_eq!(
            Reader::new(&bytes).read_postings_block(None, &mut out),
            Err(corrupt(
                4,
                Corruption::InvalidBitWidth { width: 33, max: 32 }
            ))
        );
        for count in [0_u8, 128, 255] {
            assert_eq!(
                Reader::with_base_offset(&[count, 1, 0], 3).read_postings_tail(None, &mut out),
                Err(corrupt(
                    3,
                    Corruption::InvalidCount {
                        count: u32::from(count),
                        min: 1,
                        max: 127
                    }
                ))
            );
        }
    }

    #[test]
    fn truncated_blocks_are_eof_without_moving() {
        let docids: [u32; BLOCK_LEN] = core::array::from_fn(|i| 3 * i as u32);
        let bytes = full(&docids, None);
        let mut out = [0; BLOCK_LEN];
        for len in 0..bytes.len() {
            let mut r = Reader::new(&bytes[..len]);
            assert!(r.read_postings_block(None, &mut out).is_err(), "len {len}");
            assert!(r.clone().skip_postings_block(None).is_err(), "len {len}");
            assert_eq!(r.position(), 0);
        }
        let bytes = tail(&[1, 2, 300], None);
        for len in 0..bytes.len() {
            let mut r = Reader::new(&bytes[..len]);
            assert!(r.read_postings_tail(None, &mut out).is_err(), "len {len}");
            assert_eq!(r.position(), 0);
        }
    }

    #[test]
    fn advance_to_finds_the_first_docid_at_or_after_target() {
        let docids = [2, 4, 6, 8, 10, 12, 14, 16, 18, 20];
        for from in 0..=docids.len() + 1 {
            for target in 0..=22 {
                let expected = (from..docids.len())
                    .find(|&i| docids[i] >= target)
                    .unwrap_or(docids.len());
                assert_eq!(
                    advance_to(&docids, from, target),
                    expected,
                    "from {from} target {target}"
                );
            }
        }
        assert_eq!(advance_to(&[], 0, 5), 0);
        assert_eq!(advance_to(&[u32::MAX], 0, u32::MAX), 0);
    }
}
