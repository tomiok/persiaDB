//! Frame-of-reference (FOR) blocks for numeric columns, and order-preserving `u64` keys for `i64`/`f64`
//! (SPEC §5.2.1 "Frame-of-reference (FOR) block", "Order-preserving keys"; ADR-0004).
//!
//! "Frame" here is the codec's traditional name and has nothing to do with container Frames (SPEC §4.4).
//!
//! ```text
//! 0   8  min    u64  (order-preserving key)
//! 8   4  count  u32  (1..=1024)
//! 12  1  width  u8   (0..=64)
//! 13  ceil(count·width / 8) bytes: value i - min at bits [i·w, (i+1)·w), little-endian, LSB first
//! ```
//!
//! The layout is horizontal so any value can be read in O(1) by docid. The block is validated once when it is
//! read ([`Reader::read_for_block`]); after that, every access is infallible.
//!
//! A column is cut into blocks of exactly [`VALUES_PER_BLOCK`] values (the last may be shorter), so the block holding
//! docid `d` is block `d / VALUES_PER_BLOCK`. Readers reject a `count` outside `1..=VALUES_PER_BLOCK`.

use crate::error::{Corruption, EncodeError, Error, Result};
use crate::reader::Reader;
use crate::writer::Writer;

/// Values per FOR block; only the last block of a column may hold fewer (SPEC §5.2.1).
pub const VALUES_PER_BLOCK: usize = 1024;

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
    /// [`EncodeError::InvalidLen`] unless there are 1..=[`VALUES_PER_BLOCK`] keys; nothing is written then.
    pub fn write_for_block(&mut self, keys: &[u64]) -> Result<(), EncodeError> {
        let count = u32::try_from(keys.len())
            .ok()
            .filter(|_| (1..=VALUES_PER_BLOCK).contains(&keys.len()))
            .ok_or(EncodeError::InvalidLen {
                len: keys.len(),
                min: 1,
                max: VALUES_PER_BLOCK,
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
    /// [`Error::Corrupt`] on short input, a count outside 1..=[`VALUES_PER_BLOCK`] ([`Corruption::InvalidCount`], at
    /// the count field), a width above 64 ([`Corruption::InvalidBitWidth`], at the width
    /// byte), non-zero unused bits in the last byte ([`Corruption::NonZeroReserved`], at that byte), or a value
    /// past `u64::MAX` ([`Corruption::ValueOverflow`], at the block start). The position is then unchanged.
    pub fn read_for_block(&mut self) -> Result<ForBlock<'a>> {
        let start = self.offset();
        let mut r = self.clone();
        let min = r.read_u64()?;
        let count_at = r.offset();
        let count = r.read_u32()?;
        let max_count = u32::try_from(VALUES_PER_BLOCK).unwrap_or(u32::MAX);
        if !(1..=max_count).contains(&count) {
            return Err(Error::Corrupt {
                offset: count_at,
                reason: Corruption::InvalidCount {
                    count,
                    min: 1,
                    max: max_count,
                },
            });
        }
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
        // count <= 1024 and width <= 64: the product is below 2^17.
        let bits = u64::from(count).wrapping_mul(u64::from(width));
        // At most 8 KiB (bits < 2^17), so the conversion cannot fail.
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
        if min.checked_add(block.mask()).is_none() && min.checked_add(block.max_raw()).is_none() {
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

    /// Always false for a block that was read successfully (`count` is at least 1); kept next to `len`.
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
        (0..self.len()).map(|i| self.min.wrapping_add(self.raw(i)))
    }

    /// Appends every key to `out`, in order: what [`Self::iter`] yields, unpacked in one pass (for a reader that
    /// keeps a decoded block, ROADMAP 5.3.16).
    pub fn decode_into(&self, out: &mut Vec<u64>) {
        let start = out.len();
        out.resize(start.saturating_add(self.len()), 0);
        let keys = out.get_mut(start..).unwrap_or_default();
        self.unpack(keys);
        // Validated when the block was read: min + raw does not overflow.
        let min = self.min;
        for k in keys {
            *k = min.wrapping_add(*k);
        }
    }

    /// The largest raw value.
    fn max_raw(&self) -> u64 {
        let mut raws = [0_u64; VALUES_PER_BLOCK];
        let raws = raws.get_mut(..self.len()).unwrap_or_default();
        self.unpack(raws);
        raws.iter().copied().max().unwrap_or(0)
    }

    /// Writes the raw values into `out`, which holds `len` of them.
    fn unpack(&self, out: &mut [u64]) {
        let width = u64::from(self.width);
        if width == 0 {
            out.fill(0);
            return;
        }
        if width == 64 {
            let (whole, _) = self.packed.as_chunks::<8>();
            for (o, c) in out.iter_mut().zip(whole) {
                *o = u64::from_le_bytes(*c);
            }
            return;
        }
        // The packed bits as little-endian words, zero-padded by one: a value spans at most two of them.
        let mut words = [0_u64; VALUES_PER_BLOCK + 1];
        let (whole, rest) = self.packed.as_chunks::<8>();
        for (w, c) in words.iter_mut().zip(whole) {
            *w = u64::from_le_bytes(*c);
        }
        if let Some(w) = words.get_mut(whole.len()) {
            let mut last = [0_u8; 8];
            for (dst, &b) in last.iter_mut().zip(rest) {
                *dst = b;
            }
            *w = u64::from_le_bytes(last);
        }
        let mask = self.mask();
        for (i, o) in out.iter_mut().enumerate() {
            // i < 1024 and width <= 64: below 2^17, so the word index is at most 1024.
            let bit = crate::error::to_u64(i).wrapping_mul(width);
            let at = usize::try_from(bit >> 6).unwrap_or(usize::MAX);
            let shift = bit & 63;
            let lo = words.get(at).copied().unwrap_or(0) >> shift;
            // `<< 1 << (63 - shift)` is `<< (64 - shift)`, and 0 when the value starts on a word boundary.
            let hi = (words.get(at.wrapping_add(1)).copied().unwrap_or(0) << 1) << (63 ^ shift);
            *o = (lo | hi) & mask;
        }
    }

    /// The `width` bits of value `index`; `index < len`, so they are inside `packed`.
    fn raw(&self, index: usize) -> u64 {
        // index < 1024 and width <= 64: below 2^17.
        let bit = crate::error::to_u64(index).wrapping_mul(u64::from(self.width));
        let byte = usize::try_from(bit >> 3).unwrap_or(usize::MAX);
        let shift = bit & 7;
        // The value spans at most 9 bytes (7 bits of shift + 64 bits): load 16 into a u128 in one go, or copy
        // what is left near the end of the block.
        let src = self.packed.get(byte..).unwrap_or_default();
        let window = if let Some(chunk) = src.first_chunk::<16>() {
            *chunk
        } else {
            let mut window = [0_u8; 16];
            for (dst, &b) in window.iter_mut().zip(src) {
                *dst = b;
            }
            window
        };
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
    fn golden_constant_block_and_count_limits() {
        let constant = encode(&[7; VALUES_PER_BLOCK]);
        let mut expected = vec![7, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0]; // min 7, count 1024, width 0
        assert_eq!(constant, expected, "width 0: no packed bytes");
        assert_eq!(decode(&constant), Ok(vec![7; VALUES_PER_BLOCK]));

        let mut w = Writer::new();
        for keys in [&[][..], &[0; VALUES_PER_BLOCK + 1][..]] {
            assert_eq!(
                w.write_for_block(keys),
                Err(EncodeError::InvalidLen {
                    len: keys.len(),
                    min: 1,
                    max: VALUES_PER_BLOCK
                })
            );
        }
        assert!(w.is_empty());

        for count in [0_u32, 1025, u32::MAX] {
            expected[8..12].copy_from_slice(&count.to_le_bytes());
            let mut r = Reader::with_base_offset(&expected, 40);
            assert_eq!(
                r.read_for_block().map(|b| b.len()),
                Err(corrupt(
                    48,
                    Corruption::InvalidCount {
                        count,
                        min: 1,
                        max: 1024
                    }
                ))
            );
            assert_eq!(r.position(), 0);
        }
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
        // A count that claims more data than exists is short input.
        let mut huge = encode(&[1, 2]);
        huge[8..12].copy_from_slice(&1024_u32.to_le_bytes());
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

    // ROADMAP 5.3.16: `decode_into` and the overflow check through `max_raw` unpack whole 64-bit words; the last
    // word of a block is usually partial. These tests pin both against `get`/`iter` (bit-window reads) and a
    // per-value overflow check, at every width. Sizes shrink under Miri (`just miri` runs the lib tests).

    /// Widths to sweep: all of them natively, the word-boundary ones under Miri.
    fn widths() -> Vec<u32> {
        if cfg!(miri) {
            vec![0, 1, 7, 13, 33, 63, 64]
        } else {
            (0..=64).collect()
        }
    }

    /// Counts around the 64-value boundary (where `count·width` fills whole words) and the block size.
    fn counts() -> &'static [usize] {
        if cfg!(miri) {
            &[1, 8, 63, 65]
        } else {
            &[1, 2, 7, 8, 9, 63, 64, 65, 127, 128, 129, 1023, 1024]
        }
    }

    fn mask_of(width: u32) -> u64 {
        if width == 0 {
            0
        } else {
            u64::MAX >> (64 - width)
        }
    }

    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state ^ (*state >> 29)
    }

    /// A FOR block packed bit by bit from the SPEC §5.2.1 table, independent of the writer: value `i` at stream
    /// bits `[i·w, (i+1)·w)`, LSB first.
    fn pack(min: u64, width: u32, raws: &[u64]) -> Vec<u8> {
        let w = width as usize;
        let mut out = Vec::new();
        out.extend_from_slice(&min.to_le_bytes());
        out.extend_from_slice(&u32::try_from(raws.len()).unwrap().to_le_bytes());
        out.push(u8::try_from(width).unwrap());
        let mut packed = vec![0_u8; (raws.len() * w).div_ceil(8)];
        for (i, &raw) in raws.iter().enumerate() {
            for b in 0..w {
                if raw >> b & 1 == 1 {
                    let bit = i * w + b;
                    packed[bit / 8] |= 1 << (bit % 8);
                }
            }
        }
        out.extend_from_slice(&packed);
        out
    }

    /// `count` raw values below `mask`, with `mask` itself at the last index (the partial last word carries the
    /// widest value) and 0 at the first.
    fn raws(width: u32, count: usize, seed: u64) -> Vec<u64> {
        let mask = mask_of(width);
        let mut state = seed ^ (u64::from(width) << 32) ^ count as u64;
        let mut raws: Vec<u64> = (0..count).map(|_| lcg(&mut state) & mask).collect();
        raws[0] = 0;
        raws[count - 1] = mask;
        raws
    }

    #[test]
    fn decode_into_appends_what_iter_yields_at_every_width_and_count() {
        for width in widths() {
            let mask = mask_of(width);
            for &count in counts() {
                let raws = raws(width, count, 0x5eed);
                let mins = if cfg!(miri) {
                    vec![u64::MAX - mask]
                } else {
                    vec![0, 0x0123_4567_89ab_cdef & !mask, u64::MAX - mask]
                };
                for min in mins {
                    let bytes = pack(min, width, &raws);
                    let mut r = Reader::new(&bytes);
                    let block = r.read_for_block().unwrap();
                    assert!(r.is_empty());
                    let keys: Vec<u64> = raws.iter().map(|&x| min + x).collect();
                    let ctx = format!("width {width} count {count} min {min:#x}");
                    assert_eq!(block.iter().collect::<Vec<_>>(), keys, "iter, {ctx}");
                    // Existing contents stay in front; nothing is appended past the block's values.
                    let mut out = vec![0xdead_beef, u64::MAX];
                    block.decode_into(&mut out);
                    assert_eq!(out[..2], [0xdead_beef, u64::MAX], "prefix, {ctx}");
                    assert_eq!(&out[2..], keys.as_slice(), "decode_into, {ctx}");
                    // Decoding twice appends twice: no state is kept between calls.
                    block.decode_into(&mut out);
                    assert_eq!(out.len(), 2 + 2 * count, "{ctx}");
                    assert_eq!(
                        &out[2 + count..],
                        keys.as_slice(),
                        "second decode_into, {ctx}"
                    );
                    assert_eq!(block.max_raw(), mask, "max_raw, {ctx}");
                    for (i, &k) in keys.iter().enumerate() {
                        assert_eq!(block.get(i), Some(k), "get({i}), {ctx}");
                    }
                }
            }
        }
    }

    /// `max_raw` finds the largest raw value wherever it sits (first, in a middle word, at a word boundary, last),
    /// so the overflow check built on it accepts `min = u64::MAX - max_raw` and rejects one more, as a per-value
    /// check would, including when the largest raw is below the width's mask.
    #[test]
    fn overflow_check_finds_the_largest_raw_anywhere() {
        let mut state = 7_u64;
        // Under Miri, one count with a partial last word and one inside the first word.
        let counts: &[usize] = if cfg!(miri) { &[1, 65] } else { counts() };
        for width in widths().into_iter().filter(|&w| w > 0) {
            let mask = mask_of(width);
            for &count in counts {
                // The largest raw: the mask, or a smaller value (min + mask then overflows
                // while min + max does not, the case that needs `max_raw` at all).
                for top in [mask, mask >> 1 | 1, (mask >> 1).max(1) + 1]
                    .into_iter()
                    .filter(|&t| t <= mask)
                {
                    let places = if cfg!(miri) {
                        vec![0, count - 1]
                    } else {
                        vec![0, count / 2, count - 1, 63, 64]
                    };
                    for at in places.into_iter().filter(|&a| a < count) {
                        let mut raws: Vec<u64> =
                            (0..count).map(|_| lcg(&mut state) % top).collect();
                        raws[at] = top;
                        for (min, fits) in [(u64::MAX - top, true), (u64::MAX - top + 1, false)] {
                            let fits_per_value = raws.iter().all(|&x| min.checked_add(x).is_some());
                            assert_eq!(fits, fits_per_value);
                            let bytes = pack(min, width, &raws);
                            let mut r = Reader::with_base_offset(&bytes, 1 << 33);
                            let got = r.read_for_block();
                            let ctx = format!(
                                "width {width} count {count} top {top:#x} at {at} min {min:#x}"
                            );
                            if fits {
                                let block = got.unwrap_or_else(|e| panic!("{ctx}: {e}"));
                                assert_eq!(block.max_raw(), top, "{ctx}");
                                let mut out = Vec::new();
                                block.decode_into(&mut out);
                                assert_eq!(out[at], u64::MAX, "{ctx}");
                                assert_eq!(out, block.iter().collect::<Vec<_>>(), "{ctx}");
                            } else {
                                assert_eq!(
                                    got.map(|b| b.len()),
                                    Err(corrupt(1 << 33, Corruption::ValueOverflow { bits: 64 })),
                                    "{ctx}"
                                );
                                assert_eq!(r.position(), 0, "{ctx}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// Width 0 holds no packed bits: every value is `min`, even `u64::MAX`, and `max_raw` is 0.
    #[test]
    fn width_zero_at_the_top_of_u64() {
        for &count in counts() {
            let bytes = pack(u64::MAX, 0, &vec![0; count]);
            assert_eq!(bytes.len(), 13);
            let block = Reader::new(&bytes).read_for_block().unwrap();
            assert_eq!(block.max_raw(), 0);
            let mut out = vec![1];
            block.decode_into(&mut out);
            assert_eq!(out.len(), count + 1);
            assert!(out[1..].iter().all(|&k| k == u64::MAX));
        }
    }
}
