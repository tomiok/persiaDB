//! Frame-of-reference (FOR) blocks for numeric columns, and order-preserving `u64` keys for `i64`/`f64`
//! (SPEC §5.2.1 "Frame-of-reference (FOR) block", "Order-preserving keys"; ADR-0004).
//!
//! "Frame" here is the codec's traditional name and has nothing to do with container Frames (SPEC §4.4).
//!
//! ```text
//! 0   8  min    u64  (order-preserving key)
//! 8   4  count  u32
//! 12  1  width  u8   (0..=64)
//! 13  ceil(count·width / 8) bytes: value i - min at bits [i·w, (i+1)·w), little-endian, LSB first
//! ```
//!
//! The layout is horizontal so any value can be read in O(1) by docid. The block is validated once when it is
//! read ([`Reader::read_for_block`]); after that, every access is infallible.
//!
//! `count` is not bounded by the input length: at width 0 (a constant column) a 13-byte block can claim
//! `u32::MAX` values. Callers must check [`ForBlock::len`] against the number of values they expect (e.g.
//! the segment's doc count) before iterating or allocating.

use crate::error::{Corruption, EncodeError, Error, Result};
use crate::reader::Reader;
use crate::writer::Writer;

/// The widest width: values spanning the whole `u64` range.
pub const MAX_WIDTH: u8 = 64;

const SIGN: u64 = 1 << 63;

/// The key of an `i64`: unsigned key order is signed value order.
pub const fn i64_to_key(value: i64) -> u64 {
    // Reinterpreting the bits is the point.
    #[allow(clippy::cast_sign_loss)]
    let bits = value as u64;
    bits ^ SIGN
}

/// Inverse of [`i64_to_key`].
pub const fn key_to_i64(key: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    let value = (key ^ SIGN) as i64;
    value
}

/// The key of an `f64`: unsigned key order is [`f64::total_cmp`] order, and every bit pattern (including NaN
/// payloads and `-0.0`) round-trips.
pub const fn f64_to_key(value: f64) -> u64 {
    let bits = value.to_bits();
    if bits & SIGN == 0 { bits | SIGN } else { !bits }
}

/// Inverse of [`f64_to_key`].
pub const fn key_to_f64(key: u64) -> f64 {
    f64::from_bits(if key & SIGN == 0 { !key } else { key ^ SIGN })
}

impl Writer {
    /// Appends `keys` as a FOR block, using their minimum and the minimal width (SPEC §5.2.1).
    ///
    /// # Errors
    /// [`EncodeError::InvalidLen`] if there are more than `u32::MAX` keys; nothing is written then.
    pub fn write_for_block(&mut self, keys: &[u64]) -> Result<(), EncodeError> {
        let count = u32::try_from(keys.len()).map_err(|_| EncodeError::InvalidLen {
            len: keys.len(),
            min: 0,
            max: usize::try_from(u32::MAX).unwrap_or(usize::MAX),
        })?;
        let min = keys.iter().copied().min().unwrap_or(0);
        let max = keys.iter().copied().max().unwrap_or(0);
        // max >= min, so this cannot wrap; bit_width() <= 64 fits a u8.
        let width = max.wrapping_sub(min).bit_width();
        self.write_u64(min);
        self.write_u32(count);
        self.write_u8(u8::try_from(width).unwrap_or(MAX_WIDTH));
        let (mut acc, mut filled) = (0_u128, 0_u32);
        for &key in keys {
            // filled < 8 and the delta has at most 64 bits: below 2^72, inside the u128.
            acc |= u128::from(key.wrapping_sub(min)) << filled;
            filled = filled.wrapping_add(width);
            while filled >= 8 {
                self.write_u8(acc.to_le_bytes()[0]);
                acc >>= 8;
                filled = filled.wrapping_sub(8);
            }
        }
        if filled > 0 {
            self.write_u8(acc.to_le_bytes()[0]);
        }
        Ok(())
    }
}

