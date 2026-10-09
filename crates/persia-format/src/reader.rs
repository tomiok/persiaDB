//! Bounds-checked little-endian cursor over a byte slice (SPEC §4.1).

use crate::error::{Corruption, Error, Result, to_u64};

/// A cursor that decodes little-endian values from a byte slice.
///
/// Every read is bounds-checked: running out of input returns [`Error::Corrupt`] with
/// [`Corruption::UnexpectedEof`] and leaves the position unchanged, so callers can report or retry.
/// It never panics, whatever the input or requested length.
///
/// ```
/// use persia_format::Reader;
///
/// let mut r = Reader::new(&[0x2a, 0x01, 0x00, 0xff]);
/// assert_eq!(r.read_u8()?, 42);
/// assert_eq!(r.read_u16()?, 1);
/// assert_eq!(r.remaining(), 1);
/// assert!(r.read_u32().is_err()); // only 1 byte left
/// assert_eq!(r.position(), 3); // unchanged by the failed read
/// # Ok::<(), persia_format::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    base: u64,
}

macro_rules! read_le {
    ($($(#[$doc:meta])* $name:ident -> $ty:ty;)*) => {$(
        $(#[$doc])*
        ///
        /// # Errors
        /// [`Error::Corrupt`] if fewer bytes remain than the value needs.
        pub fn $name(&mut self) -> Result<$ty> {
            self.read_array().map(<$ty>::from_le_bytes)
        }
    )*};
}

impl<'a> Reader<'a> {
    /// A reader at the start of `buf`; error offsets are relative to `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self::with_base_offset(buf, 0)
    }

    /// A reader over `buf`, which starts at absolute offset `base` in its enclosing file or object.
    /// Error offsets include `base`, so they point at the real position of the bad byte.
    pub fn with_base_offset(buf: &'a [u8], base: u64) -> Self {
        Self { buf, pos: 0, base }
    }

    /// Bytes consumed so far, relative to the start of the slice.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Absolute offset of the next byte (`base + position`).
    pub fn offset(&self) -> u64 {
        self.base.saturating_add(to_u64(self.pos))
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// True when all input has been consumed.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// The unconsumed input, without advancing.
    pub fn rest(&self) -> &'a [u8] {
        self.buf.get(self.pos..).unwrap_or_default()
    }

    /// Borrows the next `len` bytes without copying and advances past them.
    ///
    /// # Errors
    /// [`Error::Corrupt`] if fewer than `len` bytes remain (including absurd lengths from corrupt length fields).
    pub fn read_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let bytes = self.rest().get(..len).ok_or_else(|| self.eof(len))?;
        self.pos += len; // cannot overflow: pos + len <= buf.len()
        Ok(bytes)
    }

    /// Advances past the next `len` bytes.
    ///
    /// # Errors
    /// [`Error::Corrupt`] if fewer than `len` bytes remain.
    pub fn skip(&mut self, len: usize) -> Result<()> {
        self.read_bytes(len).map(|_| ())
    }

    /// Copies the next `N` bytes into an array (magic numbers, UUIDs, hashes) and advances past them.
    ///
    /// # Errors
    /// [`Error::Corrupt`] if fewer than `N` bytes remain.
    pub fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let (head, _) = self
            .rest()
            .split_first_chunk::<N>()
            .ok_or_else(|| self.eof(N))?;
        self.pos += N; // cannot overflow: pos + N <= buf.len()
        Ok(*head)
    }

    read_le! {
        /// Reads a `u8`.
        read_u8 -> u8;
        /// Reads a little-endian `u16`.
        read_u16 -> u16;
        /// Reads a little-endian `u32`.
        read_u32 -> u32;
        /// Reads a little-endian `u64`.
        read_u64 -> u64;
        /// Reads a little-endian `u128` (e.g. segment ids, SPEC §5.1).
        read_u128 -> u128;
        /// Reads a little-endian `i32`.
        read_i32 -> i32;
        /// Reads a little-endian `i64` (e.g. `datetime` microseconds, SPEC §6.1).
        read_i64 -> i64;
    }

    fn eof(&self, needed: usize) -> Error {
        Error::Corrupt {
            offset: self.offset(),
            reason: Corruption::UnexpectedEof {
                needed: to_u64(needed),
                available: to_u64(self.remaining()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eof_at(offset: u64, needed: u64, available: u64) -> Error {
        Error::Corrupt {
            offset,
            reason: Corruption::UnexpectedEof { needed, available },
        }
    }

    #[test]
    fn reads_every_width_little_endian() {
        let mut bytes = vec![0xab];
        bytes.extend(0x1234_u16.to_le_bytes());
        bytes.extend(0xdead_beef_u32.to_le_bytes());
        bytes.extend(0x0102_0304_0506_0708_u64.to_le_bytes());
        bytes.extend(u128::MAX.wrapping_sub(7).to_le_bytes());
        bytes.extend((-5_i32).to_le_bytes());
        bytes.extend(i64::MIN.to_le_bytes());
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_u8(), Ok(0xab));
        assert_eq!(r.read_u16(), Ok(0x1234));
        assert_eq!(r.read_u32(), Ok(0xdead_beef));
        assert_eq!(r.read_u64(), Ok(0x0102_0304_0506_0708));
        assert_eq!(r.read_u128(), Ok(u128::MAX - 7));
        assert_eq!(r.read_i32(), Ok(-5));
        assert_eq!(r.read_i64(), Ok(i64::MIN));
        assert!(r.is_empty());
    }

    #[test]
    fn byte_order_is_little_endian() {
        assert_eq!(
            Reader::new(&[0x01, 0x02, 0x03, 0x04]).read_u32(),
            Ok(0x0403_0201)
        );
    }

    #[test]
    fn short_read_reports_offset_needed_available() {
        let mut r = Reader::new(&[1, 2, 3, 4, 5]);
        r.skip(2).unwrap();
        assert_eq!(r.read_u64(), Err(eof_at(2, 8, 3)));
    }

    #[test]
    fn failed_read_leaves_position_unchanged() {
        let mut r = Reader::new(&[1, 2, 3]);
        assert_eq!(r.read_u8(), Ok(1));
        assert!(r.read_u32().is_err());
        assert!(r.read_bytes(3).is_err());
        assert!(r.read_array::<16>().is_err());
        assert_eq!(r.position(), 1);
        assert_eq!(r.read_u16(), Ok(0x0302));
    }

    #[test]
    fn every_read_on_empty_input_fails_cleanly() {
        let mut r = Reader::new(&[]);
        assert_eq!(r.read_u8(), Err(eof_at(0, 1, 0)));
        assert_eq!(r.read_u16(), Err(eof_at(0, 2, 0)));
        assert_eq!(r.read_u32(), Err(eof_at(0, 4, 0)));
        assert_eq!(r.read_u64(), Err(eof_at(0, 8, 0)));
        assert_eq!(r.read_u128(), Err(eof_at(0, 16, 0)));
        assert_eq!(r.read_i32(), Err(eof_at(0, 4, 0)));
        assert_eq!(r.read_i64(), Err(eof_at(0, 8, 0)));
        assert_eq!(r.read_bytes(1), Err(eof_at(0, 1, 0)));
        assert_eq!(r.skip(1), Err(eof_at(0, 1, 0)));
        assert!(r.is_empty());
    }

    #[test]
    fn zero_length_reads_always_succeed() {
        let mut r = Reader::new(&[]);
        assert_eq!(r.read_bytes(0), Ok(&[][..]));
        assert_eq!(r.read_array::<0>(), Ok([]));
        assert_eq!(r.skip(0), Ok(()));
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn absurd_lengths_from_corrupt_fields_do_not_panic() {
        let mut r = Reader::new(&[0; 4]);
        r.skip(1).unwrap();
        assert_eq!(r.read_bytes(usize::MAX), Err(eof_at(1, u64::MAX, 3)));
        assert_eq!(r.skip(usize::MAX - 1), Err(eof_at(1, u64::MAX - 1, 3)));
        assert_eq!(r.position(), 1);
    }

    #[test]
    fn read_bytes_borrows_without_copying() {
        let buf = [9, 8, 7, 6];
        let mut r = Reader::new(&buf);
        r.skip(1).unwrap();
        let got = r.read_bytes(2).unwrap();
        assert_eq!(got, &[8, 7]);
        assert!(core::ptr::eq(got.as_ptr(), buf[1..].as_ptr()));
        assert_eq!(r.rest(), &[6]);
    }

    #[test]
    fn errors_carry_absolute_offsets_with_base() {
        let mut r = Reader::with_base_offset(&[0; 3], 8192);
        r.skip(2).unwrap();
        assert_eq!(r.offset(), 8194);
        assert_eq!(r.read_u32(), Err(eof_at(8194, 4, 1)));
    }

    #[test]
    fn base_offset_saturates_instead_of_overflowing() {
        let mut r = Reader::with_base_offset(&[0; 2], u64::MAX - 1);
        r.skip(2).unwrap();
        assert_eq!(r.offset(), u64::MAX);
        assert_eq!(r.read_u8(), Err(eof_at(u64::MAX, 1, 0)));
    }

    #[test]
    fn error_message_names_offset_and_sizes() {
        let msg = Reader::with_base_offset(&[0], 100)
            .read_u32()
            .unwrap_err()
            .to_string();
        assert_eq!(
            msg,
            "corrupt data at offset 100: unexpected end of input: needed 4 bytes, 1 available"
        );
    }
}
