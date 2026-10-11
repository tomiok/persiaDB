//! Read-only memory mapping of an immutable file range (ADR-0010).
//!
//! [`MmapFile::map`] is an `unsafe fn`: whether a range is immutable is a whole-program property (SPEC §4.7) that
//! only its caller can vouch for, so each call site states why in a `// SAFETY:` comment. The one `unsafe` block
//! here wraps memmap2. A fault on a mapped page (a read error, a truncation by another program) raises `SIGBUS` on
//! Unix (`EXCEPTION_IN_PAGE_ERROR` on Windows), which ends the process; callers that cannot accept that read with
//! `pread` instead.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::ops::{Deref, Range};

use memmap2::{Mmap, MmapOptions};

/// A read-only mapping of bytes `offset..offset + len` of a file.
///
/// The mapping stays valid after the [`File`] is closed. It is `Send + Sync` and cheap to share through
/// [`crate::source::ByteSource::from_mmap`].
///
/// ```
/// # fn main() -> std::io::Result<()> {
/// # if cfg!(miri) { return Ok(()); } // Miri has no file-backed mmap (ADR-0010)
/// use std::io::Write;
/// use persia_format::mmap::MmapFile;
///
/// let mut file = tempfile::tempfile()?;
/// file.write_all(b"header..frames")?;
/// // SAFETY: an unnamed temporary file that nothing else can open, and this example never writes it again.
/// let map = unsafe { MmapFile::map(&file, 8..14)? };
/// assert_eq!(map.offset(), 8);
/// assert_eq!(&*map, b"frames");
/// // SAFETY: as above.
/// assert!(unsafe { MmapFile::map(&file, 8..15) }.is_err()); // past the end of the file
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct MmapFile {
    map: Mmap,
    offset: u64,
}

impl MmapFile {
    /// Maps bytes `range` (absolute offsets) of `file` read-only.
    ///
    /// # Safety
    /// For as long as the mapping (or any [`crate::source::ByteSource`] made from it) lives, no byte in `range` may
    /// be written, and the file may not be truncated or hole-punched below `range.end`, by this or any other
    /// process. For a container, SPEC §4.7 says which ranges qualify. Writing mapped bytes changes memory behind a
    /// live `&[u8]` (undefined behavior); truncating them makes the next access fault.
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidInput`] if `range` is inverted, longer than `isize::MAX`, or ends past the file's
    /// current length; otherwise the error from `fstat` or `mmap`.
    pub unsafe fn map(file: &File, range: Range<u64>) -> io::Result<Self> {
        let Range { start, end } = range;
        let file_len = file.metadata()?.len();
        let len = end
            .checked_sub(start)
            .filter(|_| end <= file_len)
            .and_then(|len| isize::try_from(len).ok())
            .and_then(|len| usize::try_from(len).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("cannot map {start}..{end} of a {file_len}-byte file"),
                )
            })?;
        // SAFETY: the mapping is read-only, and `start..end` lies within the file's current length, so no byte of
        // the slice `Deref` hands out lies past EOF. (memmap2 aligns `start` down to a page and maps at least one
        // byte, so an empty range may map a page past EOF, but an empty slice never touches it.) That those bytes
        // never change or disappear while mapped is this function's own safety precondition.
        let map = unsafe { MmapOptions::new().offset(start).len(len).map(file)? };
        Ok(Self { map, offset: start })
    }

    /// Absolute offset in the file of the first mapped byte.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The mapped bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.map
    }
}

impl Deref for MmapFile {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for MmapFile {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn map(file: &File, range: Range<u64>) -> io::Result<MmapFile> {
        // SAFETY: every test maps an unnamed temporary file of its own and only appends past the mapped range.
        unsafe { MmapFile::map(file, range) }
    }

    fn file_with(bytes: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    #[allow(clippy::cast_possible_truncation)] // test offsets are small
    fn maps_exactly_the_requested_range() {
        let data: Vec<u8> = (0..=255u8).cycle().take(3 * 4096 + 17).collect();
        let file = file_with(&data);
        let len = data.len() as u64;
        // Unaligned starts and ends, a whole-file range, and ranges touching either end.
        for (start, end) in [
            (0, len),
            (1, 2),
            (4095, 4097),
            (4096, 8192),
            (12287, len),
            (5, 5),
            (len, len),
        ] {
            let map = map(&file, start..end).unwrap();
            assert_eq!(map.offset(), start);
            assert_eq!(&*map, &data[start as usize..end as usize], "{start}..{end}");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    fn rejects_ranges_outside_the_file() {
        let file = file_with(&[7; 100]);
        #[allow(clippy::reversed_empty_ranges)]
        for range in [
            0..101,
            100..101,
            101..101,
            50..10,
            u64::MAX..u64::MAX,
            0..u64::MAX,
        ] {
            let err = map(&file, range.clone()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{range:?}");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    fn empty_ranges_map_nothing() {
        // memmap2 maps at least one byte, so an empty range at a page-aligned EOF maps a page past it; the slice
        // stays empty and is never touched.
        for (file_len, start) in [(0, 0), (4096, 4096), (8192, 4096), (100, 100)] {
            let file = file_with(&vec![1; file_len]);
            let map = map(&file, start..start).unwrap();
            assert!(map.is_empty());
            assert_eq!(map.offset(), start);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    fn appends_after_the_range_do_not_change_it() {
        // The writer keeps appending frames past a reader's `log_end` (SPEC §4.7).
        let mut file = file_with(b"committed");
        let map = map(&file, 0..9).unwrap();
        file.write_all(&[0xAB; 10_000]).unwrap();
        assert_eq!(&*map, b"committed");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    fn outlives_the_file_handle() {
        let map = map(&file_with(b"abc"), 1..3).unwrap();
        assert_eq!(map.as_ref(), b"bc");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    fn mapped_source_slices_and_outlives_the_mapping_handle() {
        let data: Vec<u8> = (0..=255u8).cycle().take(20_000).collect();
        let file = file_with(&data);
        let map = map(&file, 12_288..20_000).unwrap();
        let s = crate::source::ByteSource::from(map);
        drop(file);
        assert_eq!(s.base_offset(), 12_288);
        assert_eq!(s.end_offset(), 20_000);
        let sub = s.slice(13_000..13_008).unwrap();
        drop(s);
        assert_eq!(&*sub, &data[13_000..13_008]);
        let mut r = sub.reader();
        assert_eq!(
            r.read_u64().unwrap(),
            u64::from_le_bytes(data[13_000..13_008].try_into().unwrap())
        );
        let err = r.read_u8().unwrap_err();
        assert_eq!(err.corruption().map(|(offset, _)| offset), Some(13_008));
    }
}
