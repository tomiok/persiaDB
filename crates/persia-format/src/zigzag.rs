//! Zigzag mapping of signed integers to unsigned ones (SPEC §4.1 "Zigzag").
//!
//! `0, -1, 1, -2, 2, ...` map to `0, 1, 2, 3, 4, ...`, so values of small magnitude stay small whatever
//! their sign and encode as short varints. The mapping is a bijection: every unsigned value decodes, so a
//! signed varint has no failure modes beyond those of the unsigned one.

use crate::error::Result;
use crate::reader::Reader;
use crate::writer::Writer;

/// Maps `value` to `(value << 1) ^ (value >> 31)` (arithmetic shift).
pub const fn encode_i32(value: i32) -> u32 {
    // Bit reinterpretation is the point: the sign is moved to bit 0, nothing is truncated.
    #[allow(clippy::cast_sign_loss)]
    let (shifted, sign) = ((value as u32).wrapping_shl(1), (value >> 31) as u32);
    shifted ^ sign
}

/// Inverse of [`encode_i32`].
pub const fn decode_i32(value: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    let (magnitude, sign) = ((value >> 1) as i32, (value & 1) as i32);
    magnitude ^ sign.wrapping_neg()
}

/// Maps `value` to `(value << 1) ^ (value >> 63)` (arithmetic shift).
pub const fn encode_i64(value: i64) -> u64 {
    #[allow(clippy::cast_sign_loss)]
    let (shifted, sign) = ((value as u64).wrapping_shl(1), (value >> 63) as u64);
    shifted ^ sign
}

/// Inverse of [`encode_i64`].
pub const fn decode_i64(value: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    let (magnitude, sign) = ((value >> 1) as i64, (value & 1) as i64);
    magnitude ^ sign.wrapping_neg()
}

impl Writer {
    /// Appends `value` as a zigzag-mapped canonical varint (1..=5 bytes).
    pub fn write_varint_i32(&mut self, value: i32) {
        self.write_varint_u32(encode_i32(value));
    }

    /// Appends `value` as a zigzag-mapped canonical varint (1..=10 bytes).
    pub fn write_varint_i64(&mut self, value: i64) {
        self.write_varint_u64(encode_i64(value));
    }
}

impl Reader<'_> {
    /// Reads a zigzag-mapped canonical varint into an `i32`.
    ///
    /// # Errors
    /// As [`Reader::read_varint_u32`]; the position is unchanged.
    pub fn read_varint_i32(&mut self) -> Result<i32> {
        self.read_varint_u32().map(decode_i32)
    }

    /// Reads a zigzag-mapped canonical varint into an `i64`.
    ///
    /// # Errors
    /// As [`Reader::read_varint_u64`]; the position is unchanged.
    pub fn read_varint_i64(&mut self) -> Result<i64> {
        self.read_varint_u64().map(decode_i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden mapping (SPEC §4.1): small magnitudes interleave, the extremes land on the top values.
    #[test]
    fn golden_values() {
        for (signed, unsigned) in [
            (0, 0),
            (-1, 1),
            (1, 2),
            (-2, 3),
            (2, 4),
            (-64, 127),
            (64, 128),
            (i32::MAX, u32::MAX - 1),
            (i32::MIN, u32::MAX),
        ] {
            assert_eq!(encode_i32(signed), unsigned, "{signed}");
            assert_eq!(decode_i32(unsigned), signed, "{unsigned}");
            assert_eq!(
                encode_i64(i64::from(signed)),
                u64::from(unsigned),
                "{signed}"
            );
        }
        assert_eq!(encode_i64(i64::MAX), u64::MAX - 1);
        assert_eq!(encode_i64(i64::MIN), u64::MAX);
        assert_eq!(decode_i64(u64::MAX), i64::MIN);
    }

    #[test]
    fn signed_varint_bytes() {
        let mut w = Writer::new();
        w.write_varint_i32(-1);
        w.write_varint_i64(64);
        w.write_varint_i64(i64::MIN);
        let bytes = w.into_inner();
        let mut expected = vec![0x01, 0x80, 0x01];
        expected.extend([0xff; 9]);
        expected.push(0x01);
        assert_eq!(bytes, expected);

        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_varint_i32(), Ok(-1));
        assert_eq!(r.read_varint_i64(), Ok(64));
        assert_eq!(r.read_varint_i64(), Ok(i64::MIN));
        assert!(r.is_empty());
    }

    #[test]
    fn signed_varint_errors_are_the_unsigned_ones() {
        use crate::{Corruption, Error};
        let mut r = Reader::with_base_offset(&[0xff, 0xff, 0xff, 0xff, 0x1f], 9);
        assert_eq!(
            r.read_varint_i32(),
            Err(Error::Corrupt {
                offset: 9,
                reason: Corruption::VarintOverflow { bits: 32 },
            })
        );
        assert_eq!(r.position(), 0);
    }
}
