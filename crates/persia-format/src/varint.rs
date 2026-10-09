//! Unsigned LEB128 varints (`u32`, `u64`), accepting only the canonical encoding.
//!
//! Each byte carries 7 value bits, least significant group first; the high bit means "more bytes follow".
//! Decoding is strict so that every value has exactly one valid encoding (stable checksums, golden files):
//! - an encoding longer than necessary (e.g. `80 00` for 0) is [`Corruption::OverlongVarint`];
//! - a value wider than the target type, or more bytes than it can need (5 for `u32`, 10 for `u64`), is
//!   [`Corruption::VarintOverflow`];
//! - input ending inside a varint is [`Corruption::UnexpectedEof`].
//!
//! Errors point at the first byte of the varint and leave the reader where it was.

use crate::error::{Corruption, Error, Result, to_u64};
use crate::reader::Reader;
use crate::writer::Writer;

/// Bytes needed to encode `value` (1..=10).
pub fn encoded_len(value: u64) -> usize {
    let bits = value.bit_width();
    // ceil(bits / 7), at least 1; at most 10 for 64 bits.
    usize::try_from(bits.div_ceil(7).max(1)).unwrap_or(10)
}

impl Writer {
    /// Appends `value` as a canonical LEB128 varint (1..=5 bytes).
    pub fn write_varint_u32(&mut self, value: u32) {
        self.write_varint_u64(u64::from(value));
    }

    /// Appends `value` as a canonical LEB128 varint (1..=10 bytes).
    pub fn write_varint_u64(&mut self, mut value: u64) {
        loop {
            let [low, ..] = value.to_le_bytes();
            let group = low & 0x7f;
            value >>= 7;
            if value == 0 {
                self.write_u8(group);
                return;
            }
            self.write_u8(group | 0x80);
        }
    }
}

