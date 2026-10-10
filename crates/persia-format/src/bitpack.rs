//! Bit-packing of 128-value `u32` blocks, widths 0..=32 (SPEC §5.2.1 "Bit-packed block", ADR-0004).
//!
//! Layout: the block is `4·w` little-endian `u32` words; word `k` is lane `k % 4`, row `k / 4`. Value `i`
//! belongs to lane `i % 4`, slot `i / 4`; each lane is a little-endian bit stream over its `w` words and slot
//! `j` holds bits `[j·w, (j+1)·w)`. This is the SIMD-BP128 vertical layout, so a SIMD decoder can later read
//! the same bytes. The code here is the scalar baseline.
//!
//! Any byte string of the right length decodes (every bit pattern is a valid block): the decoder's only
//! failure modes are an invalid width and short input.

use crate::error::{Corruption, EncodeError, Error, Result};
use crate::reader::Reader;
use crate::writer::Writer;

/// Values per block.
pub const BLOCK_LEN: usize = 128;

/// Interleaved lanes: one row of 4 `u32` words is one 128-bit SIMD register.
const LANES: usize = 4;

/// A bit width in `0..=32`: the number of bits each value occupies in a packed block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BitWidth(u8);

impl BitWidth {
    /// The widest width: values of any `u32`.
    pub const MAX: Self = Self(32);

    /// `width` if it is at most 32.
    pub const fn new(width: u8) -> Option<Self> {
        if width <= Self::MAX.0 {
            Some(Self(width))
        } else {
            None
        }
    }

    /// The minimal width that holds every value: the bit length of the largest (0 for an all-zero or
    /// empty block). Writers must use it (SPEC §5.2.1), so output is deterministic.
    pub fn of(values: &[u32]) -> Self {
        let any_bits = values.iter().fold(0, |acc, &v| acc | v);
        // bit_width() <= 32, so the conversion cannot fail.
        Self(u8::try_from(any_bits.bit_width()).unwrap_or(Self::MAX.0))
    }

    /// The width in bits.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Bytes of a packed block at this width: `16 · width` (128 values × width bits / 8).
    pub const fn packed_len(self) -> usize {
        // At most 512: cannot overflow.
        (self.0 as usize).wrapping_mul(BLOCK_LEN / 8)
    }

    /// The largest value that fits.
    fn mask(self) -> u64 {
        // width <= 32, so the shift is in range and the result is at least 0.
        (1_u64 << self.0).wrapping_sub(1)
    }
}

impl Writer {
    /// Appends a width as one byte.
    pub fn write_bit_width(&mut self, width: BitWidth) {
        self.write_u8(width.get());
    }

    /// Appends `values` bit-packed at `width` ([`BitWidth::packed_len`] bytes).
    ///
    /// # Errors
    /// [`EncodeError::ValueTooWide`] for the first value that needs more than `width` bits; nothing is
    /// written then.
    pub fn write_packed_block(
        &mut self,
        values: &[u32; BLOCK_LEN],
        width: BitWidth,
    ) -> Result<(), EncodeError> {
        let mask = width.mask();
        if let Some(index) = values.iter().position(|&v| u64::from(v) > mask) {
            return Err(EncodeError::ValueTooWide {
                index,
                width: width.get(),
            });
        }
        let w = u32::from(width.get());
        let mut words = [0_u32; BLOCK_LEN];
        for lane in 0..LANES {
            // Words of this lane, in row order: lane, lane + 4, lane + 8, ...
            let mut out = words.iter_mut().skip(lane).step_by(LANES);
            let (mut acc, mut filled) = (0_u64, 0_u32);
            for &value in values.iter().skip(lane).step_by(LANES) {
                // filled < 32 and value < 2^w <= 2^32, so the bits stay below 2^64.
                acc |= u64::from(value) << filled;
                filled = filled.wrapping_add(w);
                if filled >= u32::BITS {
                    if let Some(word) = out.next() {
                        *word = low_u32(acc);
                    }
                    acc >>= u32::BITS;
                    filled = filled.wrapping_sub(u32::BITS);
                }
            }
        }
        // 32 slots of w bits fill exactly w words per lane: 4·w words in all.
        for word in words.iter().take(width.packed_len() / 4) {
            self.write_u32(*word);
        }
        Ok(())
    }
}

