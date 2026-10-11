//! CRC32C (Castagnoli) checksums for headers, frames and blocks (SPEC §4.1).
//!
//! A thin wrapper over the `crc32c` crate (hardware-accelerated where available) so the rest of the
//! workspace has one spelling of "the checksum" and one way to report a mismatch.

use crate::error::{Corruption, Error, Result};

/// CRC32C of `data`.
///
/// ```
/// // RFC 3720 §B.4 check value.
/// assert_eq!(persia_format::checksum::crc32c(b"123456789"), 0xe306_9283);
/// ```
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c::crc32c(data)
}

/// Verifies that `data` has CRC32C `stored`.
///
/// # Errors
/// [`Error::Corrupt`] with [`Corruption::ChecksumMismatch`] at `offset` (the absolute position of the stored
/// checksum, so a report points at the field that disagrees) when the checksums differ.
pub fn verify_crc32c(data: &[u8], stored: u32, offset: u64) -> Result<()> {
    check(crc32c(data), stored, offset)
}

/// Compares a computed checksum against the stored one.
pub(crate) fn check(computed: u32, stored: u32, offset: u64) -> Result<()> {
    if computed == stored {
        Ok(())
    } else {
        Err(Error::Corrupt {
            offset,
            reason: Corruption::ChecksumMismatch { stored, computed },
        })
    }
}

/// Incremental CRC32C over data that arrives in pieces (e.g. a frame's `kind..padding` range, SPEC §4.4).
///
/// Feeding the same bytes in any split gives the same result as [`crc32c()`] over their concatenation.
///
/// ```
/// use persia_format::checksum::{Crc32c, crc32c};
///
/// let mut crc = Crc32c::new();
/// crc.update(b"1234");
/// crc.update(b"56789");
/// assert_eq!(crc.finish(), crc32c(b"123456789"));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Crc32c {
    state: u32,
}

impl Crc32c {
    /// A checksum of no bytes.
    pub const fn new() -> Self {
        Self { state: 0 }
    }

    /// Appends `data`.
    pub fn update(&mut self, data: &[u8]) {
        self.state = crc32c::crc32c_append(self.state, data);
    }

    /// The checksum of everything appended so far. Does not reset: more data can still be appended.
    pub const fn finish(self) -> u32 {
        self.state
    }

    /// Verifies the bytes appended so far against `stored`, like [`verify_crc32c`].
    ///
    /// # Errors
    /// [`Error::Corrupt`] with [`Corruption::ChecksumMismatch`] at `offset` when the checksums differ.
    pub fn verify(self, stored: u32, offset: u64) -> Result<()> {
        check(self.state, stored, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // RFC 3720 §B.4.
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[0xffu8; 32]), 0x62a8_ab43);
        let ascending: Vec<u8> = (0..32).collect();
        assert_eq!(crc32c(&ascending), 0x46dd_794e);
        let descending: Vec<u8> = (0..32).rev().collect();
        assert_eq!(crc32c(&descending), 0x113f_db5c);
    }

    #[test]
    fn empty_stream_is_zero() {
        assert_eq!(Crc32c::new().finish(), crc32c(b""));
        assert_eq!(Crc32c::default(), Crc32c::new());
    }

    #[test]
    fn mismatch_reports_both_values_at_offset() {
        let data = b"123456789";
        assert_eq!(verify_crc32c(data, 0xe306_9283, 7), Ok(()));
        assert_eq!(
            verify_crc32c(data, 0xe306_9282, 7),
            Err(Error::Corrupt {
                offset: 7,
                reason: Corruption::ChecksumMismatch {
                    stored: 0xe306_9282,
                    computed: 0xe306_9283,
                },
            })
        );
        let mut crc = Crc32c::new();
        crc.update(data);
        assert_eq!(crc.verify(0xe306_9283, 0), Ok(()));
        assert!(crc.verify(0, 0).is_err());
    }
}
