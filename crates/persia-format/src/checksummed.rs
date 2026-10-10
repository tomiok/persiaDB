//! Fixed-size header structures protected by a CRC32C field (SPEC §4.2 header, §4.3 superblock slot).
//!
//! Both layouts follow one rule: the checksum covers every byte of the region except its own 4-byte field
//! (the header's `header_crc` at 12 covers `0..12 + 16..4096`; a slot's `crc` at 2044 covers `0..2044`).
//! A type describes its layout with [`ChecksummedLayout`]; [`Checksummed`] does the checksum, so no
//! structure can forget to verify it, and corrupt bytes are rejected before any field is interpreted.
//!
//! Because the checksum comes first, a region that is not of this format at all (wrong magic, a future
//! `format_major` with a different layout) is reported as a checksum mismatch. Callers that must tell those
//! apart (SPEC §4.2: "reader MUST reject unknown major") peek at the raw prefix with a cloned [`Reader`]
//! before calling [`Checksummed::decode`].
//!
//! Frames (SPEC §4.4) are variable-length and their CRC follows the payload, so they do not fit this
//! helper; they use the streaming [`Crc32c`] directly.

use crate::checksum::{Crc32c, check};
use crate::error::{EncodeError, Result};
use crate::reader::Reader;
use crate::writer::Writer;

/// Width of the CRC32C field (`u32`, little-endian).
const CRC_LEN: usize = 4;

