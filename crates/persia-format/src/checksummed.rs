//! Fixed-size header structures protected by a CRC32C field (SPEC §4.2 header, §4.3 superblock slot).
//!
//! Both layouts follow one rule: the checksum covers every byte of the region except its own 4-byte field
//! (the header's `header_crc` at 12 covers `0..12 + 16..4096`; a slot's `crc` at 2044 covers `0..2044`).
//! A type describes its layout with [`ChecksummedLayout`]; [`Checksummed`] does the checksum, so no
//! structure can forget to verify it, and corrupt bytes are rejected before any field is interpreted.

use crate::checksum::{Crc32c, check};
use crate::error::{EncodeError, Result};
use crate::reader::Reader;
use crate::writer::Writer;

/// Width of the CRC32C field (`u32`, little-endian).
const CRC_LEN: usize = 4;

/// The layout of a fixed-size structure whose integrity is a CRC32C field inside it.
///
/// `CRC_OFFSET + 4 <= LEN` is checked at compile time when [`Checksummed`] is used with the type.
///
/// ```compile_fail
/// use persia_format::checksummed::{Checksummed, ChecksummedLayout};
/// use persia_format::{Reader, Result, Writer};
///
/// struct Bad;
/// impl ChecksummedLayout for Bad {
///     const LEN: usize = 8;
///     const CRC_OFFSET: usize = 6; // field would end at 10 > LEN
///     fn encode_fields(&self, w: &mut Writer) { w.write_zeros(8); }
///     fn decode_fields(_: &mut Reader<'_>) -> Result<Self> { Ok(Bad) }
/// }
/// let _ = Checksummed(Bad).encode(&mut Writer::new());
/// ```
pub trait ChecksummedLayout: Sized {
    /// Total size of the region in bytes, CRC field included.
    const LEN: usize;
    /// Position of the `u32` CRC32C field within the region.
    const CRC_OFFSET: usize;

    /// Writes all `LEN` bytes of the region, with 4 zero bytes (`write_u32(0)`) as a placeholder for the CRC
    /// field; [`Checksummed::encode`] fills it in. Writing any other length is a bug, reported as
    /// [`EncodeError::LayoutLen`].
    fn encode_fields(&self, w: &mut Writer);

    /// Decodes the fields from a reader over exactly the `LEN`-byte region, whose checksum has already been
    /// verified. Must step over the CRC field (e.g. `r.skip(4)`); reserved bytes should be checked with
    /// [`Reader::expect_zeros`].
    ///
    /// # Errors
    /// [`crate::Error::Corrupt`] for field values the format forbids.
    fn decode_fields(r: &mut Reader<'_>) -> Result<Self>;
}

/// A [`ChecksummedLayout`] value, encoded with and decoded through its CRC32C.
///
/// ```
/// use persia_format::checksummed::{Checksummed, ChecksummedLayout};
/// use persia_format::{Corruption, Error, Reader, Result, Writer};
///
/// /// 16 bytes: `seq u64`, 4 reserved, `crc u32` (a miniature superblock slot).
/// #[derive(Debug, PartialEq)]
/// struct Slot { seq: u64 }
///
/// impl ChecksummedLayout for Slot {
///     const LEN: usize = 16;
///     const CRC_OFFSET: usize = 12;
///     fn encode_fields(&self, w: &mut Writer) {
///         w.write_u64(self.seq);
///         w.write_zeros(4);
///         w.write_u32(0); // CRC placeholder
///     }
///     fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
///         let seq = r.read_u64()?;
///         r.expect_zeros(4)?;
///         r.skip(4)?; // CRC
///         Ok(Slot { seq })
///     }
/// }
///
/// let mut w = Writer::new();
/// Checksummed(Slot { seq: 7 }).encode(&mut w)?;
/// let mut bytes = w.into_inner();
/// assert_eq!(Checksummed::<Slot>::decode(&mut Reader::new(&bytes))?.0, Slot { seq: 7 });
///
/// bytes[0] ^= 1; // a single flipped bit is caught before `seq` is read
/// let err = Checksummed::<Slot>::decode(&mut Reader::new(&bytes)).unwrap_err();
/// assert!(matches!(err, Error::Corrupt { offset: 12, reason: Corruption::ChecksumMismatch { .. } }));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checksummed<T>(pub T);

impl<T: ChecksummedLayout> Checksummed<T> {
    /// End of the CRC field. Evaluating it (both `encode` and `decode` do) rejects a field outside the
    /// region at compile time.
    const CRC_END: usize = {
        let end = T::CRC_OFFSET.wrapping_add(CRC_LEN);
        assert!(
            end >= CRC_LEN && end <= T::LEN,
            "ChecksummedLayout: the CRC field must lie inside the region"
        );
        end
    };
    /// Bytes after the CRC field (never wraps: `CRC_END <= LEN`).
    const TAIL_LEN: usize = T::LEN.wrapping_sub(Self::CRC_END);

