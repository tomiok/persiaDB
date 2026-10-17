//! Mapped reads of a container's committed frames (SPEC §4.7, ADR-0010, ADR-0017): the only module outside
//! `persia-format` that calls [`MmapFile::map`].
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::ops::Range;

use persia_format::mmap::MmapFile;
use persia_format::source::ByteSource;
use persia_format::superblock::{FIRST_FRAME_OFFSET, Superblock};

use super::{Container, Error, Mapped, Result};

/// Maps the committed frames `12288..log_end` of the container in `file`, whose committed superblock is `sb`.
///
/// Private to the container: it is called only with the container's own file and a superblock it read or committed
/// on that file, which is what makes the mapping sound.
pub(super) fn map_committed(file: &File, sb: &Superblock) -> io::Result<ByteSource> {
    let range = FIRST_FRAME_OFFSET..sb.log_end.max(FIRST_FRAME_OFFSET);
    // SAFETY: SPEC §4.7. `sb` is a superblock this handle read or committed on `file`, so `12288..log_end` holds
    // frames only: the header and superblock pages, which are rewritten in place, lie below 12288. Frames are
    // append-only (§4.4), and no Persia writer truncates, hole-punches or rewrites the file below a `log_end` it
    // opened with or committed: recovery truncates only after `log_end` (§8.6), compaction writes a new file and
    // renames it over this one (§4.6), and create never truncates (§4.5.1). The writer lock keeps a second writer
    // out (§4.5.1). Other programs writing the file, and a slot damaged after this handle read it, are outside the
    // contract (ADR-0010 decision 3); a fault then ends the process (decision 5), it is never a silent misread.
    let map = unsafe { MmapFile::map(file, range)? };
    Ok(ByteSource::from_mmap(map))
}

impl Container<File> {
    /// Reads committed frames through a memory mapping from now on, instead of with `pread` (SPEC §4.7, ADR-0017).
    ///
    /// Maps `12288..log_end` once, and again after each [`commit`](Container::commit) and in-place
    /// [`compact`](Container::compact). [`read_frame`](Container::read_frame) and
    /// [`read_frame_source`](Container::read_frame_source) take a frame from the mapping when it covers the frame,
    /// and read the others (frames after `log_end`, or all of them if a remap failed) with `pread`.
    ///
    /// Opt in only on a local disk file system: a read error on a mapped page, or a truncation of the file by a
    /// program other than Persia, raises `SIGBUS` (Unix) or `EXCEPTION_IN_PAGE_ERROR` (Windows) and ends the
    /// process, where `pread` would return an error. On Windows a live mapping also keeps another handle's read-write
    /// open from truncating a torn tail: that open fails with [`Error::Io`] until the mapping is gone.
    ///
    /// # Errors
    /// [`Error::Io`] if the mapping fails (e.g. no address space for a large container on a 32-bit target); reads
    /// then go on with `pread`.
    pub fn enable_mapped_reads(&mut self) -> Result<()> {
        let window =
            map_committed(&self.file, &self.active.superblock).map_err(Error::io("map"))?;
        self.mapped = Some(Mapped {
            map: map_committed,
            window: Some(window),
        });
        Ok(())
    }

    /// The absolute range of the file that frames are read from the mapping, if one is live.
    pub fn mapped_range(&self) -> Option<Range<u64>> {
        let window = self.mapped.as_ref()?.window.as_ref()?;
        Some(window.base_offset()..window.end_offset())
    }
}