/// A validated FOR block borrowed from its input.
#[derive(Debug, Clone, Copy)]
pub struct ForBlock<'a> {
    min: u64,
    count: u32,
    width: u32,
    packed: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Reads and validates a FOR block.
    ///
    /// # Errors
    /// [`Error::Corrupt`] on short input, a width above 64 ([`Corruption::InvalidBitWidth`], at the width
    /// byte), non-zero unused bits in the last byte ([`Corruption::NonZeroReserved`], at that byte), or a value
    /// past `u64::MAX` ([`Corruption::ValueOverflow`], at the block start). The position is then unchanged.
    pub fn read_for_block(&mut self) -> Result<ForBlock<'a>> {
        let start = self.offset();
        let mut r = self.clone();
        let min = r.read_u64()?;
        let count = r.read_u32()?;
        let width_at = r.offset();
        let raw_width = r.read_u8()?;
        if raw_width > MAX_WIDTH {
            return Err(Error::Corrupt {
                offset: width_at,
                reason: Corruption::InvalidBitWidth {
                    width: raw_width,
                    max: MAX_WIDTH,
                },
            });
        }
        let width = u32::from(raw_width);
        // count < 2^32 and width <= 64: the product is below 2^38.
        let bits = u64::from(count).wrapping_mul(u64::from(width));
        // A length that does not fit in memory cannot be present: let read_bytes report it as short input.
        let len = usize::try_from(bits.div_ceil(8)).unwrap_or(usize::MAX);
        let packed_at = r.offset();
        let packed = r.read_bytes(len)?;
        let used_in_last = u32::try_from(bits & 7).unwrap_or(0);
        if let (Some(&last), true) = (packed.last(), used_in_last != 0)
            && last >> used_in_last != 0
        {
            return Err(Error::Corrupt {
                offset: packed_at.saturating_add(bits >> 3),
                reason: Corruption::NonZeroReserved { value: last },
            });
        }
        let block = ForBlock {
            min,
            count,
            width,
            packed,
        };
        // Only a minimum near the top of the range can overflow; then every value must be checked.
        if min.checked_add(block.mask()).is_none()
            && block.raws().any(|raw| min.checked_add(raw).is_none())
        {
            return Err(Error::Corrupt {
                offset: start,
                reason: Corruption::ValueOverflow { bits: u64::BITS },
            });
        }
        *self = r;
        Ok(block)
    }
}

