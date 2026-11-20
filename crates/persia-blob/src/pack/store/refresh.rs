//! A read-only handle catching up with the writer (SPEC §9.3, ADR-0049 item 10).

use super::{
    ContainerFile, Error, PackMode, PackStore, Result, at, check_scan, corrupt_at, io, io_err,
    read_prefix, recover, scan_committed,
};

impl<F: ContainerFile + 'static> PackStore<F> {
    /// Catches a read-only handle up with the writer (SPEC §9.3, ADR-0049 item 10): re-reads the active slot and
    /// scans only the frames committed since, as the open would; opens the pack if it did not exist before. Returns
    /// whether anything new was found. A read-write handle is the writer and has nothing to catch up with.
    ///
    /// # Errors
    /// [`Error::PackReplaced`] when the path now holds another pack generation (a pack rewrite, SPEC §18.10): reopen;
    /// [`Error::CorruptAt`] naming the slot that moved back or disagrees with the new frames, or a damaged new frame
    /// (the handle then keeps what it had);
    /// [`Error::PackIo`].
    pub fn refresh(&self) -> Result<bool> {
        if self.mode == PackMode::ReadWrite {
            return Ok(false);
        }
        let mut guard = self.write();
        let inner = &mut *guard;
        let Some(pack) = &mut inner.pack else {
            let file = match self.vfs.open(&self.path, false) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(io_err("open")(e)),
            };
            let (mut pack, _) = recover(file, PackMode::ReadOnly, self.db_uuid, false)?;
            if let Some(mapper) = &mut inner.mapper {
                mapper.remap(&mut pack);
            }
            inner.pack = Some(pack);
            return Ok(true);
        };
        let at_path = self.vfs.open(&self.path, false).map_err(io_err("open"))?;
        let at_path_sb = read_prefix(&at_path)?.superblock;
        let found = at_path_sb.generation;
        // The generation alone repeats when a pack is recreated or restored: the pack id tells the files apart.
        if found != pack.sb.generation || at_path_sb.pack_id != pack.sb.pack_id {
            return Err(Error::PackReplaced {
                opened: pack.sb.generation,
                found,
            });
        }
        let p = read_prefix(&pack.file)?;
        let (old, new) = (pack.sb, p.superblock);
        if new == old {
            return Ok(false);
        }
        if new.seq == old.seq {
            // Two commits never share a seq (SPEC §4.3): one of the two slots is wrong.
            return Err(corrupt_at(
                "the same seq with other contents",
                p.slot.offset(),
            ));
        }
        if new.seq < old.seq
            // Every commit appends its index frame: a newer seq always moves log_end forward.
            || new.log_end <= old.log_end
            || new.generation != old.generation
            || new.pack_id != old.pack_id
        {
            return Err(corrupt_at(
                "the committed superblock moved back",
                p.slot.offset(),
            ));
        }
        new.check_bounds(pack.file.file_len().map_err(io_err("stat"))?)
            .map_err(at(p.slot.offset()))?;
        let (chunk_frames, end, seq) = (pack.chunk_frames.len(), pack.end, pack.next_frame_seq);
        // The new slot is the one the scan is checked against (`check_scan` locates its errors there).
        let old_slot = std::mem::replace(&mut pack.slot, p.slot);
        let mut added = Vec::new();
        let mut last_index = None;
        pack.sb = new;
        let scanned =
            scan_committed(pack, new.log_end, &mut last_index, &mut added).and_then(|()| {
                // No new index frame: the slot must still name the last one.
                let last =
                    last_index.or((old.blob_count != 0).then_some((old.index_off, old.index_len)));
                check_scan(pack, last)
            });
        if let Err(e) = scanned {
            for id in added {
                if let Some(entry) = pack.index.remove(&id) {
                    pack.uploads.remove(&entry.upload_id);
                }
            }
            pack.chunk_frames.truncate(chunk_frames);
            (pack.sb, pack.end, pack.next_frame_seq, pack.slot) = (old, end, seq, old_slot);
            return Err(e);
        }
        if let Some(mapper) = &mut inner.mapper {
            mapper.remap(pack);
        }
        Ok(true)
    }
}
