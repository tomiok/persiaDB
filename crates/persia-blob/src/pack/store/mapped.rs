//! Mapped reads of the committed frames (SPEC §4.7, ADR-0049 item 12).

use super::{
    ByteSource, ContainerFile, FIRST_FRAME_OFFSET, File, MapPermit, Pack, PackStore, Range, io,
};
#[cfg(test)]
use std::sync::{Arc, atomic::Ordering};

/// Maps `range` of a pack's file: the real mapping for a [`File`], a copy in tests.
pub(super) type MapFn<F> = Box<dyn Fn(&F, Range<u64>) -> io::Result<ByteSource> + Send + Sync>;

/// Mapped reads of the committed frames (SPEC §4.7, ADR-0049 item 12).
pub(super) struct Mapper<F> {
    map: MapFn<F>,
    /// Remaps that failed since mapped reads were enabled.
    failures: u64,
}

impl<F> std::fmt::Debug for Mapper<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mapper")
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}

impl<F: ContainerFile> Mapper<F> {
    /// Maps `12288..log_end` of the committed superblock again, unless the mapping already ends there. If that fails
    /// the previous mapping stays: it is still valid for the frames it covers, and newer ones are read.
    pub(super) fn remap(&mut self, pack: &mut Pack<F>) {
        let log_end = pack.sb.log_end;
        if pack.window.as_ref().map(ByteSource::end_offset) == Some(log_end) {
            return;
        }
        match (self.map)(&pack.file, FIRST_FRAME_OFFSET..log_end) {
            Ok(window) => pack.window = Some(window),
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                // SPEC §4.7: the fallback to pread is logged and counted, as for the container.
                tracing::warn!(
                    error = %e,
                    log_end,
                    "blob pack remap failed; frames past the previous mapping are read with pread"
                );
            }
        }
    }
}

impl<F: ContainerFile + 'static> PackStore<F> {
    /// Turns mapped reads on with `map` (see [`PackStore::enable_mapped_reads`]); maps now if the pack exists.
    fn enable_mapper(&self, map: MapFn<F>) {
        let mut guard = self.write();
        let inner = &mut *guard;
        let mut mapper = Mapper { map, failures: 0 };
        if let Some(pack) = &mut inner.pack {
            pack.window = None;
            mapper.remap(pack);
        }
        inner.mapper = Some(mapper);
    }

    /// The absolute range of the file that frames are read from the mapping, if one is live.
    pub fn mapped_range(&self) -> Option<Range<u64>> {
        let inner = self.read();
        let window = inner.pack.as_ref()?.window.as_ref()?;
        Some(window.base_offset()..window.end_offset())
    }

    /// How many times mapping the committed frames failed since mapped reads were turned on. Reads stay correct
    /// (the frames past the last mapping are read with `pread`), only slower.
    pub fn mapping_failures(&self) -> u64 {
        self.read().mapper.as_ref().map_or(0, |m| m.failures)
    }

    /// Mapped reads over a copy of the file, for tests of the window logic on simulated files.
    #[cfg(test)]
    fn enable_copied_reads(&self, fail: Arc<std::sync::atomic::AtomicBool>) {
        self.enable_mapper(Box::new(move |file: &F, range: Range<u64>| {
            if fail.load(Ordering::Relaxed) {
                return Err(io::Error::other("simulated map failure"));
            }
            let len =
                usize::try_from(range.end.saturating_sub(range.start)).map_err(io::Error::other)?;
            let mut buf = vec![0; len];
            file.read_exact_at(&mut buf, range.start)?;
            Ok(ByteSource::with_base_offset(buf.into(), range.start))
        }));
    }
}

