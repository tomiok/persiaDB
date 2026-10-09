//! Bounds-checked little-endian cursor over a byte slice (SPEC §4.1).

use core::num::NonZeroUsize;

use crate::align::padding_for;
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
    ///
    /// For a structure nested inside other data, use [`Reader::sub_reader`] or
    /// [`Reader::with_base_offset`] instead, so errors keep pointing at the real byte.
    pub fn new(buf: &'a [u8]) -> Self {
        Self::with_base_offset(buf, 0)
    }

    /// A reader over `buf`, which starts at absolute offset `base` in its enclosing file or object.
    /// Error offsets include `base`, so they point at the real position of the bad byte.
    pub fn with_base_offset(buf: &'a [u8], base: u64) -> Self {
        Self { buf, pos: 0, base }
    }

    /// Bytes consumed so far, relative to the start of the slice. For error reports and on-disk
    /// offsets use [`Reader::offset`] instead.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Absolute offset of the next byte (`base + position`): what error reports and on-disk offsets use
    /// (SPEC §4.1). Saturates at `u64::MAX`, which no real file reaches.
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
        // Never saturates: `get` succeeded, so pos + len <= buf.len().
        self.pos = self.pos.saturating_add(len);
        Ok(bytes)
    }

    /// Consumes the next `len` bytes and returns a reader over just them (a length-prefixed sub-structure),
    /// whose error offsets stay absolute.
    ///
    /// # Errors
    /// [`Error::Corrupt`] if fewer than `len` bytes remain.
    pub fn sub_reader(&mut self, len: usize) -> Result<Reader<'a>> {
        let base = self.offset();
        self.read_bytes(len)
            .map(|bytes| Reader::with_base_offset(bytes, base))
    }

    /// Skips the zero padding up to the next multiple of `align`, computed on the absolute [`Reader::offset`]
    /// (the counterpart of [`crate::Writer::pad_to`]).
    ///
    /// # Errors
    /// [`Error::Corrupt`] if the input ends inside the padding, or with [`Corruption::NonZeroPadding`] at the
    /// first non-zero byte. Either way the position is unchanged.
    pub fn skip_padding(&mut self, align: NonZeroUsize) -> Result<()> {
        let len = padding_for(self.offset(), align);
        let padding = self.rest().get(..len).ok_or_else(|| self.eof(len))?;
        if let Some((i, &value)) = padding.iter().enumerate().find(|&(_, &b)| b != 0) {
            return Err(Error::Corrupt {
                offset: self.offset().saturating_add(to_u64(i)),
                reason: Corruption::NonZeroPadding { value },
            });
        }
        self.skip(len)
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
        // Never saturates: the chunk exists, so pos + N <= buf.len().
        self.pos = self.pos.saturating_add(N);
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
    fn sub_reader_errors_keep_absolute_offsets() {
        let mut outer = Reader::with_base_offset(&[1, 2, 3, 4, 5, 6], 1000);
        outer.skip(1).unwrap();
        let mut inner = outer.sub_reader(4).unwrap();
        assert_eq!(outer.offset(), 1005);
        assert_eq!(inner.read_u16(), Ok(0x0302));
        // EOF inside the sub-structure: outer base + outer position + inner position.
        assert_eq!(inner.read_u32(), Err(eof_at(1003, 4, 2)));
        assert_eq!(inner.read_u16(), Ok(0x0504));
        assert!(inner.is_empty());
        assert_eq!(outer.read_u8(), Ok(6));
    }

    #[test]
    fn sub_reader_past_end_fails_without_advancing() {
        let mut outer = Reader::with_base_offset(&[0; 3], 10);
        assert_eq!(
            outer.sub_reader(4).map(|r| r.remaining()),
            Err(eof_at(10, 4, 3))
        );
        assert_eq!(outer.position(), 0);
    }

    #[test]
    fn skip_padding_consumes_zeros_to_the_absolute_boundary() {
        let mut r = Reader::with_base_offset(&[7, 0, 0, 0, 0, 9], 4093);
        assert_eq!(r.read_u8(), Ok(7)); // offset 4094
        r.skip_padding(crate::ALIGNMENT).unwrap(); // 2 bytes reach 4096
        assert_eq!(r.offset(), 4096);
        r.skip_padding(crate::ALIGNMENT).unwrap(); // already aligned: no-op
        assert_eq!(r.position(), 3);
    }

    #[test]
    fn skip_padding_rejects_non_zero_bytes_at_their_offset() {
        let mut r = Reader::with_base_offset(&[1, 0, 0, 5, 0, 0, 0, 0], 100);
        r.skip(1).unwrap(); // offset 101: 3 padding bytes to 104
        assert_eq!(
            r.skip_padding(crate::ALIGNMENT),
            Err(Error::Corrupt {
                offset: 103,
                reason: Corruption::NonZeroPadding { value: 5 },
            })
        );
        assert_eq!(r.position(), 1);
    }

    #[test]
    fn skip_padding_past_end_is_eof_without_advancing() {
        let mut r = Reader::new(&[1, 0, 0]);
        r.skip(1).unwrap();
        assert_eq!(r.skip_padding(crate::ALIGNMENT), Err(eof_at(1, 7, 2)));
        assert_eq!(r.position(), 1);
    }

    #[test]
    fn non_zero_padding_message() {
        let err = Error::Corrupt {
            offset: 9,
            reason: Corruption::NonZeroPadding { value: 0xab },
        };
        assert_eq!(
            err.to_string(),
            "corrupt data at offset 9: non-zero padding byte 0xab"
        );
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

    /// Runs `read` on a fresh reader over `len` zero bytes at `base`, returning the result and final position.
    fn try_width<T>(
        len: usize,
        base: u64,
        read: fn(&mut Reader<'_>) -> Result<T>,
    ) -> (Result<T>, usize) {
        let buf = vec![0xa5; len];
        let mut r = Reader::with_base_offset(&buf, base);
        let got = read(&mut r);
        (got, r.position())
    }

    #[test]
    fn every_width_succeeds_at_exact_size_and_fails_one_byte_short() {
        // (width, read) table: a one-byte-short buffer must fail with the exact EOF fields,
        // an exact-size buffer must succeed and consume everything.
        type Probe = fn(&mut Reader<'_>) -> Result<()>;
        let table: [(usize, Probe); 9] = [
            (1, |r| r.read_u8().map(drop)),
            (2, |r| r.read_u16().map(drop)),
            (4, |r| r.read_u32().map(drop)),
            (8, |r| r.read_u64().map(drop)),
            (16, |r| r.read_u128().map(drop)),
            (4, |r| r.read_i32().map(drop)),
            (8, |r| r.read_i64().map(drop)),
            (3, |r| r.read_array::<3>().map(drop)),
            (32, |r| r.read_array::<32>().map(drop)),
        ];
        for (width, read) in table {
            let (ok, pos) = try_width(width, 7, read);
            assert_eq!(ok, Ok(()), "width {width} exact");
            assert_eq!(pos, width, "width {width} must consume exactly its size");

            let short = width - 1;
            let (err, pos) = try_width(short, 7, read);
            assert_eq!(
                err,
                Err(eof_at(7, width as u64, short as u64)),
                "width {width} short"
            );
            assert_eq!(pos, 0, "width {width}: failed read must not advance");
        }
    }

    #[test]
    fn extreme_values_decode_little_endian_for_every_width() {
        // Asymmetric patterns catch byte-order swaps that symmetric values (0, MAX) would hide.
        let mut bytes = Vec::new();
        bytes.extend(u16::MAX.to_le_bytes());
        bytes.extend(0x8001_u16.to_le_bytes());
        bytes.extend(i32::MIN.to_le_bytes());
        bytes.extend(i32::MAX.to_le_bytes());
        bytes.extend((-1_i64).to_le_bytes());
        bytes.extend(i64::MAX.to_le_bytes());
        bytes.extend(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff_u128.to_le_bytes());
        bytes.extend(1_u64.to_le_bytes());
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_u16(), Ok(u16::MAX));
        assert_eq!(r.read_u16(), Ok(0x8001));
        assert_eq!(r.read_i32(), Ok(i32::MIN));
        assert_eq!(r.read_i32(), Ok(i32::MAX));
        assert_eq!(r.read_i64(), Ok(-1));
        assert_eq!(r.read_i64(), Ok(i64::MAX));
        assert_eq!(r.read_u128(), Ok(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff));
        assert_eq!(r.read_u64(), Ok(1));
        assert!(r.is_empty());
    }

    #[test]
    fn explicit_byte_order_for_each_width() {
        let seq: Vec<u8> = (1..=16).collect();
        assert_eq!(Reader::new(&seq).read_u16(), Ok(0x0201));
        assert_eq!(Reader::new(&seq).read_u64(), Ok(0x0807_0605_0403_0201));
        assert_eq!(
            Reader::new(&seq).read_u128(),
            Ok(0x100f_0e0d_0c0b_0a09_0807_0605_0403_0201)
        );
        assert_eq!(Reader::new(&[0xfe, 0xff, 0xff, 0xff]).read_i32(), Ok(-2));
        assert_eq!(
            Reader::new(&[0, 0, 0, 0, 0, 0, 0, 0x80]).read_i64(),
            Ok(i64::MIN)
        );
        assert_eq!(Reader::new(&seq).read_array::<3>(), Ok([1, 2, 3]));
    }

    #[test]
    fn interleaved_reads_hit_eof_exactly_at_boundary() {
        // 1 + 2 + 4 + 8 = 15 bytes: the last read ends exactly at the end of the buffer.
        let buf: Vec<u8> = (0..15).collect();
        let mut r = Reader::with_base_offset(&buf, 100);
        assert_eq!(r.read_u8(), Ok(0));
        assert_eq!(r.read_u16(), Ok(0x0201));
        assert_eq!(r.read_bytes(4), Ok(&buf[3..7]));
        assert_eq!(
            r.read_u64(),
            Ok(u64::from_le_bytes(buf[7..15].try_into().unwrap()))
        );
        assert_eq!(r.position(), 15);
        assert_eq!(r.remaining(), 0);
        assert!(r.is_empty());
        // Every further non-empty read fails at the absolute end, with nothing available.
        assert_eq!(r.read_u8(), Err(eof_at(115, 1, 0)));
        assert_eq!(r.read_bytes(1), Err(eof_at(115, 1, 0)));
        assert_eq!(r.read_array::<2>(), Err(eof_at(115, 2, 0)));
        assert_eq!(r.skip(usize::MAX), Err(eof_at(115, u64::MAX, 0)));
        // Zero-length reads at the end still succeed and do not move.
        assert_eq!(r.read_bytes(0), Ok(&[][..]));
        assert_eq!(r.read_array::<0>(), Ok([]));
        assert_eq!(r.position(), 15);
        assert_eq!(r.offset(), 115);
    }

    #[test]
    fn read_bytes_succeeds_at_remaining_and_fails_one_past() {
        let buf = [1, 2, 3, 4, 5, 6];
        let mut r = Reader::new(&buf);
        r.skip(2).unwrap();
        assert_eq!(r.read_bytes(5), Err(eof_at(2, 5, 4)));
        assert_eq!(r.skip(5), Err(eof_at(2, 5, 4)));
        assert_eq!(r.position(), 2);
        assert_eq!(r.read_bytes(4), Ok(&buf[2..]));
        assert!(r.is_empty());
    }

    #[test]
    fn rest_after_full_consume_is_empty_and_points_at_end() {
        let buf = [1, 2, 3];
        let mut r = Reader::new(&buf);
        assert_eq!(r.rest(), &buf);
        assert!(core::ptr::eq(r.rest().as_ptr(), buf.as_ptr()));
        r.skip(3).unwrap();
        assert_eq!(r.rest(), &[] as &[u8]);
        assert!(core::ptr::eq(r.rest().as_ptr(), buf.as_ptr_range().end));
        assert_eq!(r.read_bytes(0).unwrap().len(), 0);
    }

    #[test]
    fn rest_does_not_advance_and_tracks_position() {
        let buf = [1, 2, 3, 4];
        let mut r = Reader::new(&buf);
        r.skip(1).unwrap();
        assert_eq!(r.rest(), &buf[1..]);
        assert_eq!(r.rest(), &buf[1..]);
        assert_eq!(r.position(), 1);
        assert_eq!(r.remaining(), r.rest().len());
    }

    #[test]
    fn read_bytes_returns_slices_into_the_input_for_whole_buffer_and_tail() {
        let buf = [1, 2, 3, 4, 5];
        // The borrows outlive the reader (lifetime is tied to the input, not to the cursor).
        let (head, tail) = {
            let mut r = Reader::new(&buf);
            (r.read_bytes(2).unwrap(), r.read_bytes(3).unwrap())
        };
        assert!(core::ptr::eq(head.as_ptr(), buf.as_ptr()));
        assert!(core::ptr::eq(tail.as_ptr(), buf[2..].as_ptr()));
        assert_eq!((head, tail), (&buf[..2], &buf[2..]));
    }

    #[test]
    fn offset_is_base_plus_position_and_position_ignores_base() {
        let buf = [0; 10];
        let mut r = Reader::with_base_offset(&buf, 4096);
        assert_eq!((r.position(), r.offset()), (0, 4096));
        r.read_u32().unwrap();
        assert_eq!((r.position(), r.offset()), (4, 4100));
        r.skip(6).unwrap();
        assert_eq!((r.position(), r.offset()), (10, 4106));

        let plain = Reader::new(&buf);
        assert_eq!(plain.offset(), 0);
        assert_eq!(plain.offset(), plain.position() as u64);
    }

    #[test]
    fn every_failure_kind_reports_base_relative_offset() {
        let buf = [0; 5];
        let mut r = Reader::with_base_offset(&buf, 1_000_000);
        r.skip(3).unwrap();
        let want = |needed| Err(eof_at(1_000_003, needed, 2));
        assert_eq!(r.read_u32().map(drop), want(4));
        assert_eq!(r.read_i64().map(drop), want(8));
        assert_eq!(r.read_u128().map(drop), want(16));
        assert_eq!(r.read_array::<3>().map(drop), want(3));
        assert_eq!(r.read_bytes(3).map(drop), want(3));
        assert_eq!(r.skip(3), want(3));
        assert_eq!(r.position(), 3);
    }

    #[test]
    fn base_offset_at_max_saturates_from_the_start() {
        let mut r = Reader::with_base_offset(&[7], u64::MAX);
        assert_eq!(r.offset(), u64::MAX);
        assert_eq!(r.read_u16(), Err(eof_at(u64::MAX, 2, 1)));
        assert_eq!(r.read_u8(), Ok(7));
        assert_eq!(r.offset(), u64::MAX);
        assert_eq!(r.position(), 1);
    }

    #[test]
    fn position_never_exceeds_len_across_mixed_success_and_failure() {
        let buf: Vec<u8> = (0..13).collect();
        let mut r = Reader::new(&buf);
        let lens = [0, 3, usize::MAX, 4, 8, 5, 1, 1, 2, usize::MAX / 2, 0, 1];
        let mut expected = 0usize;
        for len in lens {
            let before = r.position();
            let ok = r.skip(len).is_ok();
            if ok {
                expected += len;
            }
            assert_eq!(ok, len <= buf.len() - before, "skip({len}) at {before}");
            assert_eq!(r.position(), expected);
            assert!(r.position() <= buf.len());
            assert_eq!(r.remaining(), buf.len() - r.position());
            assert_eq!(r.is_empty(), r.position() == buf.len());
        }
        assert_eq!(r.position(), buf.len());
    }

    #[test]
    fn clone_is_an_independent_cursor() {
        let buf = [1, 2, 3, 4];
        let mut a = Reader::with_base_offset(&buf, 10);
        a.skip(1).unwrap();
        let mut b = a.clone();
        assert_eq!(b.read_u16(), Ok(0x0302));
        assert_eq!(a.position(), 1);
        assert_eq!(a.offset(), 11);
        assert_eq!(b.offset(), 13);
        assert_eq!(a.read_u8(), Ok(2));
    }

    #[test]
    fn corrupt_and_eof_errors_compare_by_all_fields() {
        // Guards the `PartialEq` that every other test relies on: differing fields must not compare equal.
        assert_ne!(eof_at(1, 2, 3), eof_at(0, 2, 3));
        assert_ne!(eof_at(1, 2, 3), eof_at(1, 4, 3));
        assert_ne!(eof_at(1, 2, 3), eof_at(1, 2, 0));
    }
}