    /// Appends the `LEN`-byte region with its CRC32C filled in.
    ///
    /// # Errors
    /// [`EncodeError::LayoutLen`] if [`ChecksummedLayout::encode_fields`] wrote a length other than `LEN`;
    /// nothing is appended then.
    pub fn encode(&self, w: &mut Writer) -> Result<(), EncodeError> {
        // Same base, so alignment computed inside `encode_fields` matches the final position.
        let mut region = Writer::with_base_offset(w.offset());
        self.0.encode_fields(&mut region);
        let mut bytes = region.into_inner();
        let layout_len = EncodeError::LayoutLen {
            expected: T::LEN,
            actual: bytes.len(),
        };
        let (head, rest) = bytes
            .split_at_mut_checked(T::CRC_OFFSET)
            .ok_or(layout_len)?;
        let (slot, tail) = rest.split_at_mut_checked(CRC_LEN).ok_or(layout_len)?;
        if tail.len() != Self::TAIL_LEN {
            return Err(layout_len);
        }
        let mut crc = Crc32c::new();
        crc.update(head);
        crc.update(tail);
        slot.copy_from_slice(&crc.finish().to_le_bytes());
        w.write_bytes(&bytes);
        Ok(())
    }

    /// Reads the next `LEN` bytes, verifies their CRC32C, then decodes the fields.
    ///
    /// # Errors
    /// [`crate::Error::Corrupt`] if fewer than `LEN` bytes remain, with [`crate::Corruption::ChecksumMismatch`]
    /// at the CRC field's offset if the checksum is wrong, or whatever
    /// [`ChecksummedLayout::decode_fields`] reports. On error the position is unchanged.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let mut ahead = r.clone();
        let mut region = ahead.sub_reader(T::LEN)?;
        let mut fields = region.clone();
        // `region` holds exactly LEN bytes and CRC_END <= LEN, so these reads cannot fail.
        let head = region.read_bytes(T::CRC_OFFSET)?;
        let crc_offset = region.offset();
        let stored = region.read_u32()?;
        let tail = region.read_bytes(Self::TAIL_LEN)?;
        let mut crc = Crc32c::new();
        crc.update(head);
        crc.update(tail);
        check(crc.finish(), stored, crc_offset)?;

        let value = T::decode_fields(&mut fields)?;
        *r = ahead;
        Ok(Self(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Corruption, Error};

    /// CRC in the middle, like the container header (SPEC §4.2): `magic u32, crc u32, a u64, b u64`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Mid {
        magic: u32,
        a: u64,
        b: u64,
    }

    impl ChecksummedLayout for Mid {
        const LEN: usize = 24;
        const CRC_OFFSET: usize = 4;
        fn encode_fields(&self, w: &mut Writer) {
            w.write_u32(self.magic);
            w.write_u32(0);
            w.write_u64(self.a);
            w.write_u64(self.b);
        }
        fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
            let magic = r.read_u32()?;
            r.skip(4)?;
            Ok(Self {
                magic,
                a: r.read_u64()?,
                b: r.read_u64()?,
            })
        }
    }

