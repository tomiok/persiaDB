//! Mapped reads of a container's committed frames (SPEC §4.7, ADR-0010, ADR-0017): the only module outside
//! `persia-format` that calls [`MmapFile::map`].
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::ops::Range;

use persia_format::mmap::MmapFile;
use persia_format::source::ByteSource;
use persia_format::superblock::{FIRST_FRAME_OFFSET, Superblock};

use super::{Container, Error, Mapped, Mode, Result};

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
    // (§4.6), and create never truncates (§4.5.1). The writer lock keeps a second writer out (§4.5.1), except where
    // the lock is per process (`flock` emulated with POSIX locks, e.g. Linux NFS): two read-write containers of one
    // process are then not excluded, and a second one appends at its own end over frames this handle mapped. Every
    // other writer, and that second container, is the caller's to exclude: this runs only after
    // `Container::enable_mapped_reads`, an `unsafe fn` whose `# Safety` contract rules out other programs, other
    // handles in this process writing, truncating or hole-punching the file below the mapped `log_end` while a
    // mapping lives, and a second read-write container where locks are per process (ADR-0010 decision 3). A slot
    // damaged after this handle read it stays outside any contract, as for any mmap-based database: recovery may then
    // fall back to the older slot and truncate below the mapped `log_end`, which faults and ends the process
    // (decision 5), and a writer's later appends rewrite bytes this handle still maps.
    let map = unsafe { MmapFile::map(file, range)? };
    Ok(ByteSource::from_mmap(map))
}

/// Permission to map Persia's append-only files outside a [`Container`]: the blob pack (`persia-blob`, SPEC §9.3,
/// ADR-0049), whose crate forbids `unsafe`. Only an `unsafe` call makes one, so the embedder states the mapping
/// contract once, as for [`Container::enable_mapped_reads`], and the holder maps with safe calls.
#[derive(Debug, Clone, Copy)]
pub struct MapPermit(());

impl MapPermit {
    /// The permit.
    ///
    /// # Safety
    /// For every blob pack mapped through this permit (by `persia_blob::PackStore::enable_mapped_reads`), while a
    /// mapping, or a `ByteSource` taken from one, is alive:
    /// - nothing but Persia writes, truncates or hole-punches the pack below the end of the mapped range: not other
    ///   programs, not other handles of the file in this process;
    /// - at most one read-write `PackStore` of the pack is open at a time, anywhere (in this process or another): a
    ///   second writer's open would truncate or append over frames this one mapped. The pack enforces it with its
    ///   writer lock, `<name>.persia.blobs.lock` (ROADMAP 9.2.15, ADR-0053); nothing else may write the pack.
    ///
    /// The permit is given only to Persia code that maps committed frames: bytes a Persia writer never writes again
    /// (SPEC §4.7).
    pub const unsafe fn new() -> Self {
        Self(())
    }

