//! The container header: magic, format version, feature flags, database id and page size (SPEC §4.2; ADR-0007).
//!
//! The header is the first [`HEADER_LEN`] bytes of a `.persia` container, followed by the two superblock slots
//! ([`crate::superblock`]). [`Header::decode`] checks the magic and `format_major` from the raw bytes *before* the
//! CRC, because a future major may move every other field, `header_crc` included: such a file is
//! [`Error::Unsupported`], not corrupt. Unknown `incompat_flags` bits are unsupported too; unknown
//! `rocompat_flags` bits only make [`Header::access`] say [`Access::ReadOnly`]. `page_size` is advisory and no offset
//! depends on it (ADR-0008).

use crate::checksummed::{Checksummed, ChecksummedLayout};
use crate::error::{Corruption, EncodeError, Error, Result, Unsupported};
use crate::reader::Reader;
use crate::writer::Writer;

/// The first 8 bytes of every container.
pub const MAGIC: [u8; 8] = *b"PERSIA\0\x01";
/// The only `format_major` this build reads and writes.
pub const FORMAT_MAJOR: u16 = 1;
/// The `format_minor` this build writes; any minor of [`FORMAT_MAJOR`] is read.
pub const FORMAT_MINOR: u16 = 0;
/// Size of the header region covered by `header_crc`; the superblock slots start here.
pub const HEADER_LEN: usize = 4096;
/// Page size of a new container.
pub const DEFAULT_PAGE_SIZE: u32 = 4096;
/// Smallest valid `page_size`.
pub const MIN_PAGE_SIZE: u32 = 4096;
/// Largest valid `page_size`.
pub const MAX_PAGE_SIZE: u32 = 64 * 1024;
/// `incompat_flags` bits this build knows (none are assigned yet).
pub const KNOWN_INCOMPAT_FLAGS: u64 = 0;
/// `rocompat_flags` bits this build knows (none are assigned yet).
pub const KNOWN_ROCOMPAT_FLAGS: u64 = 0;

// SPEC §4.2 field offsets within the header.
const MINOR_OFFSET: usize = 10;
const CRC_OFFSET: usize = 12;
/// Bytes 52..4096.
const RESERVED_LEN: usize = HEADER_LEN - 52;

/// How a container with this header may be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Every feature in use is known: the container can be written.
    ReadWrite,
    /// Some `rocompat_flags` bits are unknown: reading is safe, writing is not (SPEC §4.2).
    ReadOnly,
}

/// The decoded container header (SPEC §4.2). `format_major` is not stored: a decoded header always has
/// [`FORMAT_MAJOR`], and encoding writes it.
///
/// ```
/// use persia_format::header::{Access, HEADER_LEN, Header};
/// use persia_format::{Reader, Writer};
///
/// let header = Header::new([7; 16]);
/// let mut w = Writer::new();
/// header.encode(&mut w)?;
/// assert_eq!(w.len(), HEADER_LEN);
///
/// let decoded = Header::decode(&mut Reader::new(w.as_slice()))?;
/// assert_eq!(decoded, header);
/// assert_eq!(decoded.access(), Access::ReadWrite);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Additive revision within [`FORMAT_MAJOR`].
    pub format_minor: u16,
    /// Features a reader must understand to read the container at all.
    pub incompat_flags: u64,
    /// Features a reader must understand to write the container.
    pub rocompat_flags: u64,
    /// Identifies the database across copies and replicas.
    pub db_uuid: [u8; 16],
    /// A power of two in [`MIN_PAGE_SIZE`]`..=`[`MAX_PAGE_SIZE`]. Advisory: the alignment a writer may give Segment
    /// frame payloads (ADR-0008); no offset depends on it.
    pub page_size: u32,
}