impl Reader<'_> {
    /// Reads a one-byte width.
    ///
    /// # Errors
    /// [`Error::Corrupt`] with [`Corruption::InvalidBitWidth`] if it is above 32, or on short input; the
    /// position is then unchanged.
    pub fn read_bit_width(&mut self) -> Result<BitWidth> {
        let offset = self.offset();
        let mut r = self.clone();
        let raw = r.read_u8()?;
        let width = BitWidth::new(raw).ok_or(Error::Corrupt {
            offset,
            reason: Corruption::InvalidBitWidth {
                width: raw,
                max: BitWidth::MAX.get(),
            },
        })?;
        *self = r;
        Ok(width)
    }

    /// Reads a block packed at `width` into `out`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] if fewer than [`BitWidth::packed_len`] bytes remain; the position and `out` are
    /// then unchanged.
    pub fn read_packed_block(&mut self, width: BitWidth, out: &mut [u32; BLOCK_LEN]) -> Result<()> {
        let bytes = self.read_bytes(width.packed_len())?;
        let (words, _) = bytes.as_chunks::<4>();
        let w = u32::from(width.get());
        let mask = width.mask();
        for lane in 0..LANES {
            let mut lane_words = words
                .iter()
                .skip(lane)
                .step_by(LANES)
                .map(|&b| u32::from_le_bytes(b));
            let (mut acc, mut available) = (0_u64, 0_u32);
            for slot in out.iter_mut().skip(lane).step_by(LANES) {
                if available < w {
                    // available < w <= 32: the new word lands above the bits still buffered, below 2^64.
                    acc |= u64::from(lane_words.next().unwrap_or(0)) << available;
                    available = available.wrapping_add(u32::BITS);
                }
                *slot = low_u32(acc & mask);
                acc >>= w; // w <= 32 < 64
                available = available.wrapping_sub(w);
            }
        }
        Ok(())
    }
}