    /// Maps bytes `range` of `file` read-only.
    ///
    /// The caller maps only committed frames of a Persia file: bytes that no Persia writer writes, truncates or
    /// rewrites again (SPEC §4.7; for a blob pack, `12288..log_end` of a slot it read or committed, ADR-0049). A range
    /// that starts below 12288 would cover the header or a superblock slot, which are rewritten in place, and is
    /// refused.
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidInput`] for a range that starts below 12288 or ends past the file's end, or the error
    /// from `fstat` or `mmap`.
    // `self` is the proof the contract was accepted: holding a permit is what makes the call sound.
    #[allow(clippy::unused_self)]
    pub fn map(self, file: &File, range: Range<u64>) -> io::Result<ByteSource> {
        if range.start < FIRST_FRAME_OFFSET {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cannot map {}..{}: bytes below {FIRST_FRAME_OFFSET} are rewritten in place",
                    range.start, range.end
                ),
            ));
        }
        // SAFETY: the permit exists, so its maker vouched for `MapPermit::new`'s contract: nothing outside Persia
        // changes `range` while it is mapped, and at most one pack writer exists. `range` starts at or after 12288, so
        // it covers no header or slot page, which are rewritten in place. The caller maps only committed frames (this
        // function's rule), which no Persia writer changes: frames are append-only, recovery truncates only after
        // `log_end`, and a rewrite writes a new file and renames it over the old one (SPEC §4.6, §4.7, §9.3).
        let map = unsafe { MmapFile::map(file, range)? };
        Ok(ByteSource::from_mmap(map))
    }
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
    /// Opt in only on a local disk file system: a read error on a mapped page raises `SIGBUS` (Unix) or
    /// `EXCEPTION_IN_PAGE_ERROR` (Windows) and ends the process, where `pread` would return an error. On Windows a
    /// live mapping also keeps another handle's read-write open from truncating a torn or corrupt tail: that open
    /// fails with [`Error::Io`] until the mapping is gone (possibly leaving a quarantine copy of the tail behind each
    /// time, ADR-0017). A live mapping does not block [`compact`](Container::compact)'s rename over the container on
    /// Windows 10 1607 and later on NTFS, where the rename has POSIX semantics (tested in CI, ADR-0016); on other
    /// Windows file systems (FAT, exFAT) the rename may be refused, and the compaction then fails without harm
    /// (SPEC §4.7).
    ///
    /// # Safety
    /// The mapping hands out `&[u8]` slices of the file (ADR-0010, ADR-0017). While a mapping this container makes,
    /// or a [`FrameSource`](super::FrameSource) or `ByteSource` taken from one, is alive, the caller must ensure that
    /// nothing but Persia containers writes, truncates or hole-punches the file below the `log_end` it was mapped at:
    /// not other programs, and not other handles to the file in this process (a `try_clone` of the `File` given to
    /// [`Container::open_on`], a second `File::open`, a [`ContainerFile`](super::ContainerFile) call on either).
    /// Persia's own containers keep to this (SPEC §4.7), with two exceptions the caller must also rule out:
    /// - where the writer lock belongs to the process rather than the handle (`flock` emulated with POSIX locks:
    ///   Linux NFS, some FUSE and CIFS mounts; SPEC §4.5.1), a second read-write container on the file in this
    ///   process is not locked out, and it appends over frames this one mapped. Open at most one read-write
    ///   container per file per process there, or map only on a local disk
    ///   ([`enable_mapped_reads_if_local`](Container::enable_mapped_reads_if_local));
    /// - a superblock slot damaged after this container read it may make recovery fall back to the older slot and
    ///   truncate below the mapped `log_end`; a writer's later appends then rewrite mapped bytes. As for any
    ///   mmap-based database, this is outside what Persia can rule out (ADR-0010 decision 3).
    ///
    /// Otherwise frames are append-only, recovery truncates only after `log_end`, and compaction renames a new file
    /// over the old one. Appending after `log_end` lies outside the mapping and is allowed. Breaking the contract is
    /// undefined behavior (bytes changing behind a live `&[u8]`) or, for a truncation, ends the process with
    /// `SIGBUS` / `EXCEPTION_IN_PAGE_ERROR`.
    ///
    /// # Errors
    /// [`Error::Io`] if the mapping fails (e.g. no address space for a large container on a 32-bit target); reads
    /// then go on as before the call.
    pub unsafe fn enable_mapped_reads(&mut self) -> Result<()> {
        let window =
            map_committed(&self.file, &self.active.superblock).map_err(Error::io("map"))?;
        self.mapped = Some(Mapped::new(map_committed, window));
        Ok(())
    }

    /// Whether the container file is on a local disk file system (ADR-0021): Linux ext4, XFS, Btrfs and the other
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
    /// with `SIGBUS`. Returns whether this call mapped: `false` leaves an earlier `enable_mapped_reads` in place (see
    /// [`mapped_range`](Container::mapped_range)). On Windows a live mapping keeps another handle's read-write open
    /// from truncating a torn tail, so it suits read-only handles best.
    ///
    /// ```
    /// use persia_storage::container::{Container, Mode};
    ///
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// # drop(Container::create(&path)?);
    /// let mut c = Container::open(&path, Mode::ReadOnly)?;
    /// // SAFETY: nothing but Persia containers writes this file while it is mapped.
    /// let mapped = unsafe { c.enable_mapped_reads_if_local()? };
    /// assert_eq!(mapped, c.mapped_range().is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Safety
    /// As [`enable_mapped_reads`](Container::enable_mapped_reads), when this call maps.
    ///
    /// # Errors
    /// [`Error::Io`] if the mapping fails; reads then go on as before the call. A file system that cannot be queried
    /// (e.g. a Windows volume with no drive letter) counts as not local: `Ok(false)`, as ADR-0021 decides for every
    /// unknown answer.
    pub unsafe fn enable_mapped_reads_if_local(&mut self) -> Result<bool> {
        if !self.is_on_local_disk().unwrap_or(false) {
            return Ok(false);
        }
        // SAFETY: the caller keeps `enable_mapped_reads`' contract, this function's own.
        unsafe { self.enable_mapped_reads()? };
        Ok(true)
    }

