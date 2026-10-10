//! Byte offsets and lengths as distinct types, with checked arithmetic only (CLAUDE.md "Safety & errors").
//!
//! Offsets and lengths read from disk are hostile: `offset + len` can overflow and a length can exceed what
//! fits in memory. Keeping them out of bare `u64` makes every combination go through a checked operation,
//! and the type system stops a length being used where an offset is expected (SPEC §4.1: offsets are
//! absolute `u64`).

use core::num::NonZeroUsize;
use core::ops::Range;

use crate::align::padding_for;
use crate::error::{Result, to_u64};
use crate::reader::Reader;
use crate::writer::Writer;

/// An absolute byte position in a file or object (SPEC §4.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Offset(u64);

/// A byte count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Len(u64);

impl Offset {
    /// The start of a file or object.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw offset.
    pub const fn new(offset: u64) -> Self {
        Self(offset)
    }

    /// The raw offset.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The offset `len` bytes further on, or `None` on overflow.
    pub const fn checked_add(self, len: Len) -> Option<Self> {
        match self.0.checked_add(len.0) {
            Some(end) => Some(Self(end)),
            None => None,
        }
    }

    /// The distance from `start` to `self`, or `None` if `start` is after `self`.
    pub const fn checked_since(self, start: Self) -> Option<Len> {
        match self.0.checked_sub(start.0) {
            Some(len) => Some(Len(len)),
            None => None,
        }
    }

    /// The next multiple of `align` at or after `self`, or `None` on overflow.
    pub fn checked_align_up(self, align: NonZeroUsize) -> Option<Self> {
        self.checked_add(Len::from_usize(padding_for(self.0, align)))
    }

    /// The in-memory range of the `len` bytes at `self`, provided they lie within an object of `limit` bytes.
    ///
    /// This is the check every decoder must make before following an offset/length pair read from disk:
    /// `None` means the pair points outside the object (or overflows), which the caller reports as corruption.
    pub fn span(self, len: Len, limit: Len) -> Option<Range<usize>> {
        let end = self.checked_add(len)?;
        if end.0 > limit.0 {
            return None;
        }
        let start = usize::try_from(self.0).ok()?;
        let end = usize::try_from(end.0).ok()?;
        Some(start..end)
    }
}

impl Len {
    /// Zero bytes.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw length.
    pub const fn new(len: u64) -> Self {
        Self(len)
    }

    /// The length of an in-memory buffer; lossless (`usize` is at most 64 bits, see `error::to_u64`).
    pub fn from_usize(len: usize) -> Self {
        Self(to_u64(len))
    }

    /// The raw length.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The length as a `usize`, or `None` if it cannot be addressed in memory (32-bit targets).
    pub fn to_usize(self) -> Option<usize> {
        usize::try_from(self.0).ok()
    }

    /// `self + other`, or `None` on overflow.
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// `self - other`, or `None` if `other` is longer.
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(diff) => Some(Self(diff)),
            None => None,
        }
    }

    /// `self * count` (e.g. a record size times a record count), or `None` on overflow.
    pub const fn checked_mul(self, count: u64) -> Option<Self> {
        match self.0.checked_mul(count) {
            Some(product) => Some(Self(product)),
            None => None,
        }
    }
}

impl Writer {
    /// Appends an offset as a little-endian `u64`.
    pub fn write_offset(&mut self, offset: Offset) {
        self.write_u64(offset.get());
    }

    /// Appends a length as a little-endian `u64`.
    pub fn write_len(&mut self, len: Len) {
        self.write_u64(len.get());
    }
}