/// The low 32 bits of `x`.
fn low_u32(x: u64) -> u32 {
    // Truncation is the point.
    #[allow(clippy::cast_possible_truncation)]
    let low = x as u32;
    low
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test values are built from small indices
mod tests {
    use super::*;

    fn pack(values: &[u32; BLOCK_LEN], width: u8) -> Vec<u8> {
        let mut w = Writer::new();
        w.write_packed_block(values, BitWidth::new(width).unwrap())
            .unwrap();
        w.into_inner()
    }

    fn unpack(bytes: &[u8], width: u8) -> [u32; BLOCK_LEN] {
        let mut out = [0; BLOCK_LEN];
        Reader::new(bytes)
            .read_packed_block(BitWidth::new(width).unwrap(), &mut out)
            .unwrap();
        out
    }

    /// Golden layout (SPEC §5.2.1): with width 1 and value `i % 2`, lanes 1 and 3 are all ones, so words
    /// 1 and 3 are `0xffff_ffff` and words 0 and 2 are zero.
    #[test]
    fn golden_width_1_lanes() {
        let values: [u32; BLOCK_LEN] = core::array::from_fn(|i| (i % 2) as u32);
        let mut expected = [0_u8; 16];
        expected[4..8].fill(0xff);
        expected[12..16].fill(0xff);
        assert_eq!(pack(&values, 1), expected);
        assert_eq!(unpack(&expected, 1), values);
    }

    /// Golden slot order: with width 2 and value `(i / 4) % 4`, every lane holds slots 0, 1, 2, 3, ...
    /// least significant bits first, so every byte is `0b11_10_01_00`.
    #[test]
    fn golden_width_2_slots() {
        let values: [u32; BLOCK_LEN] = core::array::from_fn(|i| ((i / 4) % 4) as u32);
        assert_eq!(pack(&values, 2), [0xe4; 32]);
    }

    /// Golden width 32: words are the values themselves, in order.
    #[test]
    fn golden_width_32_is_plain_little_endian() {
        let values: [u32; BLOCK_LEN] = core::array::from_fn(|i| 0x0101_0101 * i as u32);
        let expected: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(pack(&values, 32), expected);
    }

    #[test]
    fn width_0_is_empty_and_decodes_zeros() {
        assert_eq!(pack(&[0; BLOCK_LEN], 0), [] as [u8; 0]);
        let mut out = [7; BLOCK_LEN];
        let mut r = Reader::new(&[1, 2]);
        r.read_packed_block(BitWidth::new(0).unwrap(), &mut out)
            .unwrap();
        assert_eq!(out, [0; BLOCK_LEN]);
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn every_width_round_trips_its_extremes() {
        for width in 0..=32_u8 {
            let max = (1_u64 << width) - 1;
            let values: [u32; BLOCK_LEN] = core::array::from_fn(|i| {
                if i % 3 == 0 {
                    max as u32
                } else {
                    (i as u64 & max) as u32
                }
            });
            let bytes = pack(&values, width);
            assert_eq!(bytes.len(), 16 * usize::from(width), "width {width}");
            assert_eq!(unpack(&bytes, width), values, "width {width}");
        }
    }

    #[test]
    fn minimal_width() {
        assert_eq!(BitWidth::of(&[]).get(), 0);
        assert_eq!(BitWidth::of(&[0; BLOCK_LEN]).get(), 0);
        assert_eq!(BitWidth::of(&[1]).get(), 1);
        assert_eq!(BitWidth::of(&[4, 3]).get(), 3);
        assert_eq!(BitWidth::of(&[255]).get(), 8);
        assert_eq!(BitWidth::of(&[256]).get(), 9);
        assert_eq!(BitWidth::of(&[u32::MAX]).get(), 32);
    }

    #[test]
    fn too_wide_values_are_rejected_before_writing() {
        let mut values = [1; BLOCK_LEN];
        values[77] = 2;
        let mut w = Writer::new();
        assert_eq!(
            w.write_packed_block(&values, BitWidth::new(1).unwrap()),
            Err(EncodeError::ValueTooWide {
                index: 77,
                width: 1
            })
        );
        assert!(w.is_empty());
        assert_eq!(
            w.write_packed_block(&values, BitWidth::new(0).unwrap()),
            Err(EncodeError::ValueTooWide { index: 0, width: 0 })
        );
    }

    #[test]
    fn widths_above_32_are_corrupt() {
        assert_eq!(BitWidth::new(33), None);
        assert_eq!(BitWidth::new(32), Some(BitWidth::MAX));
        let mut r = Reader::with_base_offset(&[33, 0], 40);
        assert_eq!(
            r.read_bit_width(),
            Err(Error::Corrupt {
                offset: 40,
                reason: Corruption::InvalidBitWidth { width: 33, max: 32 },
            })
        );
        assert_eq!(r.position(), 0);
        let mut r = Reader::new(&[32]);
        assert_eq!(r.read_bit_width(), Ok(BitWidth::MAX));
        assert!(r.is_empty());
    }

    #[test]
    fn short_input_is_eof_and_leaves_output_untouched() {
        let mut out = [9; BLOCK_LEN];
        let mut r = Reader::with_base_offset(&[0; 47], 100);
        assert_eq!(
            r.read_packed_block(BitWidth::new(3).unwrap(), &mut out),
            Err(Error::Corrupt {
                offset: 100,
                reason: Corruption::UnexpectedEof {
                    needed: 48,
                    available: 47
                },
            })
        );
        assert_eq!(out, [9; BLOCK_LEN]);
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn decode_consumes_exactly_the_packed_length() {
        let values = [5; BLOCK_LEN];
        let mut bytes = pack(&values, 3);
        bytes.push(0xaa);
        let mut r = Reader::new(&bytes);
        let mut out = [0; BLOCK_LEN];
        r.read_packed_block(BitWidth::new(3).unwrap(), &mut out)
            .unwrap();
        assert_eq!(out, values);
        assert_eq!(r.rest(), &[0xaa]);
    }
}
