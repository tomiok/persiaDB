//! Read-only memory mapping of an immutable file range: the crate's only `unsafe` (ADR-0010).
//!
//! Soundness rests on the contract in SPEC §4.7: Persia maps only bytes it never writes again (frames below a
//! read superblock's `log_end`, or other append-only, immutable files), and never truncates, hole-punches or
//! rewrites a file below what may be mapped. Another program modifying the file is outside that contract.
//! A disk error on a mapped page raises `SIGBUS`, which ends the process; callers that cannot accept that read
//! with `pread` instead (ADR-0010).
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
/// let map = MmapFile::map(&file, 8..14)?;
/// assert_eq!(map.offset(), 8);
/// assert_eq!(&*map, b"frames");
/// assert!(MmapFile::map(&file, 8..15).is_err()); // past the end of the file
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
    /// The caller upholds SPEC §4.7: no byte in `range` is written, and the file is not truncated or hole-punched
    /// below `range.end`, for as long as the mapping (or any [`crate::source::ByteSource`] made from it) lives.
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidInput`] if `range` is inverted, does not fit in `usize`, or ends past the file's
    /// current length; otherwise the error from `fstat` or `mmap`.
    pub fn map(file: &File, range: Range<u64>) -> io::Result<Self> {
        let Range { start, end } = range;
        let file_len = file.metadata()?.len();
        let len = end
            .checked_sub(start)
            .filter(|_| end <= file_len)
            .and_then(|len| usize::try_from(len).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("cannot map {start}..{end} of a {file_len}-byte file"),
                )
            })?;
        // SAFETY: the mapping is read-only, and `start..end` lies within the file's current length, so no page is
        // mapped past its end. The caller's contract (SPEC §4.7, ADR-0010) is that these bytes are never written,
        // truncated or hole-punched while mapped, so the `&[u8]` handed out by `Deref` never changes and never
        // faults on a missing page. memmap2 aligns `start` down to a page itself and hides the extra bytes.
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
            let map = MmapFile::map(&file, start..end).unwrap();
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
            let err = MmapFile::map(&file, range.clone()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{range:?}");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    fn empty_file_maps_empty_range() {
        let file = file_with(&[]);
        let map = MmapFile::map(&file, 0..0).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    fn appends_after_the_range_do_not_change_it() {
        // The writer keeps appending frames past a reader's `log_end` (SPEC §4.7).
        let mut file = file_with(b"committed");
        let map = MmapFile::map(&file, 0..9).unwrap();
        file.write_all(&[0xAB; 10_000]).unwrap();
        assert_eq!(&*map, b"committed");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    fn outlives_the_file_handle() {
        let map = MmapFile::map(&file_with(b"abc"), 1..3).unwrap();
        assert_eq!(map.as_ref(), b"bc");
    }
}