impl Reader<'_> {
    /// Reads a little-endian `u64` offset. It is not validated: use [`Offset::span`] before following it.
    ///
    /// # Errors
    /// [`crate::Error::Corrupt`] if fewer than 8 bytes remain.
    pub fn read_offset(&mut self) -> Result<Offset> {
        self.read_u64().map(Offset::new)
    }

    /// Reads a little-endian `u64` length. It is not validated: use [`Offset::span`] before following it.
    ///
    /// # Errors
    /// [`crate::Error::Corrupt`] if fewer than 8 bytes remain.
    pub fn read_len(&mut self) -> Result<Len> {
        self.read_u64().map(Len::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ALIGNMENT;

    #[test]
    fn offset_arithmetic_is_checked() {
        let o = Offset::new(100);
        assert_eq!(o.checked_add(Len::new(28)), Some(Offset::new(128)));
        assert_eq!(Offset::new(u64::MAX).checked_add(Len::new(1)), None);
        assert_eq!(
            Offset::new(u64::MAX).checked_add(Len::ZERO),
            Some(Offset::new(u64::MAX))
        );
        assert_eq!(o.checked_since(Offset::new(40)), Some(Len::new(60)));
        assert_eq!(o.checked_since(o), Some(Len::ZERO));
        assert_eq!(o.checked_since(Offset::new(101)), None);
    }

    #[test]
    fn align_up() {
        assert_eq!(
            Offset::new(9).checked_align_up(ALIGNMENT),
            Some(Offset::new(16))
        );
        assert_eq!(
            Offset::new(16).checked_align_up(ALIGNMENT),
            Some(Offset::new(16))
        );
        assert_eq!(Offset::ZERO.checked_align_up(ALIGNMENT), Some(Offset::ZERO));
        assert_eq!(Offset::new(u64::MAX).checked_align_up(ALIGNMENT), None);
        assert_eq!(
            Offset::new(u64::MAX).checked_align_up(NonZeroUsize::MIN),
            Some(Offset::new(u64::MAX))
        );
    }

    #[test]
    fn len_arithmetic_is_checked() {
        let l = Len::new(10);
        assert_eq!(l.checked_add(Len::new(5)), Some(Len::new(15)));
        assert_eq!(Len::new(u64::MAX).checked_add(Len::new(1)), None);
        assert_eq!(l.checked_sub(Len::new(10)), Some(Len::ZERO));
        assert_eq!(l.checked_sub(Len::new(11)), None);
        assert_eq!(l.checked_mul(3), Some(Len::new(30)));
        assert_eq!(Len::new(1 << 33).checked_mul(1 << 31), None);
        assert_eq!(Len::from_usize(7).to_usize(), Some(7));
        assert_eq!(Len::from_usize(usize::MAX).get(), usize::MAX as u64);
    }

    #[test]
    fn span_accepts_exactly_the_ranges_inside_the_limit() {
        let limit = Len::new(64);
        assert_eq!(Offset::new(0).span(Len::new(64), limit), Some(0..64));
        assert_eq!(Offset::new(60).span(Len::new(4), limit), Some(60..64));
        assert_eq!(Offset::new(64).span(Len::ZERO, limit), Some(64..64));
        assert_eq!(Offset::new(60).span(Len::new(5), limit), None);
        assert_eq!(Offset::new(65).span(Len::ZERO, limit), None);
        // Hostile pairs whose sum wraps around must not look in range.
        assert_eq!(Offset::new(u64::MAX).span(Len::new(2), limit), None);
        assert_eq!(
            Offset::new(8).span(Len::new(u64::MAX), Len::new(u64::MAX)),
            None
        );
    }

    #[test]
    fn reader_writer_round_trip() {
        let mut w = Writer::new();
        w.write_offset(Offset::new(0x0102_0304_0506_0708));
        w.write_len(Len::new(u64::MAX));
        let bytes = w.into_inner();
        assert_eq!(bytes.get(..8), Some(&[8, 7, 6, 5, 4, 3, 2, 1][..]));
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_offset(), Ok(Offset::new(0x0102_0304_0506_0708)));
        assert_eq!(r.read_len(), Ok(Len::new(u64::MAX)));
        assert!(r.read_offset().is_err());
    }
}
