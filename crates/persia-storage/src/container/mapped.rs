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
/// on that file. That is half of what makes the mapping sound; the other half is that nothing outside Persia writes
/// the file while it is mapped (see the `SAFETY` comment).
pub(super) fn map_committed(file: &File, sb: &Superblock) -> io::Result<ByteSource> {
    let range = FIRST_FRAME_OFFSET..sb.log_end.max(FIRST_FRAME_OFFSET);
    // SAFETY: SPEC §4.7. `sb` is a superblock this handle read or committed on `file`, so `12288..log_end` holds
    // frames only. The header and superblock pages, which are rewritten in place, are below 12288: the OS view may
    // include them (memmap2 aligns its start down to the page or, on Windows, the 64 KiB allocation granularity),
    // but the slice handed out starts at 12288, so no reference ever covers them. Frames are append-only (§4.4), and
    // no Persia writer truncates, hole-punches or rewrites the file below a `log_end` it opened with or committed:
    // recovery truncates only after `log_end` (§8.6), compaction writes a new file and renames it over this one
    // (§4.6), and create never truncates (§4.5.1). The writer lock keeps a second writer out (§4.5.1). Outside the
    // contract (ADR-0010 decision 3), as for any mmap-based database: other programs writing the file, other
    // handles to it in this process (e.g. a `try_clone` of the `File` given to `open_on`) writing or truncating it
    // outside the container API, and a slot damaged after this handle read it. A truncation then faults and ends the
    // process (decision 5); a write changes bytes behind a live `&[u8]`, which the embedder must never do.
    let map = unsafe { MmapFile::map(file, range)? };
    Ok(ByteSource::from_mmap(map))
}

impl Container<File> {
    /// Reads committed frames through a memory mapping from now on, instead of with `pread` (SPEC §4.7, ADR-0017).
    ///
    /// Maps `12288..log_end` once, and again after each [`commit`](Container::commit) and in-place
    /// [`compact`](Container::compact). [`read_frame`](Container::read_frame) and
    /// [`read_frame_source`](Container::read_frame_source) take a frame from the mapping when it covers the frame,
    /// and read the others with `pread`: frames after `log_end`, frames committed after a failed remap (the previous
    /// mapping stays), and every frame if mapping the file a compaction moved to failed. Calling it again maps anew.
    ///
    /// Opt in only on a local disk file system: a read error on a mapped page, or a truncation of the file by a
    /// program other than Persia, raises `SIGBUS` (Unix) or `EXCEPTION_IN_PAGE_ERROR` (Windows) and ends the
    /// process, where `pread` would return an error. While a mapping (or a [`FrameSource`](super::FrameSource) from
    /// it) lives, nothing but Persia containers may write or truncate the file: not even other handles to it in this
    /// process. On Windows a live mapping also keeps another handle's read-write open from truncating a torn or
    /// corrupt tail: that open fails with [`Error::Io`] until the mapping is gone (possibly leaving a quarantine copy
    /// of the tail behind each time, ADR-0017). A live mapping does not block [`compact`](Container::compact)'s
    /// rename over the container on Windows 10 1607 and later on NTFS, where the rename has POSIX semantics (tested in
    /// CI, ADR-0016); on other Windows file systems (FAT, exFAT) the rename may be refused, and the compaction then
    /// fails without harm (SPEC §4.7).
    ///
    /// # Errors
    /// [`Error::Io`] if the mapping fails (e.g. no address space for a large container on a 32-bit target); reads
    /// then go on as before the call.
    pub fn enable_mapped_reads(&mut self) -> Result<()> {
        let window =
            map_committed(&self.file, &self.active.superblock).map_err(Error::io("map"))?;
        self.mapped = Some(Mapped {
            map: map_committed,
            window: Some(window),
        });
        Ok(())
    }

    /// Whether the container file is on a local disk file system (ADR-0020): Linux ext4, XFS, Btrfs and the other
    /// local types, macOS local volumes other than FUSE, Windows fixed and RAM disks. Network, FUSE and removable
    /// file systems, and those of other operating systems, are not.
    ///
    /// # Errors
    /// [`Error::Io`] if the file system cannot be queried.
    pub fn is_on_local_disk(&self) -> Result<bool> {
        crate::sys::is_local_disk(&self.file).map_err(Error::io("query file system"))
    }