impl Reader<'_> {
    /// Reads a canonical LEB128 varint that must fit in a `u32`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] with [`Corruption::UnexpectedEof`], [`Corruption::OverlongVarint`] or
    /// [`Corruption::VarintOverflow`]; the position is unchanged.
    pub fn read_varint_u32(&mut self) -> Result<u32> {
        let value = self.read_varint(u32::BITS)?;
        // `read_varint` already bounded the value to 32 bits.
        Ok(u32::try_from(value).unwrap_or(u32::MAX))
    }

    /// Reads a canonical LEB128 varint.
    ///
    /// # Errors
    /// [`Error::Corrupt`] with [`Corruption::UnexpectedEof`], [`Corruption::OverlongVarint`] or
    /// [`Corruption::VarintOverflow`]; the position is unchanged.
    pub fn read_varint_u64(&mut self) -> Result<u64> {
        self.read_varint(u64::BITS)
    }

    /// Decodes a varint of at most `bits` value bits (32 or 64) without advancing until it is valid.
    fn read_varint(&mut self, bits: u32) -> Result<u64> {
        let start = self.offset();
        let corrupt = |reason| Error::Corrupt {
            offset: start,
            reason,
        };
        let max_len = bits.div_ceil(7); // 5 for u32, 10 for u64
        let mut value = 0_u64;
        let mut shift = 0_u32;
        for (len, &byte) in (1_u32..=max_len).zip(self.rest()) {
            let group = u64::from(byte & 0x7f);
            if len == max_len {
                // The last possible byte must end the varint and hold only the bits that are left
                // (4 for u32, 1 for u64).
                let bits_left = bits - shift;
                if byte & 0x80 != 0 || group >> bits_left != 0 {
                    return Err(corrupt(Corruption::VarintOverflow { bits }));
                }
            }
            value |= group << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && len > 1 {
                    return Err(corrupt(Corruption::OverlongVarint));
                }
                self.skip(usize::try_from(len).unwrap_or(usize::MAX))?;
                return Ok(value);
            }
            shift += 7;
        }
        // Every available byte had the continuation bit set (or there were none).
        let available = to_u64(self.remaining());
        Err(corrupt(Corruption::UnexpectedEof {
            needed: available.saturating_add(1),
            available,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(value: u64) -> Vec<u8> {
        let mut w = Writer::new();
        w.write_varint_u64(value);
        w.into_inner()
    }

    fn corrupt(offset: u64, reason: Corruption) -> Error {
        Error::Corrupt { offset, reason }
    }

    /// Golden vectors: the canonical bytes for boundary values (pinned format, SPEC §4.1 little-endian groups).
    const GOLDEN: &[(u64, &[u8])] = &[
        (0, &[0x00]),
        (1, &[0x01]),
        (127, &[0x7f]),
        (128, &[0x80, 0x01]),
        (300, &[0xac, 0x02]),
        (16_383, &[0xff, 0x7f]),
        (16_384, &[0x80, 0x80, 0x01]),
        (0x0fff_ffff, &[0xff, 0xff, 0xff, 0x7f]),
        (u32::MAX as u64, &[0xff, 0xff, 0xff, 0xff, 0x0f]),
        (1 << 35, &[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]),
        (
            u64::MAX >> 1,
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f],
        ),
        (
            u64::MAX,
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
        ),
    ];

    #[test]
    fn golden_vectors_encode_and_decode() {
        for &(value, bytes) in GOLDEN {
            assert_eq!(encode(value), bytes, "encode {value}");
            assert_eq!(encoded_len(value), bytes.len(), "len {value}");
            let mut r = Reader::new(bytes);
            assert_eq!(r.read_varint_u64(), Ok(value), "decode {value}");
            assert!(r.is_empty());
            if let Ok(small) = u32::try_from(value) {
                let mut w = Writer::new();
                w.write_varint_u32(small);
                assert_eq!(w.as_slice(), bytes);
                assert_eq!(Reader::new(bytes).read_varint_u32(), Ok(small));
            }
        }
    }

    #[test]
    fn overlong_encodings_are_rejected() {
        for bytes in [
            &[0x80, 0x00][..],
            &[0xff, 0x80, 0x00],
            &[0x81, 0x80, 0x80, 0x00],
            &[0x80, 0x80, 0x80, 0x80, 0x00],
            &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00],
        ] {
            let mut r = Reader::with_base_offset(bytes, 40);
            assert_eq!(
                r.read_varint_u64(),
                Err(corrupt(40, Corruption::OverlongVarint)),
                "{bytes:02x?}"
            );
            assert_eq!(r.position(), 0);
        }
        assert_eq!(
            Reader::new(&[0x80, 0x00]).read_varint_u32(),
            Err(corrupt(0, Corruption::OverlongVarint))
        );
    }

    #[test]
    fn values_wider_than_the_type_are_rejected() {
        let overflow32 = corrupt(0, Corruption::VarintOverflow { bits: 32 });
        let overflow64 = corrupt(0, Corruption::VarintOverflow { bits: 64 });
        // u32: the 5th byte may only hold 4 bits, and must end the varint.
        assert_eq!(
            Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x10]).read_varint_u32(),
            Err(overflow32.clone())
        );
        assert_eq!(
            Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x8f, 0x00]).read_varint_u32(),
            Err(overflow32.clone())
        );
        // A valid u64 above u32::MAX is still an overflow for u32.
        assert_eq!(
            Reader::new(&encode(1 << 32)).read_varint_u32(),
            Err(overflow32.clone())
        );
        // u64: the 10th byte may only be 0x01 (0x00 there would be overlong).
        let mut nine_ff = vec![0xff; 9];
        nine_ff.push(0x02);
        assert_eq!(
            Reader::new(&nine_ff).read_varint_u64(),
            Err(overflow64.clone())
        );
        assert_eq!(
            Reader::new(&[0xff; 11]).read_varint_u64(),
            Err(overflow64.clone())
        );
    }

    #[test]
    fn truncated_varints_are_eof_at_their_start() {
        let mut r = Reader::with_base_offset(&[0x07, 0x80, 0xff], 10);
        assert_eq!(r.read_varint_u64(), Ok(7));
        let eof = corrupt(
            11,
            Corruption::UnexpectedEof {
                needed: 3,
                available: 2,
            },
        );
        assert_eq!(r.read_varint_u64(), Err(eof));
        assert_eq!(r.position(), 1);
        let empty = corrupt(
            0,
            Corruption::UnexpectedEof {
                needed: 1,
                available: 0,
            },
        );
        assert_eq!(Reader::new(&[]).read_varint_u32(), Err(empty));
    }

    #[test]
    fn decoding_stops_at_the_last_byte() {
        let mut r = Reader::new(&[0xac, 0x02, 0x05, 0xff]);
        assert_eq!(r.read_varint_u32(), Ok(300));
        assert_eq!(r.read_varint_u32(), Ok(5));
        assert_eq!(r.rest(), &[0xff]);
    }

    #[test]
    fn encoded_len_matches_every_length_boundary() {
        for n in 1..=9_u32 {
            let first_of_len = 1_u64 << (7 * n);
            assert_eq!(encoded_len(first_of_len - 1), n as usize);
            assert_eq!(encoded_len(first_of_len), n as usize + 1);
            assert_eq!(encode(first_of_len).len(), n as usize + 1);
        }
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            corrupt(3, Corruption::OverlongVarint).to_string(),
            "corrupt data at offset 3: overlong varint (non-canonical encoding)"
        );
        assert_eq!(
            corrupt(3, Corruption::VarintOverflow { bits: 32 }).to_string(),
            "corrupt data at offset 3: varint does not fit in 32 bits"
        );
    }
}
