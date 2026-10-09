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

    /// Appends `len` zero bytes (reserved fields, SPEC §4.3/§4.4); the counterpart of [`crate::Reader::expect_zeros`].
    pub fn write_zeros(&mut self, len: usize) {
        self.buf.resize(self.buf.len().saturating_add(len), 0);
    }

    /// Appends zero bytes until the absolute offset is a multiple of `align` (no-op if already aligned).
    ///
    /// Only with a base near `u64::MAX` (unreachable for real files) does the saturated offset stop advancing,
    /// so each call would add another byte.
    pub fn pad_to(&mut self, align: NonZeroUsize) {
        self.write_zeros(padding_for(self.offset(), align));
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
    use crate::{ALIGNMENT, Corruption, Error, Reader};

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

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
    fn write_zeros_round_trips_through_expect_zeros() {
        let mut w = Writer::new();
        w.write_u8(1);
        w.write_zeros(4);
        w.write_zeros(0);
        w.write_u8(2);
        assert_eq!(w.as_slice(), &[1, 0, 0, 0, 0, 2]);
        let mut r = Reader::new(w.as_slice());
        assert_eq!(r.read_u8(), Ok(1));
        assert_eq!(r.expect_zeros(4), Ok(()));
        assert_eq!(r.read_u8(), Ok(2));
    }

    #[test]
    fn empty_writer() {
        let w = Writer::with_base_offset(16);
        assert!(w.is_empty());
        assert_eq!(w.offset(), 16);
        assert_eq!(w.into_inner(), Vec::<u8>::new());
    }
    #[test]
    fn every_width_round_trips_through_reader() {
        // Extremes plus asymmetric patterns, interleaved with padding at an unaligned base (invariant 1).
        let u16s = [0, 1, 0x1234, u16::MAX];
        let u32s = [0, 1, 0x0102_0304, u32::MAX];
        let u64s = [0, 1, 0x0102_0304_0506_0708, u64::MAX];
        let u128s = [0, 1, 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10, u128::MAX];
        let i32s = [i32::MIN, -1, 0, 1, i32::MAX];
        let i64s = [i64::MIN, -1, 0, 1, i64::MAX];
        let blob = [0xde, 0xad, 0xbe];

        for base in [0_u64, 1, 3, 4093] {
            let mut w = Writer::with_base_offset(base);
            for v in [0_u8, 0x7f, 0x80, u8::MAX] {
                w.write_u8(v);
            }
            w.pad_to(ALIGNMENT);
            for &v in &u16s {
                w.write_u16(v);
            }
            for &v in &u32s {
                w.write_u32(v);
            }
            w.write_bytes(&blob);
            w.write_bytes(&[]);
            w.pad_to(ALIGNMENT);
            for &v in &u64s {
                w.write_u64(v);
            }
            for &v in &u128s {
                w.write_u128(v);
            }
            for &v in &i32s {
                w.write_i32(v);
            }
            for &v in &i64s {
                w.write_i64(v);
            }
            w.pad_to(ALIGNMENT);
            let end = w.offset();
            let bytes = w.into_inner();

            let mut r = Reader::with_base_offset(&bytes, base);
            for v in [0_u8, 0x7f, 0x80, u8::MAX] {
                assert_eq!(r.read_u8(), Ok(v));
            }
            r.skip_padding(ALIGNMENT).unwrap();
            for &v in &u16s {
                assert_eq!(r.read_u16(), Ok(v));
            }
            for &v in &u32s {
                assert_eq!(r.read_u32(), Ok(v));
            }
            assert_eq!(r.read_bytes(blob.len()), Ok(&blob[..]));
            r.skip_padding(ALIGNMENT).unwrap();
            for &v in &u64s {
                assert_eq!(r.read_u64(), Ok(v));
            }
            for &v in &u128s {
                assert_eq!(r.read_u128(), Ok(v));
            }
            for &v in &i32s {
                assert_eq!(r.read_i32(), Ok(v));
            }
            for &v in &i64s {
                assert_eq!(r.read_i64(), Ok(v));
            }
            r.skip_padding(ALIGNMENT).unwrap();
            assert!(
                r.is_empty(),
                "base={base}: reader must consume exactly what was written"
            );
            assert_eq!(r.offset(), end);
            assert_eq!(end % 8, 0);
        }
    }

    #[test]
    fn pad_to_and_skip_padding_agree_for_any_base_and_prefix() {
        // Invariants 2 and 3: for every base residue (including near u64::MAX), prefix length and alignment,
        // the writer pads with zeros to an absolute boundary and the reader skips exactly those bytes.
        // Under Miri (which checks for UB, irrelevant to this logic) a smaller grid keeps the CI job fast.
        let (max_residue, max_prefix) = if cfg!(miri) { (8, 9) } else { (17, 17) };
        let bases = (0..=max_residue).chain([4093, 1 << 40, u64::MAX - 40]);
        for base in bases {
            for align in [1, 2, 3, 4, 8, 16] {
                for prefix in 0..=max_prefix {
                    let ctx = format!("base={base} align={align} prefix={prefix}");
                    let mut w = Writer::with_base_offset(base);
                    w.write_bytes(&vec![0xff; prefix]);
                    w.pad_to(nz(align));
                    let pad = w.len() - prefix;
                    assert!(pad < align, "{ctx}: pad={pad}");
                    assert_eq!(w.offset() % align as u64, 0, "{ctx}: not aligned");
                    assert_eq!(pad, padding_for(base + prefix as u64, nz(align)), "{ctx}");
                    assert!(
                        w.as_slice()[prefix..].iter().all(|&b| b == 0),
                        "{ctx}: padding must be zeros"
                    );
                    w.write_u8(0xa5);
                    let bytes = w.into_inner();

                    let mut r = Reader::with_base_offset(&bytes, base);
                    r.skip(prefix).unwrap();
                    assert_eq!(r.skip_padding(nz(align)), Ok(()), "{ctx}");
                    assert_eq!(r.position(), prefix + pad, "{ctx}");
                    assert_eq!(r.read_u8(), Ok(0xa5), "{ctx}");
                    assert!(r.is_empty(), "{ctx}");
                }
            }
        }
    }

    #[test]
    fn pad_to_is_idempotent_and_padding_only_grows_to_the_next_boundary() {
        for base in [0_u64, 1, 6, 7, 9, 4095] {
            let mut w = Writer::with_base_offset(base);
            w.write_u8(1);
            w.pad_to(ALIGNMENT);
            let once = w.as_slice().to_vec();
            w.pad_to(ALIGNMENT);
            w.pad_to(nz(4)); // a divisor of the current alignment: also a no-op
            w.pad_to(NonZeroUsize::MIN);
            assert_eq!(w.as_slice(), once.as_slice(), "base={base}");
        }
    }

    #[test]
    fn corrupting_any_padding_byte_of_writer_output_is_detected() {
        // Invariant 4 end to end: flip each padding byte the writer produced and decode at the same base.
        let base = 3_u64;
        let mut w = Writer::with_base_offset(base);
        w.write_u16(0xbeef); // offset 5: 3 padding bytes to 8
        w.pad_to(ALIGNMENT);
        w.write_u64(42);
        let good = w.into_inner();
        for i in 2..5 {
            let mut bad = good.clone();
            bad[i] = 0x40;
            let mut r = Reader::with_base_offset(&bad, base);
            assert_eq!(r.read_u16(), Ok(0xbeef));
            assert_eq!(
                r.skip_padding(ALIGNMENT),
                Err(Error::Corrupt {
                    offset: base + i as u64,
                    reason: Corruption::NonZeroReserved { value: 0x40 },
                }),
                "padding byte {i}"
            );
            assert_eq!(r.position(), 2, "failed skip_padding must not advance");
        }
    }

    #[test]
    fn output_is_deterministic() {
        let build = || {
            let mut w = Writer::with_base_offset(11);
            w.write_i32(-7);
            w.pad_to(ALIGNMENT);
            w.write_u128(u128::MAX - 1);
            w.into_inner()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn len_offset_and_is_empty_track_writes() {
        let mut w = Writer::with_base_offset(100);
        w.write_bytes(&[]);
        assert!(w.is_empty());
        assert_eq!((w.len(), w.offset()), (0, 100));
        w.write_u16(1);
        assert!(!w.is_empty());
        assert_eq!((w.len(), w.offset()), (2, 102));
        w.pad_to(ALIGNMENT);
        assert_eq!((w.len(), w.offset()), (4, 104));
        w.write_u128(0);
        assert_eq!((w.len(), w.offset()), (20, 120));
        assert_eq!(w.as_slice().len(), w.len());
    }
}