    /// The automatic choice of mapped reads (SPEC §4.7, ROADMAP 2.3.10): [`enable_mapped_reads`](Container::enable_mapped_reads)
    /// if the container is [on a local disk](Container::is_on_local_disk), where a read error behind a page is as rare
    /// as one behind `pread`; `pread` otherwise, where a server or a FUSE process that fails would end this process
    /// with `SIGBUS`. Returns whether mapped reads are now on. The same contract as `enable_mapped_reads` applies:
    /// while a mapping lives, nothing but Persia containers may write or truncate the file, and on Windows a live
    /// mapping keeps another handle's read-write open from truncating a torn tail, so it suits read-only handles best.
    ///
    /// ```
    /// use persia_storage::container::{Container, Mode};
    ///
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// # drop(Container::create(&path)?);
    /// let mut c = Container::open(&path, Mode::ReadOnly)?;
    /// let mapped = c.enable_mapped_reads_if_local()?;
    /// assert_eq!(mapped, c.mapped_range().is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`Error::Io`] if the file system cannot be queried or the mapping fails; reads then go on as before the call.
    pub fn enable_mapped_reads_if_local(&mut self) -> Result<bool> {
        if !self.is_on_local_disk()? {
            return Ok(false);
        }
        self.enable_mapped_reads()?;
        Ok(true)
    }

    /// The absolute range of the file that frames are read from the mapping, if one is live.
    pub fn mapped_range(&self) -> Option<Range<u64>> {
        let window = self.mapped.as_ref()?.window.as_ref()?;
        Some(window.base_offset()..window.end_offset())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use persia_format::block::Compression;
    use persia_format::frame::FrameKind;

    use super::*;
    use crate::container::Commit;
    use crate::container::sim::SimFile;

    thread_local! {
        /// Makes the next calls of [`heap_map`] on this thread fail.
        static FAIL: Cell<bool> = const { Cell::new(false) };
    }

    /// A stand-in for [`map_committed`] on a simulated file: a copy of `12288..log_end` at its absolute offset.
    fn heap_map(file: &SimFile, sb: &Superblock) -> io::Result<ByteSource> {
        if FAIL.get() {
            return Err(io::Error::other("no address space"));
        }
        let bytes = file.bytes();
        let range =
            usize::try_from(FIRST_FRAME_OFFSET).unwrap()..usize::try_from(sb.log_end).unwrap();
        Ok(ByteSource::with_base_offset(
            bytes[range].to_vec().into(),
            FIRST_FRAME_OFFSET,
        ))
    }

    fn commit(c: &mut Container<SimFile>, payload: &[u8]) -> u64 {
        let wal = c
            .append(FrameKind::WalChunk, payload, Compression::None)
            .unwrap();
        c.sync().unwrap();
        let manifest = c
            .append(FrameKind::Manifest, b"m", Compression::None)
            .unwrap();
        c.commit(Commit {
            manifest,
            wal_start: wal.offset,
            fence_token: 0,
        })
        .unwrap();
        wal.offset
    }

    fn from_window(c: &Container<SimFile>, offset: u64) -> bool {
        let a = c.read_frame_source(offset, 1 << 20).unwrap().payload;
        let b = c.read_frame_source(offset, 1 << 20).unwrap().payload;
        a.as_ptr() == b.as_ptr()
    }

    #[test]
    fn a_failed_remap_keeps_the_previous_window() {
        FAIL.set(false);
        let mut c = Container::create_on(SimFile::new(), [1; 16]).unwrap();
        let old = commit(&mut c, b"first");
        c.mapped = Some(Mapped {
            map: heap_map,
            window: Some(heap_map(&c.file, &c.active.superblock).unwrap()),
        });
        let covered = F..c.superblock().log_end;
        FAIL.set(true);
        let new = commit(&mut c, b"second");
        FAIL.set(false);
        assert_eq!(
            c.mapped
                .as_ref()
                .unwrap()
                .window
                .as_ref()
                .map(|w| w.base_offset()..w.end_offset()),
            Some(covered)
        );
        assert!(from_window(&c, old));
        assert!(!from_window(&c, new));
        assert_eq!(
            &*c.read_frame_source(new, 1 << 20).unwrap().payload,
            b"second"
        );
        // The next commit that maps catches up.
        let newest = commit(&mut c, b"third");
        assert!(from_window(&c, new) && from_window(&c, newest));
    }

    #[test]
    fn a_failed_map_after_a_move_drops_the_window() {
        FAIL.set(false);
        let mut c = Container::create_on(SimFile::new(), [1; 16]).unwrap();
        commit(&mut c, b"first");
        let mut mapped = Mapped {
            map: heap_map,
            window: Some(heap_map(&c.file, &c.active.superblock).unwrap()),
        };
        FAIL.set(true);
        mapped.moved_to(&c.file, &c.active.superblock);
        FAIL.set(false);
        assert!(mapped.window.is_none());
        mapped.moved_to(&c.file, &c.active.superblock);
        assert!(mapped.window.is_some());
    }

    const F: u64 = FIRST_FRAME_OFFSET;
}