/// The layout of a fixed-size structure whose integrity is a CRC32C field inside it.
///
/// `CRC_OFFSET + 4 <= LEN` is checked during const evaluation, so a crate using [`Checksummed`] with a bad
/// layout fails to build (`cargo build`/`cargo test`; `cargo check` does not evaluate it).
///
/// ```compile_fail,E0080
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
    use core::cell::Cell;

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

    /// Records whether `decode_fields` ran and where `encode_fields` started, so tests can observe the
    /// order of operations without reaching into `Checksummed` (thread-local: tests run in parallel).
    mod probe {
        use core::cell::Cell;
        thread_local! {
            pub static DECODED: Cell<usize> = const { Cell::new(0) };
            pub static ENCODE_OFFSET: Cell<u64> = const { Cell::new(u64::MAX) };
        }
    }

    /// 8 bytes, CRC last; counts `decode_fields` calls and records the writer offset seen by `encode_fields`.
    #[derive(Debug, PartialEq)]
    struct Probe;

    impl ChecksummedLayout for Probe {
        const LEN: usize = 8;
        const CRC_OFFSET: usize = 4;
        fn encode_fields(&self, w: &mut Writer) {
            probe::ENCODE_OFFSET.with(|c| c.set(w.offset()));
            w.write_u32(0x1234_5678);
            w.write_u32(0);
        }
        fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
            probe::DECODED.with(|c| c.set(c.get() + 1));
            r.skip(8)?;
            Ok(Self)
        }
    }

    /// The smallest legal layout: the region is just the CRC field (`CRC_OFFSET = 0`, `LEN = 4`).
    #[derive(Debug, PartialEq)]
    struct CrcOnly;

    impl ChecksummedLayout for CrcOnly {
        const LEN: usize = 4;
        const CRC_OFFSET: usize = 0;
        fn encode_fields(&self, w: &mut Writer) {
            w.write_u32(0);
        }
        fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
            r.skip(4)?;
            Ok(Self)
        }
    }

    /// CRC first, like `Dirty` but with real payload after it: `crc u32, x u32`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Head {
        x: u32,
    }

    impl ChecksummedLayout for Head {
        const LEN: usize = 8;
        const CRC_OFFSET: usize = 0;
        fn encode_fields(&self, w: &mut Writer) {
            w.write_u32(0);
            w.write_u32(self.x);
        }
        fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
            r.skip(4)?;
            Ok(Self { x: r.read_u32()? })
        }
    }

    /// `tag u8`, zero padding to the next 8-byte boundary (absolute), `crc u32`, `v u32`: 16 bytes only when
    /// the region starts at a multiple of 8.
    struct Aligned;

    impl ChecksummedLayout for Aligned {
        const LEN: usize = 16;
        const CRC_OFFSET: usize = 8;
        fn encode_fields(&self, w: &mut Writer) {
            w.write_u8(0xa5);
            w.pad_to(core::num::NonZeroUsize::MIN.saturating_add(7));
            w.write_u32(0);
            w.write_u32(9);
        }
        fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
            r.skip(16)?;
            Ok(Self)
        }
    }

    /// Asserts every single-bit flip of `bytes` (a valid `T` region) is a mismatch at the CRC field with the
    /// exact stored and computed values, and that the cursor does not move.
    fn assert_flips_detected<T: ChecksummedLayout + core::fmt::Debug>(bytes: &[u8]) {
        assert_eq!(bytes.len(), T::LEN);
        let crc_end = T::CRC_OFFSET + 4;
        for bit in 0..bytes.len() * 8 {
            let mut flipped = bytes.to_vec();
            flipped[bit / 8] ^= 1 << (bit % 8);
            let stored = u32::from_le_bytes(flipped[T::CRC_OFFSET..crc_end].try_into().unwrap());
            let mut covered = flipped[..T::CRC_OFFSET].to_vec();
            covered.extend_from_slice(&flipped[crc_end..]);
            let computed = crate::checksum::crc32c(&covered);
            let mut r = Reader::new(&flipped);
            assert_eq!(
                Checksummed::<T>::decode(&mut r).map(|_| ()),
                Err(Error::Corrupt {
                    offset: T::CRC_OFFSET as u64,
                    reason: Corruption::ChecksumMismatch { stored, computed },
                }),
                "bit {bit}"
            );
            assert_eq!(r.position(), 0, "bit {bit}");
        }
    }

    #[test]
    fn mismatch_reports_exact_stored_and_computed() {
        assert_flips_detected::<Mid>(&encoded(MID));
        assert_flips_detected::<Tail>(&encoded(Tail { seq: 0 }));
        assert_flips_detected::<Head>(&encoded(Head { x: 0xffff_ffff }));
        assert_flips_detected::<CrcOnly>(&encoded(CrcOnly));
    }

    #[test]
    fn crc_only_region_checksums_nothing() {
        // CRC32C of zero bytes is 0, so the only valid 4-byte region is all zeros.
        assert_eq!(encoded(CrcOnly), [0; 4]);
        let mut r = Reader::new(&[0, 0, 0, 0, 0xff]);
        assert_eq!(
            Checksummed::<CrcOnly>::decode(&mut r),
            Ok(Checksummed(CrcOnly))
        );
        assert_eq!(r.position(), 4);
    }

    #[test]
    fn crc_first_layout_covers_the_rest() {
        let bytes = encoded(Head { x: 0xdead_beef });
        assert_eq!(bytes[4..], 0xdead_beef_u32.to_le_bytes());
        assert_eq!(
            bytes[..4],
            crate::checksum::crc32c(&bytes[4..]).to_le_bytes()
        );
        assert_eq!(
            Checksummed::<Head>::decode(&mut Reader::new(&bytes)),
            Ok(Checksummed(Head { x: 0xdead_beef }))
        );
    }

    #[test]
    fn corrupt_bytes_never_reach_decode_fields() {
        let bytes = encoded(Probe);
        probe::DECODED.with(|c| c.set(0));
        for bit in 0..bytes.len() * 8 {
            let mut flipped = bytes.clone();
            flipped[bit / 8] ^= 1 << (bit % 8);
            assert!(Checksummed::<Probe>::decode(&mut Reader::new(&flipped)).is_err());
        }
        // Truncated input is rejected before the fields too.
        assert!(Checksummed::<Probe>::decode(&mut Reader::new(&bytes[..7])).is_err());
        assert_eq!(probe::DECODED.with(Cell::get), 0);
        assert_eq!(
            Checksummed::<Probe>::decode(&mut Reader::new(&bytes)),
            Ok(Checksummed(Probe))
        );
        assert_eq!(probe::DECODED.with(Cell::get), 1);
    }

    #[test]
    fn encode_fields_sees_the_absolute_offset() {
        let mut w = Writer::with_base_offset(4096);
        w.write_bytes(&[7; 5]);
        assert_eq!(Checksummed(Probe).encode(&mut w), Ok(()));
        assert_eq!(probe::ENCODE_OFFSET.with(Cell::get), 4101);
        // The region is appended after the existing bytes, CRC over the region only.
        let out = w.into_inner();
        assert_eq!(out[..5], [7; 5]);
        assert_eq!(out[5..9], 0x1234_5678_u32.to_le_bytes());
        assert_eq!(out[9..], crate::checksum::crc32c(&out[5..9]).to_le_bytes());
    }

    #[test]
    fn alignment_inside_encode_fields_uses_the_final_position() {
        // At an 8-aligned offset the padding is 7 bytes and the layout fits.
        let mut w = Writer::with_base_offset(4088);
        w.write_bytes(&[1; 8]);
        assert_eq!(Checksummed(Aligned).encode(&mut w), Ok(()));
        assert_eq!(w.len(), 24);
        // At offset 4099 (≡ 3 mod 8) the tag ends at 4100, so only 4 padding bytes: 13 bytes, not 16.
        let mut w = Writer::with_base_offset(4096);
        w.write_bytes(&[1; 3]);
        assert_eq!(
            Checksummed(Aligned).encode(&mut w),
            Err(EncodeError::LayoutLen {
                expected: 16,
                actual: 13,
            })
        );
        assert_eq!(w.as_slice(), [1; 3]);
    }

    #[test]
    fn too_long_or_too_short_layout_appends_nothing() {
        /// Writes `N` bytes for an 8-byte layout with the CRC at 4.
        struct Writes<const N: usize>;
        impl<const N: usize> ChecksummedLayout for Writes<N> {
            const LEN: usize = 8;
            const CRC_OFFSET: usize = 4;
            fn encode_fields(&self, w: &mut Writer) {
                w.write_zeros(N);
            }
            fn decode_fields(_: &mut Reader<'_>) -> Result<Self> {
                Ok(Self)
            }
        }
        fn check<const N: usize>() {
            let mut w = Writer::with_base_offset(64);
            w.write_u8(0xee);
            let expected = if N == 8 {
                Ok(())
            } else {
                Err(EncodeError::LayoutLen {
                    expected: 8,
                    actual: N,
                })
            };
            assert_eq!(Checksummed(Writes::<N>).encode(&mut w), expected, "N = {N}");
            if N != 8 {
                assert_eq!(w.as_slice(), [0xee], "N = {N}");
            }
        }
        // Inside the head, inside the CRC field, inside the tail, exact, one over, far over.
        check::<0>();
        check::<3>();
        check::<4>();
        check::<6>();
        check::<7>();
        check::<8>();
        check::<9>();
        check::<64>();
    }

    #[test]
    fn decode_fields_is_confined_to_the_region() {
        /// Reads one byte past its own `LEN`.
        #[derive(Debug)]
        struct Greedy;
        impl ChecksummedLayout for Greedy {
            const LEN: usize = 8;
            const CRC_OFFSET: usize = 4;
            fn encode_fields(&self, w: &mut Writer) {
                w.write_zeros(8);
            }
            fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
                r.skip(9)?;
                Ok(Self)
            }
        }
        let mut bytes = encoded(Greedy);
        bytes.extend_from_slice(&[0; 8]); // bytes the fields must not see
        let mut r = Reader::with_base_offset(&bytes, 100);
        assert_eq!(
            Checksummed::<Greedy>::decode(&mut r).map(|_| ()),
            Err(Error::Corrupt {
                offset: 100,
                reason: Corruption::UnexpectedEof {
                    needed: 9,
                    available: 8,
                },
            })
        );
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn partial_decode_fields_still_advances_exactly_len() {
        /// Decodes nothing at all.
        #[derive(Debug, PartialEq)]
        struct Lazy;
        impl ChecksummedLayout for Lazy {
            const LEN: usize = 8;
            const CRC_OFFSET: usize = 4;
            fn encode_fields(&self, w: &mut Writer) {
                w.write_u32(3);
                w.write_u32(0);
            }
            fn decode_fields(_: &mut Reader<'_>) -> Result<Self> {
                Ok(Self)
            }
        }
        let mut bytes = encoded(Lazy);
        bytes.push(0x42);
        let mut r = Reader::new(&bytes);
        assert_eq!(Checksummed::<Lazy>::decode(&mut r), Ok(Checksummed(Lazy)));
        assert_eq!(r.position(), 8);
        assert_eq!(r.rest(), [0x42]);
    }

    #[test]
    fn errors_mid_stream_keep_position_and_absolute_offsets() {
        // [5 filler bytes][Tail region], read at base 4096.
        let mut bytes = vec![0xcc; 5];
        bytes.extend(encoded(Tail { seq: 1 }));
        let base = 4096;

        // Checksum mismatch: reported at the CRC field's absolute offset, cursor stays at 5.
        let mut bad = bytes.clone();
        bad[5] ^= 1;
        let mut r = Reader::with_base_offset(&bad, base);
        r.skip(5).unwrap();
        assert!(
            Checksummed::<Tail>::decode(&mut r)
                .as_ref()
                .is_err_and(mismatch_at(base + 5 + 12))
        );
        assert_eq!(r.position(), 5);

        // Field error after a valid CRC: offset absolute too.
        let mut bad = bytes.clone();
        bad[5 + 10] = 0x01;
        let crc = crate::checksum::crc32c(&bad[5..17]);
        bad[17..].copy_from_slice(&crc.to_le_bytes());
        let mut r = Reader::with_base_offset(&bad, base);
        r.skip(5).unwrap();
        assert_eq!(
            Checksummed::<Tail>::decode(&mut r),
            Err(Error::Corrupt {
                offset: base + 5 + 10,
                reason: Corruption::NonZeroReserved { value: 1 },
            })
        );
        assert_eq!(r.position(), 5);

        // Truncation: EOF at the region's absolute start.
        let mut r = Reader::with_base_offset(&bytes[..20], base);
        r.skip(5).unwrap();
        assert_eq!(
            Checksummed::<Tail>::decode(&mut r),
            Err(Error::Corrupt {
                offset: base + 5,
                reason: Corruption::UnexpectedEof {
                    needed: 16,
                    available: 15,
                },
            })
        );
        assert_eq!(r.position(), 5);

        // And the intact stream decodes from the same place.
        let mut r = Reader::with_base_offset(&bytes, base);
        r.skip(5).unwrap();
        assert_eq!(
            Checksummed::<Tail>::decode(&mut r),
            Ok(Checksummed(Tail { seq: 1 }))
        );
        assert!(r.is_empty());
    }
}