impl PackStore<File> {
    /// Reads committed frames through a memory mapping from now on, instead of with `pread` (SPEC §4.7, ADR-0049
    /// item 12): `12288..log_end` is mapped now (if the pack exists) and again after each bind. Chunks the mapping
    /// covers are read from it ([`PackStore::read_chunk_source`] then copies nothing); the others with `pread`. A
    /// failed remap keeps the previous mapping and is counted ([`PackStore::mapping_failures`]).
    ///
    /// Map only a pack on a local disk file system: a read error on a mapped page ends the process (`SIGBUS`), where
    /// `pread` would return an error (SPEC §4.7). On Windows a live mapping keeps the file from being shrunk: a
    /// read-write open of the pack that must truncate a tail fails with an I/O error while a reader maps it
    /// (ADR-0049, Consequences). `permit`'s contract includes at most one read-write `PackStore` of the pack at a
    /// time, anywhere ([`MapPermit::new`]).
    ///
    /// ```
    /// use persia_blob::pack::{PackStore, pack_path};
    /// use persia_blob::{BindOutcome, BlobManifest, BlobStore, ChunkSize, UploadId, blob_id_of};
    /// use persia_storage::container::{Container, MapPermit};
    ///
    /// # if cfg!(miri) { return Ok(()); } // a real mapped file: Miri has neither a filesystem nor file-backed mmap
    /// # let dir = tempfile::tempdir()?;
    /// let container = Container::create(dir.path().join("app.persia"))?;
    /// let proof = container.writer_proof().ok_or("read-only")?;
    /// let store = PackStore::open_writer(pack_path(&dir.path().join("app.persia")), &proof)?;
    /// // SAFETY: nothing but this store writes the pack while it is mapped (MapPermit::new's contract).
    /// store.enable_mapped_reads(unsafe { MapPermit::new() });
    /// let m = BlobManifest::new(blob_id_of(b"mapped"), 6, ChunkSize::from_encoding(6)?, UploadId::new(1),
    ///     vec![persia_format::checksum::crc32c(b"mapped")])?;
    /// store.stage_chunk(m.upload_id(), 0, b"mapped")?;
    /// assert_eq!(store.bind(&m)?, BindOutcome::Bound);
    /// assert!(store.mapped_range().is_some());
    /// assert_eq!(&*store.read_chunk_source(&m, 0)?, b"mapped");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// A GC rewrite (ROADMAP 9.5.2) swaps in a new file and maps it again at once (ROADMAP 9.2.12): the mapping then
    /// covers the new file's `12288..log_end`, and the kept blobs are read from it at their new offsets. If that remap
    /// fails, nothing is mapped (never the old file) and every read is a `pread` until the next bind maps again.
    ///
    /// ```
    /// use std::time::{Duration, SystemTime};
    ///
    /// use persia_blob::gc::{GcObject, Mark, PinCheck, PinGate, Rewrite, Sweep, SweepOptions};
    /// use persia_blob::pack::{PackStore, pack_path};
    /// use persia_blob::{BlobManifest, BlobStore, ChunkSize, UploadId, blob_id_of};
    /// use persia_format::checksum::crc32c;
    /// use persia_storage::container::{Container, MapPermit};
    ///
    /// /// No pins: the mark alone decides what the rewrite keeps.
    /// struct NoPins;
    /// impl PinCheck for NoPins {
    ///     fn is_pinned(&self, _: GcObject) -> bool { false }
    ///     fn pinned_blobs(&self) -> Vec<persia_format::BlobId> { Vec::new() }
    /// }
    /// impl PinGate for NoPins {
    ///     fn pinned(&self) -> Vec<GcObject> { Vec::new() }
    ///     fn unless_pinned<T>(&self, _: GcObject, delete: impl FnOnce(&dyn PinCheck) -> T) -> Option<T> {
    ///         Some(delete(self))
    ///     }
    /// }
    ///
    /// # if cfg!(miri) { return Ok(()); } // a real mapped file: Miri has neither a filesystem nor file-backed mmap
    /// # let dir = tempfile::tempdir()?;
    /// let container = Container::create(dir.path().join("app.persia"))?;
    /// let proof = container.writer_proof().ok_or("read-only")?;
    /// let store = PackStore::open_writer(pack_path(&dir.path().join("app.persia")), &proof)?;
    /// // SAFETY: nothing but this store writes the pack while it is mapped (MapPermit::new's contract).
    /// store.enable_mapped_reads(unsafe { MapPermit::new() });
    /// let put = |data: &[u8], upload| -> Result<BlobManifest, persia_blob::Error> {
    ///     let m = BlobManifest::new(blob_id_of(data), data.len() as u64, ChunkSize::new(ChunkSize::MIN)?,
    ///         UploadId::new(upload), vec![crc32c(data)])?;
    ///     store.stage_chunk(m.upload_id(), 0, data)?;
    ///     store.bind(&m)?;
    ///     Ok(m)
    /// };
    /// put(b"dropped by the rewrite", 1)?;
    /// let kept = put(b"kept", 2)?;
    /// let options = SweepOptions { grace: Duration::ZERO, now: SystemTime::now(), rewrite_ratio: 0.0 };
    /// let report = store.sweep(&Mark::new([kept.blob_id()], []), &NoPins, &options)?;
    /// assert!(matches!(report.rewrite, Some(Rewrite::Done { .. })));
    /// let log_end = store.superblock().ok_or("a pack")?.log_end;
    /// assert_eq!(store.mapped_range(), Some(12288..log_end));
    /// let chunk = store.read_chunk_source(&kept, 0)?; // from the new file's mapping
    /// assert_eq!(&*chunk, b"kept");
    /// assert_eq!(chunk.base_offset(), 12288 + 56); // in the new file's first frame, after its frame and chunk heads
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn enable_mapped_reads(&self, permit: MapPermit) {
        self.enable_mapper(Box::new(move |file: &File, range: Range<u64>| {
            permit.map(file, range)
        }));
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use persia_storage::container::Container;
    use persia_storage::container::sim::SimFile;
    use persia_storage::vfs::sim::{SimFs, SimFsFile};

    use super::*;
    use crate::store::BlobStore;
    use crate::{BindOutcome, BlobManifest, ChunkSize, UploadId, blob_id_of, read_chunk_checked};
    use persia_format::checksum::crc32c;

    fn manifest_of(data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
        let crcs = data.chunks(chunk as usize).map(crc32c).collect();
        BlobManifest::new(
            blob_id_of(data),
            data.len() as u64,
            ChunkSize::from_encoding(chunk).unwrap(),
            UploadId::new(upload),
            crcs,
        )
        .unwrap()
    }

    fn store() -> (SimFs, PackStore<SimFsFile>) {
        // A real proof, from a read-write container that outlives every test (ROADMAP 9.2.7).
        static WRITER: std::sync::OnceLock<Container<SimFile>> = std::sync::OnceLock::new();
        let fs = SimFs::new();
        let writer = WRITER.get_or_init(|| Container::create_on(SimFile::new(), [0; 16]).unwrap());
        let proof = writer.writer_proof().unwrap();
        let store = PackStore::open_writer_in(Arc::new(fs.clone()), "/p.blobs", &proof).unwrap();
        (fs, store)
    }

    fn put(store: &PackStore<SimFsFile>, data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
        let m = manifest_of(data, chunk, upload);
        for (i, c) in data.chunks(chunk as usize).enumerate() {
            store.stage_chunk(m.upload_id(), i as u32, c).unwrap();
        }
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        m
    }

    #[test]
    fn mapped_reads_follow_each_bind_and_serve_committed_chunks_from_the_window() {
        let (_fs, store) = store();
        let fail = Arc::new(AtomicBool::new(false));
        store.enable_copied_reads(fail.clone());
        assert_eq!(store.mapped_range(), None); // no pack yet
        let a = put(&store, b"0123456789", 4, 1);
        let range = store.mapped_range().unwrap();
        assert_eq!(
            range,
            FIRST_FRAME_OFFSET..store.superblock().unwrap().log_end
        );
        let chunk = store.read_chunk_source(&a, 1).unwrap();
        assert_eq!(&*chunk, b"4567");
        assert!(
            range.contains(&chunk.base_offset()),
            "served from the window"
        );
        // A failed remap keeps the old window; the new blob's chunks are read with pread.
        fail.store(true, Ordering::Relaxed);
        let b = put(&store, b"abcdef", 4, 2);
        assert_eq!(store.mapping_failures(), 1);
        assert_eq!(store.mapped_range(), Some(range.clone()));
        assert_eq!(read_chunk_checked(&store, &b, 1).unwrap(), b"ef");
        fail.store(false, Ordering::Relaxed);
        let c = put(&store, b"xyz", 4, 3);
        assert_eq!(
            store.mapped_range().unwrap().end,
            store.superblock().unwrap().log_end
        );
        for (m, i, want) in [(&a, 2, &b"89"[..]), (&b, 0, b"abcd"), (&c, 0, b"xyz")] {
            assert_eq!(read_chunk_checked(&store, m, i).unwrap(), want);
        }
    }

    /// An error found before anything is written (here the `frame_seq` run exhausted) leaves the handle usable.
    #[test]
    fn an_error_before_any_write_does_not_poison() {
        let (_fs, store) = store();
        put(&store, b"first", 8, 1);
        let m = manifest_of(b"", 8, 2);
        let saved = {
            let mut inner = store.write();
            let pack = inner.pack.as_mut().unwrap();
            std::mem::replace(&mut pack.next_frame_seq, u64::MAX)
        };
        assert!(matches!(
            store.bind(&m),
            Err(crate::Error::InvalidManifest { .. })
        ));
        assert!(store.stage_chunk(UploadId::new(3), 0, b"x").is_err());
        assert!(!store.is_poisoned());
        store.write().pack.as_mut().unwrap().next_frame_seq = saved;
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    }

    #[test]
    fn a_range_that_crosses_the_end_of_the_window_is_read_with_pread() {
        let (_fs, store) = store();
        store.enable_copied_reads(Arc::new(AtomicBool::new(false)));
        put(&store, b"first", 8, 1);
        let m = manifest_of(b"later", 8, 2);
        store.stage_chunk(m.upload_id(), 0, b"later").unwrap();
        let inner = store.read();
        let pack = inner.pack.as_ref().unwrap();
        let end = store.mapped_range().unwrap().end;
        let bytes = pack.bytes_at(end - 8, 16).unwrap();
        assert_eq!(bytes.base_offset(), end - 8);
        assert_eq!(bytes.as_slice().len(), 16);
    }

    #[test]
    fn a_staged_chunk_past_the_window_is_read_with_pread() {
        let (_fs, store) = store();
        store.enable_copied_reads(Arc::new(AtomicBool::new(false)));
        put(&store, b"first", 8, 1);
        let m = manifest_of(b"later", 8, 2);
        store.stage_chunk(m.upload_id(), 0, b"later").unwrap();
        let chunk = store.read_chunk_source(&m, 0).unwrap();
        assert!(chunk.base_offset() >= store.mapped_range().unwrap().end);
        assert_eq!(&*chunk, b"later");
    }

    #[test]
    fn a_chunk_frame_that_does_not_match_its_manifest_is_corrupt() {
        let (_fs, store) = store();
        let a = put(&store, b"0123456789", 4, 1);
        // Same id and chunks, another upload: the index does not name it, and nothing is staged under it.
        let other = manifest_of(b"0123456789", 4, 2);
        assert!(store.read_chunk(&other, 0).unwrap_err().is_corruption());
        // Another CRC than the stored chunk's.
        let mut crcs = a.chunk_crcs().to_vec();
        crcs[0] ^= 1;
        let wrong =
            BlobManifest::new(a.blob_id(), a.size(), a.chunk_size(), a.upload_id(), crcs).unwrap();
        assert!(store.read_chunk(&wrong, 0).unwrap_err().is_corruption());
        assert!(store.read_chunk(&a, 3).unwrap_err().is_corruption());
    }

    /// ROADMAP 9.2.9 over the mapping: a damaged byte anywhere in a committed chunk frame is `Corrupt`, at the open
    /// (its header) or at the read from the window, as with `pread`.
    #[test]
    fn a_damaged_chunk_frame_read_from_the_window_is_corrupt() {
        let (fs, store) = store();
        let m = put(&store, b"0123456789ab", 8, 1);
        drop(store);
        let bytes = fs.read(std::path::Path::new("/p.blobs")).unwrap();
        let frames =
            crate::pack::layout::chunk_frame_len(8) + crate::pack::layout::chunk_frame_len(4);
        let second = FIRST_FRAME_OFFSET + crate::pack::layout::chunk_frame_len(8);
        for at in FIRST_FRAME_OFFSET..FIRST_FRAME_OFFSET + frames {
            let frame = if at < second {
                FIRST_FRAME_OFFSET
            } else {
                second
            };
            let mut damaged = bytes.clone();
            damaged[at as usize] ^= 0xff;
            let fs = SimFs::new();
            let file = persia_storage::vfs::Vfs::create_new(&fs, std::path::Path::new("/p.blobs"))
                .unwrap();
            file.write_all_at(&damaged, 0).unwrap();
            file.sync_data().unwrap();
            let opened = PackStore::open_in(Arc::new(fs), "/p.blobs", [0; 16]);
            let store = match opened {
                Ok(store) => store,
                Err(e) => {
                    assert!(e.is_corruption(), "byte {at}: {e}");
                    // A damaged header misaligns the walk: the error names this frame or a later one.
                    assert!(
                        e.corrupt_offset().is_some_and(|o| o >= frame),
                        "byte {at}: {e}"
                    );
                    continue;
                }
            };
            store.enable_copied_reads(Arc::new(AtomicBool::new(false)));
            assert!(store.mapped_range().is_some());
            let e = crate::read_range(
                &store,
                &crate::BlobRef::new(
                    m.blob_id(),
                    m.size(),
                    "a/b",
                    8,
                    std::collections::BTreeMap::default(),
                )
                .unwrap(),
                0..m.size(),
            )
            .unwrap_err();
            assert!(e.is_corruption(), "byte {at}: {e}");
            assert_eq!(e.corrupt_offset(), Some(frame), "byte {at}: {e}");
        }
    }

    /// ROADMAP 9.2.9, no copy: with mapped reads on, what `read_chunk_source` returns and what `read_chunks_checked`
    /// lends its sink lie inside the window's own buffer.
    #[test]
    fn chunks_read_from_the_window_are_slices_of_it() {
        let (_fs, store) = store();
        store.enable_copied_reads(Arc::new(AtomicBool::new(false)));
        let m = put(&store, b"0123456789abcdef!", 8, 1);
        let window = {
            let inner = store.read();
            let pack = inner.pack.as_ref().unwrap();
            pack.window.as_ref().unwrap().as_slice().as_ptr_range()
        };
        let inside = |s: &[u8]| {
            let r = s.as_ptr_range();
            window.start <= r.start && r.end <= window.end
        };
        for index in 0..m.chunk_count() {
            let chunk = store.read_chunk_source(&m, index).unwrap();
            assert!(inside(chunk.as_slice()), "read_chunk_source {index}");
        }
        let mut seen = 0;
        store
            .read_chunks_checked(&m, 0..m.chunk_count(), &mut Vec::new(), &mut |_, bytes| {
                assert!(inside(bytes), "read_chunks_checked");
                seen += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, 3);
    }

    // --- A pack rewrite with mapped reads on (ROADMAP 9.2.12) ---

    /// No pin at all: the mark alone decides what a rewrite keeps.
    struct NoPins;

    impl crate::gc::PinCheck for NoPins {
        fn is_pinned(&self, _: crate::gc::GcObject) -> bool {
            false
        }
        fn pinned_blobs(&self) -> Vec<persia_format::BlobId> {
            Vec::new()
        }
    }

    impl crate::gc::PinGate for NoPins {
        fn pinned(&self) -> Vec<crate::gc::GcObject> {
            Vec::new()
        }
        fn unless_pinned<T>(
            &self,
            _: crate::gc::GcObject,
            delete: impl FnOnce(&dyn crate::gc::PinCheck) -> T,
        ) -> Option<T> {
            Some(delete(self))
        }
    }

    /// A pack of a blob the rewrite drops, then one it keeps, with mapped reads on (the copying stand-in, `fail` to
    /// make its remaps fail), forced through a rewrite: the kept blob, and the window before the rewrite.
    fn rewritten(
        fail: &Arc<AtomicBool>,
        fail_remap: bool,
    ) -> (SimFs, PackStore<SimFsFile>, BlobManifest, Range<u64>) {
        let (fs, store) = store();
        store.enable_copied_reads(Arc::clone(fail));
        let dropped = put(&store, b"dropped by the rewrite", 4, 1);
        let kept = put(&store, b"kept by the mark", 4, 2);
        let before = store.mapped_range().unwrap();
        assert_eq!(before.end, store.superblock().unwrap().log_end);
        fail.store(fail_remap, Ordering::Relaxed);
        let report = store
            .rewrite(&crate::gc::Mark::new([kept.blob_id()], []), &NoPins, 0.0)
            .unwrap();
        assert!(
            matches!(report.rewrite, Some(crate::gc::Rewrite::Done { .. })),
            "{report:?}"
        );
        assert_eq!(store.manifest(dropped.blob_id()).unwrap(), None);
        (fs, store, kept, before)
    }

    /// ROADMAP 9.2.12: after the swap the window is mapped again over the new file (`12288..` its `log_end`), holds
    /// that file's bytes, and the kept blob's chunks are slices of it at their new offsets: nothing is read from the
    /// old file's window.
    #[test]
    fn a_rewrite_maps_the_new_file_and_reads_come_from_it() {
        let fail = Arc::new(AtomicBool::new(false));
        let (fs, store, kept, before) = rewritten(&fail, false);
        let log_end = store.superblock().unwrap().log_end;
        assert!(log_end < before.end, "the rewrite dropped frames");
        assert_eq!(store.mapped_range(), Some(FIRST_FRAME_OFFSET..log_end));
        assert_eq!(store.mapping_failures(), 0);
        let file = fs.read(std::path::Path::new("/p.blobs")).unwrap();
        let window = {
            let inner = store.read();
            let window = inner.pack.as_ref().unwrap().window.clone().unwrap();
            assert_eq!(
                window.as_slice(),
                &file[FIRST_FRAME_OFFSET as usize..log_end as usize],
                "the window holds the new file"
            );
            window.as_slice().as_ptr_range()
        };
        // The kept blob is the first in the new file: its first chunk frame is the first frame.
        let mut offset = FIRST_FRAME_OFFSET;
        for index in 0..kept.chunk_count() {
            let chunk = store.read_chunk_source(&kept, index).unwrap();
            let range = chunk.as_slice().as_ptr_range();
            assert!(
                window.start <= range.start && range.end <= window.end,
                "chunk {index} from the window"
            );
            assert_eq!(
                chunk.base_offset(),
                offset + crate::pack::layout::CHUNK_BYTES_OFFSET as u64,
                "chunk {index} at its new offset"
            );
            offset += crate::pack::layout::chunk_frame_len(chunk.as_slice().len() as u32);
        }
        let all = crate::read_manifest_range(&store, &kept, 0..kept.size()).unwrap();
        assert_eq!(all, b"kept by the mark");
        // Binds after the rewrite move the window on, over the same new file.
        let next = put(&store, b"after the rewrite", 4, 3);
        assert_eq!(
            store.mapped_range().unwrap().end,
            store.superblock().unwrap().log_end
        );
        assert_eq!(read_chunk_checked(&store, &next, 0).unwrap(), b"afte");
    }

    /// A remap that fails after the swap leaves no window at all, never the old file's: every read is a `pread` of the
    /// new file, and the failure is counted. The next bind maps again.
    #[test]
    fn a_rewrite_whose_remap_fails_reads_the_new_file_with_pread() {
        let fail = Arc::new(AtomicBool::new(false));
        let (_fs, store, kept, _) = rewritten(&fail, true);
        assert_eq!(
            store.mapped_range(),
            None,
            "the old file's window is dropped"
        );
        assert_eq!(store.mapping_failures(), 1);
        let all = crate::read_manifest_range(&store, &kept, 0..kept.size()).unwrap();
        assert_eq!(all, b"kept by the mark");
        fail.store(false, Ordering::Relaxed);
        put(&store, b"after the rewrite", 4, 3);
        assert_eq!(
            store.mapped_range(),
            Some(FIRST_FRAME_OFFSET..store.superblock().unwrap().log_end)
        );
        assert_eq!(read_chunk_checked(&store, &kept, 1).unwrap(), b" by ");
    }
}
