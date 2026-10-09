//! Little-endian encoder into a growable buffer (SPEC §4.1). The mirror of [`crate::Reader`].

use core::num::NonZeroUsize;

use crate::align::padding_for;
use crate::error::to_u64;

/// Encodes little-endian values into a `Vec<u8>`.
///
/// Like [`crate::Reader`], a writer knows the absolute offset its buffer will start at, so
/// [`Writer::pad_to`] aligns to the position in the final file or object (SPEC §4.1), not just within the buffer.
/// Writes are infallible: only allocation can fail, which aborts like any `Vec` growth.
///
/// ```
/// use persia_format::{ALIGNMENT, Reader, Writer};
///
/// let mut w = Writer::new();
/// w.write_u8(42);
/// w.write_u32(7);
/// w.pad_to(ALIGNMENT); // 5 bytes so far: 3 zero bytes of padding
/// w.write_u64(1);
/// let bytes = w.into_inner();
/// assert_eq!(bytes.len(), 16);
///
/// let mut r = Reader::new(&bytes);
/// assert_eq!(r.read_u8()?, 42);
/// assert_eq!(r.read_u32()?, 7);
/// r.skip_padding(ALIGNMENT)?;
/// assert_eq!(r.read_u64()?, 1);
/// # Ok::<(), persia_format::Error>(())
/// ```
#[derive(Debug, Clone, Default)]
pub struct Writer {
    buf: Vec<u8>,
    base: u64,
}

macro_rules! write_le {
    ($($(#[$doc:meta])* $name:ident($ty:ty);)*) => {$(
        $(#[$doc])*
        pub fn $name(&mut self, value: $ty) {
            self.buf.extend_from_slice(&value.to_le_bytes());
        }
    )*};
}

impl Writer {
    /// An empty writer whose output starts at offset 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty writer whose output will be placed at absolute offset `base` (alignment is computed from it).
    pub fn with_base_offset(base: u64) -> Self {
        Self {
            buf: Vec::new(),
            base,
        }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True when nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Absolute offset of the next byte (`base + len`). Saturates at `u64::MAX`, which no real file reaches.
    pub fn offset(&self) -> u64 {
        self.base.saturating_add(to_u64(self.buf.len()))
    }

    /// The bytes written so far.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Consumes the writer and returns its buffer.
    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    /// Appends raw bytes.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Appends zero bytes until the absolute offset is a multiple of `align` (no-op if already aligned).
    pub fn pad_to(&mut self, align: NonZeroUsize) {
        let pad = padding_for(self.offset(), align);
        self.buf.resize(self.buf.len().saturating_add(pad), 0);
    }

    write_le! {
        /// Appends a `u8`.
        write_u8(u8);
        /// Appends a little-endian `u16`.
        write_u16(u16);
        /// Appends a little-endian `u32`.
        write_u32(u32);
        /// Appends a little-endian `u64`.
        write_u64(u64);
        /// Appends a little-endian `u128`.
        write_u128(u128);
        /// Appends a little-endian `i32`.
        write_i32(i32);
        /// Appends a little-endian `i64`.
        write_i64(i64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ALIGNMENT;

    #[test]
    fn writes_every_width_little_endian() {
        let mut w = Writer::new();
        w.write_u8(0xab);
        w.write_u16(0x1234);
        w.write_u32(0xdead_beef);
        w.write_u64(0x0102_0304_0506_0708);
        w.write_u128(0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100);
        w.write_i32(-2);
        w.write_i64(i64::MIN);
        let mut expected = vec![
            0xab, 0x34, 0x12, 0xef, 0xbe, 0xad, 0xde, 8, 7, 6, 5, 4, 3, 2, 1,
        ];
        expected.extend(0..16_u8);
        expected.extend([0xfe, 0xff, 0xff, 0xff]);
        expected.extend([0, 0, 0, 0, 0, 0, 0, 0x80]);
        assert_eq!(w.as_slice(), expected.as_slice());
    }

    #[test]
    fn pad_to_writes_zeros_up_to_the_boundary() {
        for (prefix, expected_len) in [(0, 0), (1, 8), (5, 8), (7, 8), (8, 8), (9, 16)] {
            let mut w = Writer::new();
            w.write_bytes(&vec![0xff; prefix]);
            w.pad_to(ALIGNMENT);
            assert_eq!(w.len(), expected_len, "prefix={prefix}");
            assert!(
                w.as_slice()[prefix..].iter().all(|&b| b == 0),
                "padding must be zeros"
            );
        }
    }

    #[test]
    fn pad_to_aligns_on_the_absolute_offset() {
        // The buffer will start at file offset 8195: 5 bytes reach 8200.
        let mut w = Writer::with_base_offset(8195);
        w.pad_to(ALIGNMENT);
        assert_eq!(w.len(), 5);
        assert_eq!(w.offset(), 8200);
        w.pad_to(ALIGNMENT);
        assert_eq!(w.len(), 5, "already aligned: no-op");
    }

    #[test]
    fn align_one_is_a_no_op() {
        let mut w = Writer::new();
        w.write_u8(1);
        w.pad_to(NonZeroUsize::MIN);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn empty_writer() {
        let w = Writer::with_base_offset(16);
        assert!(w.is_empty());
        assert_eq!(w.offset(), 16);
        assert_eq!(w.into_inner(), Vec::<u8>::new());
    }
}