    /// Maps this read-only container's committed frames if it is on a local disk: the default of read handles (SPEC
    /// §4.7, ROADMAP 10.2.4; ADR-0017 as amended 2026-11-23). Returns whether it mapped; `false` leaves the container
    /// reading with `pread`, as it does off the local file systems of [`is_on_local_disk`](Container::is_on_local_disk)
    /// (network, FUSE, overlay, removable, unknown), where a failed page read would end the process.
    ///
    /// Unlike [`enable_mapped_reads`](Container::enable_mapped_reads) this is a safe call: for a read-only handle on a
    /// local disk, Persia itself keeps the parts of the mapping contract that are about Persia. It keeps none of the
    /// part that is about everything else, which stays with whoever deploys the database:
    ///
    /// - **Nothing but Persia may write the file while it is mapped.** Another program, or another handle to the file
    ///   in this process that is not a Persia container, must not write, truncate or hole-punch it below its committed
    ///   `log_end`. A write changes bytes behind the mapping, which is undefined behavior in a Rust program; a
    ///   truncation ends the process with `SIGBUS` (Unix) or `EXCEPTION_IN_PAGE_ERROR` (Windows) at the next read of a
    ///   lost page. That is the residual risk of every database that maps its file. Where it cannot be excluded, do not
    ///   call this (the engine's read-only open has an option for ordinary reads).
    /// - A superblock slot damaged after this handle read it may make a writer's recovery truncate below the mapped
    ///   `log_end`, with the same end (ADR-0010 decision 5).
    ///
    /// A read-only handle never commits, so the mapping is made once and not again: a handle that follows a writer
    /// reopens the container ([`reopen_read_only`](Container::reopen_read_only)) and calls this on the new handle,
    /// which maps the frames committed since, or the file a compaction renamed over the path. On Windows the mapping
    /// keeps a read-write open elsewhere from truncating a torn or corrupt tail until it is gone (SPEC §4.7).
    ///
    /// ```
    /// use persia_storage::container::{Container, Mode};
    ///
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// # drop(Container::create(&path)?);
    /// let mut reader = Container::open(&path, Mode::ReadOnly)?;
    /// let mapped = reader.map_read_only_if_local()?;
    /// assert_eq!(mapped, reader.is_mapped());
    /// // The writer's handle is never mapped this way.
    /// let mut writer = Container::open(&path, Mode::ReadWrite)?;
    /// assert!(writer.map_read_only_if_local().is_err());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`Error::InvalidInput`] for a container opened read-write: a writer truncates torn tails and remaps at every
    /// commit, so it maps only through the `unsafe` opt-in. [`Error::Io`] if the mapping fails (no address space);
    /// reads then go on with `pread`. A file system that cannot be queried counts as not local: `Ok(false)`.
    pub fn map_read_only_if_local(&mut self) -> Result<bool> {
        let local = self.is_on_local_disk().unwrap_or(false);
        self.map_read_only(local)
    }

    /// [`map_read_only_if_local`](Container::map_read_only_if_local), given whether the file is on a local disk
    /// (ADR-0021's answer for this file, and nothing else: the safety argument below rests on it).
    fn map_read_only(&mut self, local: bool) -> Result<bool> {
        if self.mode() != Mode::ReadOnly {
            return Err(Error::InvalidInput(
                "only a read-only container maps by default: a read-write one opts in with enable_mapped_reads"
                    .into(),
            ));
        }
        if !local {
            return Ok(false);
        }
        // SAFETY: `enable_mapped_reads`' contract (ADR-0010 decision 3; ADR-0017, amended 2026-11-23) asks that
        // nothing writes, truncates or hole-punches the file below the mapped `log_end` while a mapping, or a
        // `ByteSource` taken from one, is alive. It has a part Persia vouches for here and a part it cannot.
        //
        // What Persia vouches for, which is why this function is safe to call:
        // 1. Persia's own writers, in any process. No Persia container truncates, hole-punches or rewrites a file
        //    below a `log_end` it opened with or committed: recovery truncates only past `log_end` (SPEC §8.6),
        //    compaction writes a new file and renames it over this one (§4.6), create never truncates (§4.5.1), and
        //    frames are append-only (§4.4).
        // 2. This handle. It is read-only (checked above, and a handle's mode never changes), so it never appends,
        //    commits, truncates or compacts. Its one mapping is the one made here, of `12288..log_end` of the
        //    superblock it read on this file (`map_committed`); nothing remaps it over a file this handle changed.
        // 3. A second writer where the lock is per process. `enable_mapped_reads` names that exception for file
        //    systems whose `flock` is emulated with POSIX locks (NFS, some FUSE and CIFS mounts), where two read-write
        //    containers of one process are not excluded and the second appends over frames the first committed.
        //    `local` is true only on the allow-listed local disk file systems (ADR-0021), where the writer lock
        //    belongs to the open file and keeps a second read-write container out, in this process or another.
        //
        // What Persia cannot vouch for, and what this function's documentation hands to whoever deploys it:
        // - a write, truncation or hole-punch below the mapped `log_end` by another program, or by another handle to
        //   the file in this process that is not a Persia container (a `File::open` of the path, a tool that rewrites
        //   the file in place). A write changes bytes behind a live `&[u8]`: undefined behavior. A truncation faults
        //   the next read of a lost page and ends the process. No check made here can exclude either. It is the
        //   residual every database that maps its file accepts (ADR-0010 decision 3), and ADR-0017's amendment
        //   accepts it for read-only handles on local disks; a deployment that cannot exclude it reads with `pread`.
        // - a superblock slot damaged after this handle read it: a writer's recovery may then fall back to the older
        //   slot and truncate below the mapped `log_end` (ADR-0010 decision 5), which ends this process the same way.
        unsafe { self.enable_mapped_reads()? };
        Ok(true)
    }