impl Header {
    /// The header of a new container written by this build.
    pub const fn new(db_uuid: [u8; 16]) -> Self {
        Self {
            format_minor: FORMAT_MINOR,
            incompat_flags: 0,
            rocompat_flags: 0,
            db_uuid,
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// The header this build writes back to a container it opened with this header: `format_minor` becomes
    /// `max(format_minor, FORMAT_MINOR)`, so it never decreases (SPEC §4.2; ADR-0008). Everything else is kept.
    ///
    /// ```
    /// use persia_format::header::{FORMAT_MINOR, Header};
    ///
    /// let newer = Header { format_minor: FORMAT_MINOR + 3, ..Header::new([0; 16]) };
    /// assert_eq!(newer.for_write(), newer);
    /// ```
    #[must_use]
    // While FORMAT_MINOR is 0 the `max` is a no-op; it is the rule for every later minor.
    #[allow(clippy::unnecessary_min_or_max)]
    pub fn for_write(&self) -> Self {
        Self {
            format_minor: self.format_minor.max(FORMAT_MINOR),
            ..*self
        }
    }

    /// Whether the container may be written, judging by its `rocompat_flags`.
    pub const fn access(&self) -> Access {
        if self.rocompat_flags & !KNOWN_ROCOMPAT_FLAGS == 0 {
            Access::ReadWrite
        } else {
            Access::ReadOnly
        }
    }

    /// Appends the [`HEADER_LEN`] header bytes, CRC included.
    ///
    /// # Errors
    /// [`EncodeError::InvalidPageSize`] for a page size the format forbids, [`EncodeError::UnknownFlags`] if any
    /// flag bit is unknown to this build (a writer sets only bits it knows). Nothing is appended then.
    pub fn encode(&self, w: &mut Writer) -> Result<(), EncodeError> {
        if !is_valid_page_size(self.page_size) {
            return Err(EncodeError::InvalidPageSize {
                size: self.page_size,
                min: MIN_PAGE_SIZE,
                max: MAX_PAGE_SIZE,
            });
        }
        let unknown = (self.incompat_flags & !KNOWN_INCOMPAT_FLAGS)
            | (self.rocompat_flags & !KNOWN_ROCOMPAT_FLAGS);
        if unknown != 0 {
            return Err(EncodeError::UnknownFlags { flags: unknown });
        }
        Checksummed(Layout(*self)).encode(w)
    }

    /// Reads and validates the next [`HEADER_LEN`] bytes, in the order of SPEC §4.2: magic, `format_major`, CRC,
    /// `incompat_flags`, then `page_size` and reserved bytes.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a short input, wrong magic, a CRC mismatch, an invalid page size or non-zero
    /// reserved bytes; [`Error::Unsupported`] for an unknown `format_major` or unknown `incompat_flags` bits. On
    /// error the position is unchanged.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let mut peek = r.clone();
        let magic_offset = peek.offset();
        if peek.read_array::<{ MAGIC.len() }>()? != MAGIC {
            return Err(Error::Corrupt {
                offset: magic_offset,
                reason: Corruption::BadMagic,
            });
        }
        check_major(&mut peek)?;
        let Checksummed(Layout(header)) = Checksummed::<Layout>::decode(r)?;
        Ok(header)
    }
}

/// SPEC §4.2: a power of two in `MIN_PAGE_SIZE..=MAX_PAGE_SIZE`.
const fn is_valid_page_size(size: u32) -> bool {
    size.is_power_of_two() && size >= MIN_PAGE_SIZE && size <= MAX_PAGE_SIZE
}

/// Reads `format_major` and rejects any but [`FORMAT_MAJOR`].
fn check_major(r: &mut Reader<'_>) -> Result<()> {
    let offset = r.offset();
    let major = r.read_u16()?;
    if major == FORMAT_MAJOR {
        Ok(())
    } else {
        Err(Error::Unsupported {
            offset,
            reason: Unsupported::FormatMajor {
                major,
                supported: FORMAT_MAJOR,
            },
        })
    }
}

/// The checksummed layout, private so that every decode goes through [`Header::decode`]'s magic and major checks.
struct Layout(Header);

impl ChecksummedLayout for Layout {
    const LEN: usize = HEADER_LEN;
    const CRC_OFFSET: usize = CRC_OFFSET;

    fn encode_fields(&self, w: &mut Writer) {
        let h = &self.0;
        w.write_bytes(&MAGIC);
        w.write_u16(FORMAT_MAJOR);
        w.write_u16(h.format_minor);
        w.write_u32(0); // CRC placeholder
        w.write_u64(h.incompat_flags);
        w.write_u64(h.rocompat_flags);
        w.write_bytes(&h.db_uuid);
        w.write_u32(h.page_size);
        w.write_zeros(RESERVED_LEN);
    }

    fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
        // Magic and major were checked by `Header::decode` before the CRC.
        r.skip(MINOR_OFFSET)?;
        let format_minor = r.read_u16()?;
        r.skip(4)?; // CRC
        let incompat_offset = r.offset();
        let incompat_flags = r.read_u64()?;
        let unknown = incompat_flags & !KNOWN_INCOMPAT_FLAGS;
        if unknown != 0 {
            return Err(Error::Unsupported {
                offset: incompat_offset,
                reason: Unsupported::IncompatFlags { flags: unknown },
            });
        }
        let rocompat_flags = r.read_u64()?;
        let db_uuid = r.read_array::<16>()?;
        let page_size_offset = r.offset();
        let page_size = r.read_u32()?;
        if !is_valid_page_size(page_size) {
            return Err(Error::Corrupt {
                offset: page_size_offset,
                reason: Corruption::InvalidPageSize {
                    size: page_size,
                    min: MIN_PAGE_SIZE,
                    max: MAX_PAGE_SIZE,
                },
            });
        }
        r.expect_zeros(RESERVED_LEN)?;
        Ok(Self(Header {
            format_minor,
            incompat_flags,
            rocompat_flags,
            db_uuid,
            page_size,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checksum::Crc32c;

    const BASE: u64 = 0x1000;

    fn encode(h: &Header) -> Vec<u8> {
        let mut w = Writer::new();
        h.encode(&mut w).unwrap();
        w.into_inner()
    }

    /// Recomputes `header_crc` (SPEC §4.2: over bytes 0..12 + 16..4096), so a field edit reaches validation.
    fn fix_crc(bytes: &mut [u8]) {
        let mut crc = Crc32c::new();
        crc.update(&bytes[..12]);
        crc.update(&bytes[16..HEADER_LEN]);
        bytes[12..16].copy_from_slice(&crc.finish().to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Header> {
        let mut r = Reader::with_base_offset(bytes, BASE);
        let result = Header::decode(&mut r);
        let expected_pos = if result.is_ok() { HEADER_LEN } else { 0 };
        assert_eq!(r.position(), expected_pos);
        result
    }

    fn corrupt(offset: u64, reason: Corruption) -> Error {
        Error::Corrupt {
            offset: BASE + offset,
            reason,
        }
    }

    fn sample() -> Header {
        Header {
            format_minor: 0x0302,
            incompat_flags: 0,
            rocompat_flags: 0,
            db_uuid: core::array::from_fn(|i| 0xa0 + u8::try_from(i).unwrap()),
            page_size: 8192,
        }
    }

    #[test]
    fn golden_header() {
        let mut expected = Vec::new();
        expected.extend_from_slice(b"PERSIA\0\x01");
        expected.extend_from_slice(&[1, 0, 0x02, 0x03]);
        expected.extend_from_slice(&[0; 4]); // CRC, patched below
        expected.extend_from_slice(&[0; 16]); // incompat, rocompat
        expected.extend(0xa0..0xb0_u8);
        expected.extend_from_slice(&[0, 0x20, 0, 0]);
        expected.resize(HEADER_LEN, 0);
        fix_crc(&mut expected);
        assert_eq!(encode(&sample()), expected);
        // Pins the CRC32C itself, independently of `fix_crc`.
        assert_eq!(&expected[12..16], &0x584e_ffca_u32.to_le_bytes());
        assert_eq!(decode(&expected), Ok(sample()));
    }

    #[test]
    fn new_uses_defaults() {
        let h = Header::new([1; 16]);
        assert_eq!(
            (
                h.format_minor,
                h.page_size,
                h.incompat_flags,
                h.rocompat_flags
            ),
            (FORMAT_MINOR, DEFAULT_PAGE_SIZE, 0, 0)
        );
        assert_eq!(decode(&encode(&h)), Ok(h));
    }

    #[test]
    fn for_write_never_lowers_the_minor() {
        for minor in [FORMAT_MINOR, FORMAT_MINOR + 1, u16::MAX] {
            let h = Header {
                format_minor: minor,
                ..sample()
            };
            assert_eq!(h.for_write(), h);
        }
        // An older minor is raised to this build's (vacuous while FORMAT_MINOR is 0).
        let old = Header {
            format_minor: 0,
            ..sample()
        };
        assert_eq!(old.for_write().format_minor, FORMAT_MINOR);
        assert_eq!(
            Header {
                format_minor: FORMAT_MINOR,
                ..old
            },
            old.for_write()
        );
    }

    #[test]
    fn bad_magic_is_corrupt() {
        let mut bytes = encode(&sample());
        bytes[7] = 2;
        assert_eq!(decode(&bytes), Err(corrupt(0, Corruption::BadMagic)));
        assert_eq!(
            decode(&[0; 8]),
            Err(corrupt(0, Corruption::BadMagic)),
            "magic is checked before the length of the region"
        );
    }

    #[test]
    fn unknown_major_is_unsupported_before_the_crc() {
        for major in [0_u16, 2, u16::MAX] {
            let mut bytes = encode(&sample());
            bytes[8..10].copy_from_slice(&major.to_le_bytes());
            let expected = Err(Error::Unsupported {
                offset: BASE + 8,
                reason: Unsupported::FormatMajor {
                    major,
                    supported: FORMAT_MAJOR,
                },
            });
            // The CRC no longer matches, but a future major may lay the header out differently.
            assert_eq!(decode(&bytes), expected);
            // A future header may also be shorter.
            assert_eq!(decode(&bytes[..10]), expected);
        }
    }

    #[test]
    fn any_minor_is_accepted() {
        for minor in [0_u16, 1, u16::MAX] {
            let h = Header {
                format_minor: minor,
                ..sample()
            };
            assert_eq!(decode(&encode(&h)), Ok(h));
        }
    }

    #[test]
    fn crc_mismatch_is_corrupt() {
        let mut bytes = encode(&sample());
        bytes[HEADER_LEN - 1] = 1;
        let Err(err) = decode(&bytes) else {
            panic!("accepted a bad CRC")
        };
        assert!(matches!(
            err.corruption(),
            Some((o, Corruption::ChecksumMismatch { .. })) if o == BASE + 12
        ));
    }

    #[test]
    fn unknown_incompat_flags_are_unsupported() {
        for flags in [1_u64, 1 << 63, u64::MAX] {
            let mut bytes = encode(&sample());
            bytes[16..24].copy_from_slice(&flags.to_le_bytes());
            // An invalid page size too: SPEC §4.2 checks incompat flags first.
            bytes[48..52].copy_from_slice(&0_u32.to_le_bytes());
            fix_crc(&mut bytes);
            assert_eq!(
                decode(&bytes),
                Err(Error::Unsupported {
                    offset: BASE + 16,
                    reason: Unsupported::IncompatFlags {
                        flags: flags & !KNOWN_INCOMPAT_FLAGS
                    },
                })
            );
        }
    }

    #[test]
    fn unknown_rocompat_flags_open_read_only() {
        let mut bytes = encode(&sample());
        bytes[24..32].copy_from_slice(&(1_u64 << 5).to_le_bytes());
        fix_crc(&mut bytes);
        let h = decode(&bytes).unwrap();
        assert_eq!(h.rocompat_flags, 1 << 5);
        assert_eq!(h.access(), Access::ReadOnly);
        assert_eq!(sample().access(), Access::ReadWrite);
        assert_eq!(
            h.encode(&mut Writer::new()),
            Err(EncodeError::UnknownFlags { flags: 1 << 5 }),
            "a writer sets only bits it knows"
        );
    }

    #[test]
    fn page_size_must_be_a_power_of_two_in_range() {
        for size in [4096_u32, 8192, 16384, 32768, 65536] {
            let h = Header {
                page_size: size,
                ..sample()
            };
            assert_eq!(decode(&encode(&h)), Ok(h));
        }
        for size in [
            0_u32,
            1,
            2048,
            4095,
            4097,
            12288,
            131_072,
            1 << 31,
            u32::MAX,
        ] {
            let mut bytes = encode(&sample());
            bytes[48..52].copy_from_slice(&size.to_le_bytes());
            fix_crc(&mut bytes);
            let range = (MIN_PAGE_SIZE, MAX_PAGE_SIZE);
            assert_eq!(
                decode(&bytes),
                Err(corrupt(
                    48,
                    Corruption::InvalidPageSize {
                        size,
                        min: range.0,
                        max: range.1
                    }
                ))
            );
            let mut w = Writer::new();
            assert_eq!(
                Header {
                    page_size: size,
                    ..sample()
                }
                .encode(&mut w),
                Err(EncodeError::InvalidPageSize {
                    size,
                    min: range.0,
                    max: range.1
                })
            );
            assert!(w.is_empty());
        }
    }

    #[test]
    fn reserved_bytes_must_be_zero() {
        for at in [52, 55, 56, 1000, HEADER_LEN - 1] {
            let mut bytes = encode(&sample());
            bytes[at] = 0x80;
            fix_crc(&mut bytes);
            assert_eq!(
                decode(&bytes),
                Err(corrupt(
                    at as u64,
                    Corruption::NonZeroReserved { value: 0x80 }
                ))
            );
        }
    }

    #[test]
    fn short_input_is_corrupt() {
        let bytes = encode(&sample());
        for len in [0, 7, 9, HEADER_LEN - 1] {
            let Err(err) = decode(&bytes[..len]) else {
                panic!("accepted {len} bytes")
            };
            assert!(matches!(
                err.corruption(),
                Some((_, Corruption::UnexpectedEof { .. }))
            ));
        }
    }

    #[test]
    fn decode_stops_after_the_header() {
        let mut bytes = encode(&sample());
        bytes.extend_from_slice(b"slots");
        let mut r = Reader::new(&bytes);
        assert_eq!(Header::decode(&mut r), Ok(sample()));
        assert_eq!(r.rest(), b"slots");
    }
}