impl ForBlock<'_> {
    /// Number of values.
    pub fn len(&self) -> usize {
        usize::try_from(self.count).unwrap_or(usize::MAX)
    }

    /// True when the block holds no values.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The smallest key the block can hold (the frame of reference).
    pub fn min(&self) -> u64 {
        self.min
    }

    /// Bits per value (0..=64).
    pub fn width(&self) -> u8 {
        u8::try_from(self.width).unwrap_or(MAX_WIDTH)
    }

    /// The key at `index`, or `None` past the end.
    pub fn get(&self, index: usize) -> Option<u64> {
        if index >= self.len() {
            return None;
        }
        // Validated when the block was read: min + raw does not overflow.
        Some(self.min.wrapping_add(self.raw(index)))
    }

    /// All keys, in order.
    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        // Validated when the block was read: min + raw does not overflow.
        self.raws().map(|raw| self.min.wrapping_add(raw))
    }

    fn raws(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len()).map(|i| self.raw(i))
    }

    /// The `width` bits of value `index`; `index < len`, so they are inside `packed`.
    fn raw(&self, index: usize) -> u64 {
        // index < 2^32 and width <= 64: below 2^38.
        let bit = crate::error::to_u64(index).wrapping_mul(u64::from(self.width));
        let byte = usize::try_from(bit >> 3).unwrap_or(usize::MAX);
        let shift = bit & 7;
        // The value spans at most 9 bytes (7 bits of shift + 64 bits); load up to 16 into a u128.
        let mut window = [0_u8; 16];
        let src = self.packed.get(byte..).unwrap_or_default();
        for (dst, &b) in window.iter_mut().zip(src) {
            *dst = b;
        }
        let bits = u128::from_le_bytes(window) >> shift;
        // Truncation to the low 64 bits, then the width mask.
        #[allow(clippy::cast_possible_truncation)]
        let low = bits as u64;
        low & self.mask()
    }

    fn mask(&self) -> u64 {
        if self.width == 0 {
            0
        } else {
            // 1 <= width <= 64, so the shift is 0..=63.
            u64::MAX >> u64::BITS.wrapping_sub(self.width)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(keys: &[u64]) -> Vec<u8> {
        let mut w = Writer::new();
        w.write_for_block(keys).unwrap();
        w.into_inner()
    }

    fn decode(bytes: &[u8]) -> Result<Vec<u64>> {
        let mut r = Reader::new(bytes);
        let block = r.read_for_block()?;
        assert!(r.is_empty());
        Ok(block.iter().collect())
    }

    fn corrupt(offset: u64, reason: Corruption) -> Error {
        Error::Corrupt { offset, reason }
    }

    /// Golden block (SPEC §5.2.1): keys [10, 12, 11] are min 10, width 2, offsets 0, 2, 1 packed LSB first:
    /// `0b00_01_10_00` = `0x18`.
    #[test]
    fn golden_block() {
        let mut expected = vec![10, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 2];
        expected.push(0x18);
        assert_eq!(encode(&[10, 12, 11]), expected);
        assert_eq!(decode(&expected), Ok(vec![10, 12, 11]));
    }

    #[test]
    fn golden_empty_and_constant_blocks() {
        assert_eq!(encode(&[]), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let constant = encode(&[7; 1000]);
        assert_eq!(constant.len(), 13, "width 0: no packed bytes");
        assert_eq!(decode(&constant), Ok(vec![7; 1000]));
    }

    #[test]
    fn full_range_uses_width_64() {
        let keys = [u64::MAX, 0, 1 << 63, u64::MAX - 1];
        let bytes = encode(&keys);
        assert_eq!(bytes[12], 64);
        assert_eq!(bytes.len(), 13 + 32);
        assert_eq!(decode(&bytes), Ok(keys.to_vec()));
    }

    #[test]
    fn get_is_random_access() {
        let keys: Vec<u64> = (0..100).map(|i| 1_000_000 + i * i * 37 % 1001).collect();
        let bytes = encode(&keys);
        let block = Reader::new(&bytes).read_for_block().unwrap();
        assert_eq!(block.len(), 100);
        assert_eq!(block.min(), *keys.iter().min().unwrap());
        for (i, &k) in keys.iter().enumerate().rev() {
            assert_eq!(block.get(i), Some(k));
        }
        assert_eq!(block.get(100), None);
        assert_eq!(block.get(usize::MAX), None);
    }

    #[test]
    fn invalid_width_is_corrupt_at_the_width_byte() {
        let mut bytes = encode(&[1, 2]);
        bytes[12] = 65;
        let mut r = Reader::with_base_offset(&bytes, 100);
        assert_eq!(
            r.read_for_block().map(|b| b.len()),
            Err(corrupt(
                112,
                Corruption::InvalidBitWidth { width: 65, max: 64 }
            ))
        );
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn unused_bits_must_be_zero() {
        let mut bytes = encode(&[10, 12, 11]); // 6 bits used in the only packed byte
        bytes[13] |= 0x40;
        assert_eq!(
            decode(&bytes),
            Err(corrupt(13, Corruption::NonZeroReserved { value: 0x58 }))
        );
    }

    #[test]
    fn overflowing_values_are_corrupt() {
        // min u64::MAX - 1 with width 2: offsets 0..=1 are fine, 2 and 3 overflow.
        let mut ok = vec![];
        ok.extend((u64::MAX - 1).to_le_bytes());
        ok.extend(2_u32.to_le_bytes());
        ok.extend([2, 0b0100]); // offsets 0, 1
        assert_eq!(decode(&ok), Ok(vec![u64::MAX - 1, u64::MAX]));
        let mut bad = ok.clone();
        bad[13] = 0b1000; // offsets 0, 2
        let mut r = Reader::with_base_offset(&bad, 7);
        assert_eq!(
            r.read_for_block().map(|b| b.len()),
            Err(corrupt(7, Corruption::ValueOverflow { bits: 64 }))
        );
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn truncated_blocks_are_eof_without_moving() {
        let bytes = encode(&[1, 99, 3, 4000]);
        for len in 0..bytes.len() {
            let mut r = Reader::new(&bytes[..len]);
            assert!(r.read_for_block().is_err(), "len {len}");
            assert_eq!(r.position(), 0);
        }
        // A count that claims far more data than exists is short input, not an allocation.
        let mut huge = encode(&[1, 2]);
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        huge[12] = 64;
        assert!(matches!(
            decode(&huge),
            Err(Error::Corrupt {
                offset: 13,
                reason: Corruption::UnexpectedEof { .. }
            })
        ));
    }

    #[test]
    fn i64_keys_preserve_order_and_round_trip() {
        let values = [i64::MIN, -1_000, -1, 0, 1, 1_000, i64::MAX];
        let keys: Vec<u64> = values.iter().map(|&v| i64_to_key(v)).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(keys[0], 0);
        assert_eq!(keys[3], 1 << 63);
        assert_eq!(keys[6], u64::MAX);
        for (&v, &k) in values.iter().zip(&keys) {
            assert_eq!(key_to_i64(k), v);
        }
    }

    #[test]
    fn f64_keys_follow_total_order_and_round_trip() {
        let values = [
            -f64::NAN,
            f64::NEG_INFINITY,
            f64::MIN,
            -1.5,
            -f64::MIN_POSITIVE,
            -0.0,
            0.0,
            f64::MIN_POSITIVE,
            1.5,
            f64::MAX,
            f64::INFINITY,
            f64::NAN,
        ];
        let keys: Vec<u64> = values.iter().map(|&v| f64_to_key(v)).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:x?}");
        assert_eq!(f64_to_key(0.0), 1 << 63);
        assert_eq!(f64_to_key(-0.0), (1 << 63) - 1);
        for (&v, &k) in values.iter().zip(&keys) {
            assert_eq!(key_to_f64(k).to_bits(), v.to_bits());
        }
    }
}
