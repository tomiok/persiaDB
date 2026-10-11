//! `ByteSource`: one cheaply cloneable buffer type for mapped, heap and cached remote bytes (ADR-0010).
//!
//! Decoders take `&[u8]` plus the absolute offset of its first byte ([`Reader::with_base_offset`],
//! [`crate::frame::FrameScanner::new`]); a `ByteSource` carries both, so the bytes can come from a memory mapping
//! ([`MmapFile`]), a heap buffer, or a block cache filled by ranged reads (SPEC §10.6) without the engine knowing.

use core::fmt;
use core::ops::{Deref, Range};

use bytes::Bytes;

use crate::error::{Error, Result, to_u64};
use crate::mmap::MmapFile;
use crate::reader::Reader;

/// Immutable bytes that start at an absolute offset in their file or object (SPEC §4.1).
///
/// Cloning and [`ByteSource::slice`] share the underlying buffer (and keep a mapping alive) without copying.
///
/// ```
/// use persia_format::source::ByteSource;
///
/// // E.g. a cached ranged read of bytes 4096..4104 of an object.
/// let src = ByteSource::with_base_offset(b"0123abcd".to_vec().into(), 4096);
/// let tail = src.slice(4100..4104)?;
/// assert_eq!(&*tail, b"abcd");
/// assert_eq!(tail.base_offset(), 4100);
/// assert_eq!(tail.reader().offset(), 4100); // error offsets stay absolute
/// assert!(src.slice(4100..4105).is_err()); // past the end of the buffer
/// # Ok::<(), persia_format::Error>(())
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ByteSource {
    bytes: Bytes,
    base: u64,
}

impl ByteSource {
    /// `bytes` as the start of their file or object (offset 0).
    pub fn new(bytes: Bytes) -> Self {
        Self::with_base_offset(bytes, 0)
    }

    /// `bytes` that start at absolute offset `base`, e.g. the result of a ranged read.
    pub fn with_base_offset(bytes: Bytes, base: u64) -> Self {
        Self { bytes, base }
    }

    /// The mapped bytes, at the mapping's offset. The mapping lives as long as any clone or slice.
    pub fn from_mmap(map: MmapFile) -> Self {
        let base = map.offset();
        Self::with_base_offset(Bytes::from_owner(map), base)
    }

    /// Absolute offset of the first byte.
    pub fn base_offset(&self) -> u64 {
        self.base
    }

    /// Absolute offset just past the last byte. Saturates at `u64::MAX`, which no real file reaches.
    pub fn end_offset(&self) -> u64 {
        self.base.saturating_add(to_u64(self.bytes.len()))
    }

    /// The bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Bytes `range` (absolute offsets), sharing this buffer.
    ///
    /// # Errors
    /// [`Error::RangeOutOfBounds`] if `range` is inverted or not within
    /// [`base_offset`](Self::base_offset)`..`[`end_offset`](Self::end_offset).
    pub fn slice(&self, range: Range<u64>) -> Result<Self> {
        let out_of_bounds = || Error::RangeOutOfBounds {
            start: range.start,
            end: range.end,
            available_start: self.base,
            available_end: self.end_offset(),
        };
        let rel = |offset: u64| {
            offset
                .checked_sub(self.base)
                .and_then(|o| usize::try_from(o).ok())
        };
        let (Some(start), Some(end)) = (rel(range.start), rel(range.end)) else {
            return Err(out_of_bounds());
        };
        if start > end || end > self.bytes.len() {
            return Err(out_of_bounds());
        }
        Ok(Self::with_base_offset(
            self.bytes.slice(start..end),
            range.start,
        ))
    }

    /// A [`Reader`] over the bytes whose error offsets are absolute.
    pub fn reader(&self) -> Reader<'_> {
        Reader::with_base_offset(&self.bytes, self.base)
    }

    /// The underlying buffer; the base offset is dropped.
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }
}

impl Deref for ByteSource {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for ByteSource {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl From<Bytes> for ByteSource {
    fn from(bytes: Bytes) -> Self {
        Self::new(bytes)
    }
}

impl From<Vec<u8>> for ByteSource {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes.into())
    }
}

impl From<MmapFile> for ByteSource {
    fn from(map: MmapFile) -> Self {
        Self::from_mmap(map)
    }
}

// The bytes can be a whole segment: show only where they are.
impl fmt::Debug for ByteSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ByteSource")
            .field("range", &(self.base..self.end_offset()))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::cast_possible_truncation)] // a repeating byte pattern
    fn src(len: usize, base: u64) -> (Vec<u8>, ByteSource) {
        let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let s = ByteSource::with_base_offset(data.clone().into(), base);
        (data, s)
    }

    #[test]
    fn accessors() {
        let (data, s) = src(10, 100);
        assert_eq!(s.base_offset(), 100);
        assert_eq!(s.end_offset(), 110);
        assert_eq!(s.len(), 10);
        assert_eq!(s.as_ref(), &data[..]);
        assert_eq!(s.clone().into_bytes(), Bytes::from(data.clone()));
        assert_eq!(ByteSource::from(data.clone()).base_offset(), 0);
        assert_eq!(ByteSource::from(Bytes::from(data)).end_offset(), 10);
        assert!(ByteSource::default().is_empty());
        assert_eq!(format!("{s:?}"), "ByteSource { range: 100..110, .. }");
    }

    #[test]
    fn end_offset_saturates() {
        let s = ByteSource::with_base_offset(vec![1, 2, 3].into(), u64::MAX - 1);
        assert_eq!(s.end_offset(), u64::MAX);
        assert_eq!(&*s.slice(u64::MAX - 1..u64::MAX).unwrap(), &[1]);
    }

    #[test]
    fn slice_errors_are_caller_errors_with_the_ranges() {
        let (_, s) = src(10, 100);
        #[allow(clippy::reversed_empty_ranges)]
        for range in [
            99..105,
            100..111,
            111..111,
            105..104,
            0..0,
            u64::MAX..u64::MAX,
        ] {
            let err = s.slice(range.clone()).unwrap_err();
            assert_eq!(
                err,
                Error::RangeOutOfBounds {
                    start: range.start,
                    end: range.end,
                    available_start: 100,
                    available_end: 110,
                }
            );
            assert!(!err.is_corruption());
        }
    }

    #[test]
    fn reader_reports_absolute_offsets() {
        let (_, s) = src(4, 4096);
        let sub = s.slice(4098..4100).unwrap();
        let mut r = sub.reader();
        assert_eq!(r.read_u16().unwrap(), u16::from_le_bytes([2, 3]));
        let err = r.read_u8().unwrap_err();
        assert_eq!(err.corruption().map(|(offset, _)| offset), Some(4100));
    }

    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    #[test]
    fn mapped_source_slices_and_outlives_the_mapping_handle() {
        use std::io::Write;

        let data: Vec<u8> = (0..=255u8).cycle().take(20_000).collect();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&data).unwrap();
        let s = ByteSource::from(MmapFile::map(&file, 12_288..20_000).unwrap());
        drop(file);
        assert_eq!(s.base_offset(), 12_288);
        assert_eq!(s.end_offset(), 20_000);
        let sub = s.slice(13_000..13_008).unwrap();
        drop(s);
        assert_eq!(&*sub, &data[13_000..13_008]);
    }
}