    /// The absolute range of the file that frames are read from the mapping, if one is live.
    pub fn mapped_range(&self) -> Option<Range<u64>> {
        let window = self.mapped.as_ref()?.window.as_ref()?;
        Some(window.base_offset()..window.end_offset())
    }
}

impl<F> Container<F> {
    /// Whether a mapping is live ([`Container::enable_mapped_reads`]): committed frames it covers are read from it
    /// (a new container's mapping covers none yet). `false` without mapped reads, and after mapping the file a
    /// compaction moved to failed: every frame is then read with `pread`.
    pub fn is_mapped(&self) -> bool {
        self.mapped.as_ref().is_some_and(|m| m.window.is_some())
    }

    /// How many times remapping failed since mapped reads were enabled: after a commit (the frames it added are then
    /// read with `pread`) or after a compaction (every frame is). Each failure also logs a `tracing` warning, since
    /// the fallback is otherwise invisible: reads stay correct, only slower. Enabling mapped reads again starts the
    /// count from 0.
    pub fn mapping_failures(&self) -> u64 {
        self.mapped.as_ref().map_or(0, |m| m.failures)
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
        c.mapped = Some(Mapped::new(
            heap_map,
            heap_map(&c.file, &c.active.superblock).unwrap(),
        ));
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
        assert!(c.is_mapped());
        assert_eq!(c.mapping_failures(), 1);
        let newest = commit(&mut c, b"third");
        assert!(from_window(&c, new) && from_window(&c, newest));
        assert_eq!(c.mapping_failures(), 1);
    }

    #[test]
    fn a_failed_map_after_a_move_drops_the_window() {
        FAIL.set(false);
        let mut c = Container::create_on(SimFile::new(), [1; 16]).unwrap();
        commit(&mut c, b"first");
        let mut mapped = Mapped::new(heap_map, heap_map(&c.file, &c.active.superblock).unwrap());
        FAIL.set(true);
        mapped.moved_to(&c.file, &c.active.superblock);
        FAIL.set(false);
        assert!(mapped.window.is_none());
        assert_eq!(mapped.failures, 1);
        mapped.moved_to(&c.file, &c.active.superblock);
        assert!(mapped.window.is_some());
        assert_eq!(mapped.failures, 1);
        c.mapped = Some(mapped);
        assert!(c.is_mapped());
    }

    /// What `map_read_only` does with the file system's answer: a read-write container is refused whatever it is, and
    /// a read-only one off a local disk maps nothing and reads on.
    #[test]
    fn a_read_only_container_off_a_local_disk_is_not_mapped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let mut w = Container::create(&path).unwrap();
        let wal = w
            .append(FrameKind::WalChunk, b"wal", Compression::None)
            .unwrap();
        w.sync().unwrap();
        let manifest = w
            .append(FrameKind::Manifest, b"m", Compression::None)
            .unwrap();
        w.commit(Commit {
            manifest,
            wal_start: wal.offset,
            fence_token: 0,
        })
        .unwrap();
        for local in [false, true] {
            assert!(matches!(
                w.map_read_only(local),
                Err(Error::InvalidInput(_))
            ));
            assert!(!w.is_mapped());
        }
        drop(w);
        let mut reader = Container::open(&path, Mode::ReadOnly).unwrap();
        assert!(!reader.map_read_only(false).unwrap());
        assert!(!reader.is_mapped());
        assert_eq!(reader.mapped_range(), None);
        assert_eq!(
            &*reader.read_frame(wal.offset, 1 << 20).unwrap().payload,
            b"wal"
        );
        // The same handle, told it is local, maps.
        assert!(reader.map_read_only(true).unwrap());
        assert_eq!(reader.mapped_range(), Some(F..reader.superblock().log_end));
    }

    const F: u64 = FIRST_FRAME_OFFSET;

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mmap (ADR-0010)")]
    fn a_permit_never_maps_the_header_or_the_slots() {
        use std::io::Write;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&[7; 12288 + 64]).unwrap();
        // SAFETY: an unnamed temporary file this test alone holds, never written again while mapped.
        let permit = unsafe { MapPermit::new() };
        for start in [0, 4096, 12287] {
            let err = permit.map(&file, start..12288 + 64).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{start}");
        }
        let map = permit.map(&file, 12288..12288 + 64).unwrap();
        assert_eq!(map.base_offset(), 12288);
        assert_eq!(&*map, &[7; 64][..]);
    }
}