    /// CRC last, like a superblock slot (SPEC §4.3): `seq u64, 4 reserved, crc u32`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Tail {
        seq: u64,
    }

    impl ChecksummedLayout for Tail {
        const LEN: usize = 16;
        const CRC_OFFSET: usize = 12;
        fn encode_fields(&self, w: &mut Writer) {
            w.write_u64(self.seq);
            w.write_zeros(4);
            w.write_u32(0);
        }
        fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
            let seq = r.read_u64()?;
            r.expect_zeros(4)?;
            r.skip(4)?;
            Ok(Self { seq })
        }
    }

    /// Writes one byte too few.
    struct Short;

    impl ChecksummedLayout for Short {
        const LEN: usize = 8;
        const CRC_OFFSET: usize = 4;
        fn encode_fields(&self, w: &mut Writer) {
            w.write_zeros(7);
        }
        fn decode_fields(_: &mut Reader<'_>) -> Result<Self> {
            Ok(Self)
        }
    }

    const MID: Mid = Mid {
        magic: 0x5045_5253,
        a: 0x0102_0304_0506_0708,
        b: u64::MAX,
    };

    fn encoded<T: ChecksummedLayout>(value: T) -> Vec<u8> {
        let mut w = Writer::new();
        assert_eq!(Checksummed(value).encode(&mut w), Ok(()));
        w.into_inner()
    }

    fn mismatch_at(offset: u64) -> impl Fn(&Error) -> bool {
        move |e| matches!(e, Error::Corrupt { offset: o, reason: Corruption::ChecksumMismatch { .. } } if *o == offset)
    }

    #[test]
    fn crc_excludes_its_own_field() {
        let bytes = encoded(MID);
        let mut covered = bytes[..4].to_vec();
        covered.extend_from_slice(&bytes[8..]);
        assert_eq!(bytes[4..8], crate::checksum::crc32c(&covered).to_le_bytes());

        let bytes = encoded(Tail { seq: 9 });
        assert_eq!(
            bytes[12..],
            crate::checksum::crc32c(&bytes[..12]).to_le_bytes()
        );
    }

    #[test]
    fn round_trip_advances_exactly_len() {
        let mut bytes = encoded(MID);
        bytes.extend(encoded(Tail { seq: 3 }));
        let mut r = Reader::new(&bytes);
        assert_eq!(Checksummed::<Mid>::decode(&mut r), Ok(Checksummed(MID)));
        assert_eq!(r.position(), Mid::LEN);
        assert_eq!(
            Checksummed::<Tail>::decode(&mut r),
            Ok(Checksummed(Tail { seq: 3 }))
        );
        assert!(r.is_empty());
    }

    #[test]
    fn every_single_bit_flip_is_detected() {
        for bytes in [encoded(MID), encoded(Tail { seq: u64::MAX })] {
            let crc_offset = if bytes.len() == Mid::LEN { 4 } else { 12 };
            for bit in 0..bytes.len() * 8 {
                let mut flipped = bytes.clone();
                flipped[bit / 8] ^= 1 << (bit % 8);
                let mut r = Reader::new(&flipped);
                let err = if crc_offset == 4 {
                    Checksummed::<Mid>::decode(&mut r).map(|_| ())
                } else {
                    Checksummed::<Tail>::decode(&mut r).map(|_| ())
                };
                assert!(
                    err.as_ref().is_err_and(mismatch_at(crc_offset)),
                    "bit {bit}: {err:?}"
                );
                assert_eq!(r.position(), 0, "bit {bit}: failed decode moved the cursor");
            }
        }
    }

    #[test]
    fn mismatch_offset_is_absolute() {
        let mut bytes = encoded(MID);
        bytes[20] ^= 0x80;
        let mut r = Reader::with_base_offset(&bytes, 4096);
        assert!(
            Checksummed::<Mid>::decode(&mut r)
                .as_ref()
                .is_err_and(mismatch_at(4100))
        );
    }

    #[test]
    fn truncated_region_is_eof_and_moves_nothing() {
        let bytes = encoded(MID);
        let mut r = Reader::new(&bytes[..Mid::LEN - 1]);
        assert_eq!(
            Checksummed::<Mid>::decode(&mut r),
            Err(Error::Corrupt {
                offset: 0,
                reason: Corruption::UnexpectedEof {
                    needed: 24,
                    available: 23,
                },
            })
        );
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn field_errors_surface_after_a_valid_crc() {
        // Non-zero reserved bytes with a CRC that matches them: the checksum passes, the field check fails.
        let mut bytes = encoded(Tail { seq: 1 });
        bytes[9] = 0xab;
        let crc = crate::checksum::crc32c(&bytes[..12]);
        bytes[12..].copy_from_slice(&crc.to_le_bytes());
        let mut r = Reader::new(&bytes);
        assert_eq!(
            Checksummed::<Tail>::decode(&mut r),
            Err(Error::Corrupt {
                offset: 9,
                reason: Corruption::NonZeroReserved { value: 0xab },
            })
        );
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn wrong_layout_length_appends_nothing() {
        let mut w = Writer::new();
        w.write_u8(1);
        assert_eq!(
            Checksummed(Short).encode(&mut w),
            Err(EncodeError::LayoutLen {
                expected: 8,
                actual: 7,
            })
        );
        assert_eq!(w.as_slice(), [1]);
    }

    #[test]
    fn encode_ignores_placeholder_value() {
        struct Dirty;
        impl ChecksummedLayout for Dirty {
            const LEN: usize = 8;
            const CRC_OFFSET: usize = 0;
            fn encode_fields(&self, w: &mut Writer) {
                w.write_u32(0xdead_beef);
                w.write_u32(5);
            }
            fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
                r.skip(8)?;
                Ok(Self)
            }
        }
        let mut w = Writer::new();
        assert_eq!(Checksummed(Dirty).encode(&mut w), Ok(()));
        assert_eq!(
            w.as_slice()[..4],
            crate::checksum::crc32c(&5u32.to_le_bytes()).to_le_bytes()
        );
        assert!(Checksummed::<Dirty>::decode(&mut Reader::new(w.as_slice())).is_ok());
    }
}
