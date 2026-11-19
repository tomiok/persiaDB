//! The pack rewrite (SPEC §9.3, §9.5, §18.10; ROADMAP 9.5.2, ADR-0052): GC copies what it keeps into a new pack file
//! and renames it over the pack, never compacting in place.
//!
//! The writer mutex is held from the plan to the swap, so nothing is staged or bound meanwhile (a `put_blob` waits),
//! and nothing is bound into the new pack before its directory sync succeeded (§18.10 (b)). Readers go on under the
//! `RwLock`'s read side, on the old file, until the swap replaces the file and the index at once: a `get_blob`
//! stream looks every chunk up by id, so it reads on in the new file (§18.10 (c)).
//!
//! The new file, `<pack>.tmp.<nonce>.<n>` (the temporary name a read-write open sweeps, ROADMAP 9.2.6), holds the
//! header, both slots with generation + 1 and a new `pack_id`, then each kept blob's chunk frames followed by its
//! `BlobIndex` frame, renumbered from `frame_seq` 1 (the last index frame ends at `log_end`, SPEC §9.3), then the
//! chunks this handle staged for pinned uploads, after `log_end` like any staged chunk (a crash drops them, as it
//! would have from the old pack: local uploads are not resumable, ADR-0049). It is synced, renamed over the pack
//! (only if the path still names this handle's file), and the directory synced. The pin check of everything dropped
//! and the rename, directory sync and swap are one step under the pin set's lock (SPEC §9.5), so a pin taken after
//! the plan defers the rewrite instead of losing what it names.
//!
//! ROADMAP 9.5.8 holds locally by construction: a bound blob's chunks are kept or dropped with it, and kept when
//! its blob or its upload is pinned, while a staged upload is one no manifest in the pack names yet (a bind takes
//! its chunks out of the staged ones). So no pinned blob's manifest can name an upload whose chunks the rewrite
//! drops.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use persia_format::checksum::crc32c;

use super::{
    BlobId, ContainerFile, Entry, Error, FRAME_HEADER_LEN, FrameHeader, FrameKind, Header, Inner,
    Pack, PackStore, Poison, Result, Slot, Staged, WriterState, corrupt, decode_chunk_payload,
    decode_frame, from_format, io, io_err, layout, parent_of, random_u128, temp_path,
    u64_from_usize, usize_from_u32,
};
use crate::gc::{GcObject, Mark, PinCheck, PinGate, Rewrite, SweepReport};
use crate::manifest::UploadId;
use crate::pack::layout::{FIRST_FRAME, PackSuperblock};

/// What a rewrite keeps and drops, planned under the writer mutex.
#[derive(Debug, Default)]
struct Plan {
    /// The generation of the pack it replaces.
    generation: u64,
    /// Kept blobs, in the old file's order.
    blobs: Vec<(BlobId, Entry)>,
    /// Kept staged chunks, by upload and index, in the old file's order.
    staged: Vec<((UploadId, u32), Staged)>,
    /// Dropped blobs, with the upload each one's manifest names.
    dropped_blobs: Vec<(BlobId, UploadId)>,
    /// Uploads whose staged chunks are dropped.
    dropped_uploads: Vec<UploadId>,
    /// Bytes of the frames now (`12288..end`), and of the frames the plan keeps.
    frame_bytes: u64,
    live_bytes: u64,
    /// Chunk frames now, and kept.
    chunk_frames: u64,
    kept_chunk_frames: u64,
}

/// The new pack, written and synced under its temporary name.
struct Written<F> {
    file: F,
    temp: PathBuf,
    sb: PackSuperblock,
    end: u64,
    next_frame_seq: u64,
    index: HashMap<BlobId, Entry>,
    uploads: HashMap<UploadId, BlobId>,
    chunk_frames: Vec<u64>,
    staged: HashMap<(UploadId, u32), Staged>,
}

impl<F: ContainerFile + 'static> PackStore<F> {
    /// GC of the pack (SPEC §9.5, ADR-0052): keeps every blob the mark names or that is pinned, every blob whose
    /// upload is marked or pinned, and the chunks this handle staged for marked or pinned uploads; everything else is
    /// dropped at once (locally there is no grace period: only this writer writes the pack, and local uploads are not
    /// resumable). The file is rewritten only when the dropped bytes reach `ratio` of the frames' bytes (0 forces
    /// it); otherwise nothing changes.
    ///
    /// # Errors
    /// [`Error::PackReadOnly`], [`Error::PackPoisoned`]; [`Error::Corrupt`] for a kept frame that is damaged (nothing
    /// is dropped then: a rewrite never repairs); [`Error::PackIo`]. A failure before the rename leaves the pack as it
    /// was. A failed directory sync after it poisons the handle, which moves to the new file (a crash may bring back
    /// the old one: both hold every kept blob).
    pub(crate) fn rewrite<P: PinGate>(
        &self,
        mark: &Mark,
        pins: &P,
        ratio: f64,
    ) -> Result<SweepReport> {
        let mut w = self.writer(None)?;
        // Pins taken since the mark: what they name is kept too. Nothing new can be staged or bound until the swap.
        let mut pinned_blobs = HashSet::new();
        let mut pinned_uploads = HashSet::new();
        for object in pins.pinned() {
            match object {
                GcObject::Blob(id) => pinned_blobs.insert(id),
                GcObject::Upload(id) => pinned_uploads.insert(id),
            };
        }
        let keep_upload =
            |upload: &UploadId| mark.uploads.contains(upload) || pinned_uploads.contains(upload);
        let keep_blob = |id: &BlobId, upload: &UploadId| {
            mark.blobs.contains(id) || pinned_blobs.contains(id) || keep_upload(upload)
        };
        let plan = {
            let inner = self.read();
            let Some(pack) = &inner.pack else {
                return Ok(report(Rewrite::NoPack));
            };
            plan(pack, &inner, keep_blob, keep_upload)?
        };
        let dead = plan
            .frame_bytes
            .checked_sub(plan.live_bytes)
            .ok_or_else(|| corrupt("kept frames larger than the pack"))?;
        if !ratio_reached(dead, plan.frame_bytes, ratio) {
            return Ok(report(Rewrite::BelowRatio {
                dead_bytes: dead,
                frame_bytes: plan.frame_bytes,
            }));
        }
        let written = self.write_new(&plan)?;
        let (temp, generation) = (written.temp.clone(), written.sb.generation);
        let first = plan
            .dropped_blobs
            .first()
            .map(|(id, _)| GcObject::Blob(*id))
            .or_else(|| plan.dropped_uploads.first().copied().map(GcObject::Upload));
        let replaced = match first {
            // Only frames nobody can name are dropped (orphan chunks): there is no pin to check.
            None => Some(self.replace(&mut w, written)),
            Some(first) => pins
                .unless_pinned(first, |view| {
                    (!dropped_is_pinned(&plan, view)).then(|| self.replace(&mut w, written))
                })
                .flatten(),
        };
        let Some(replaced) = replaced else {
            // A pin taken since the plan names something it drops: GC deletes less, never more.
            return match self.vfs.remove_file(&temp) {
                Ok(()) => Ok(report(Rewrite::Deferred)),
                Err(e) => Err(io_err("remove the rewrite's temporary file")(e)),
            };
        };
        replaced?;
        Ok(SweepReport {
            blobs_deleted: u64_from_usize(plan.dropped_blobs.len()),
            uploads_deleted: u64_from_usize(plan.dropped_uploads.len()),
            chunks_deleted: plan.chunk_frames.saturating_sub(plan.kept_chunk_frames),
            rewrite: Some(Rewrite::Done {
                generation,
                frame_bytes_before: plan.frame_bytes,
                frame_bytes_after: plan.live_bytes,
            }),
            ..SweepReport::default()
        })
    }

    /// Writes the new pack under a temporary name and syncs it. On an error the temporary file is removed.
    fn write_new(&self, plan: &Plan) -> Result<Written<F>> {
        let temp = temp_path(&self.path);
        let file = self
            .vfs
            .create_new(&temp)
            .map_err(io_err("create the rewrite's temporary file"))?;
        match self.fill(file, temp.clone(), plan) {
            Ok(written) => Ok(written),
            Err(e) => Err(self.discard(&temp, e)),
        }
    }

    /// Copies the kept frames into `file`, renumbered, then writes the header and both slots, and syncs.
    fn fill(&self, file: F, temp: PathBuf, plan: &Plan) -> Result<Written<F>> {
        let pack_id = random_u128();
        let generation = plan
            .generation
            .checked_add(1)
            .ok_or_else(|| corrupt("the pack generation is exhausted"))?;
        let mut out = Out {
            file: &file,
            end: FIRST_FRAME,
            seq: 1,
        };
        let mut index = HashMap::new();
        let mut uploads = HashMap::new();
        let mut chunk_frames = Vec::new();
        let mut last = None;
        for (id, entry) in &plan.blobs {
            let manifest = {
                let inner = self.read();
                let pack = inner.pack.as_ref().ok_or(Error::PackPoisoned)?;
                pack.manifest_of(*id, entry)?
            };
            let upload_id = manifest.upload_id();
            let mut offsets = Vec::with_capacity(entry.chunks.len());
            for (i, &offset) in (0_u32..).zip(entry.chunks.iter()) {
                let crc = *manifest
                    .chunk_crcs()
                    .get(usize_from_u32(i))
                    .ok_or_else(|| corrupt("an index frame names more chunks than its manifest"))?;
                let frame = self.copied_chunk(offset, upload_id, i, crc, out.seq)?;
                offsets.push(out.append(&frame)?);
            }
            chunk_frames.extend_from_slice(&offsets);
            let frame = layout::encode_index_frame(out.seq, &manifest, &offsets)?;
            let index_off = out.append(&frame)?;
            let index_len = u64_from_usize(frame.len());
            index.insert(
                *id,
                Entry {
                    index_off,
                    index_len,
                    upload_id,
                    chunks: offsets.into_boxed_slice(),
                },
            );
            uploads.insert(upload_id, *id);
            last = Some((index_off, index_len));
        }
        let (log_end, seq_at_log_end) = (out.end, out.seq);
        let mut staged = HashMap::new();
        for &((upload_id, i), s) in &plan.staged {
            let frame = self.copied_chunk(s.offset, upload_id, i, s.crc, out.seq)?;
            let offset = out.append(&frame)?;
            chunk_frames.push(offset);
            staged.insert((upload_id, i), Staged { offset, ..s });
        }
        let sb = match last {
            // SPEC §9.3: `seq 0` is exactly the new-pack values, whatever the generation.
            None => PackSuperblock {
                generation,
                ..PackSuperblock::new_pack(pack_id)
            },
            Some((index_off, index_len)) => PackSuperblock {
                seq: 1,
                index_off,
                index_len,
                log_end,
                generation,
                blob_count: u64_from_usize(index.len()),
                next_frame_seq: seq_at_log_end,
                pack_id,
            },
        };
        let mut prefix = layout::new_pack_prefix(self.db_uuid, pack_id)?;
        let slot = sb.encode();
        for s in [Slot::A, Slot::B] {
            let at = usize::try_from(s.offset()).map_err(|_| corrupt("a slot offset"))?;
            prefix
                .get_mut(at..at.saturating_add(slot.len()))
                .ok_or_else(|| corrupt("a pack prefix shorter than its slots"))?
                .copy_from_slice(&slot);
        }
        file.write_all_at(&prefix, 0)
            .map_err(io_err("write the rewrite's header"))?;
        file.sync_data().map_err(io_err("sync the rewrite"))?;
        let (end, next_frame_seq) = (out.end, out.seq);
        Ok(Written {
            file,
            temp,
            sb,
            end,
            next_frame_seq,
            index,
            uploads,
            chunk_frames,
            staged,
        })
    }

    /// The chunk frame at `offset` of the old pack, checked against what keeps it (chunk `index` of `upload_id` with
    /// CRC32C `crc`, its bytes included), encoded again as frame `seq`.
    fn copied_chunk(
        &self,
        offset: u64,
        upload_id: UploadId,
        index: u32,
        crc: u32,
        seq: u64,
    ) -> Result<Vec<u8>> {
        let inner = self.read();
        let pack = inner.pack.as_ref().ok_or(Error::PackPoisoned)?;
        let frame = pack.frame_at(offset, pack.end)?;
        let (_, payload) = decode_frame(&frame, offset, FrameKind::BlobChunk)?;
        let (head, bytes) = decode_chunk_payload(payload)?;
        if head.upload_id != upload_id || head.index != index {
            return Err(corrupt(
                "a kept chunk frame of another upload or index than recorded",
            ));
        }
        // A rewrite never copies damage forward: the new frame's CRC would cover it.
        if head.crc != crc || crc32c(bytes) != crc {
            return Err(corrupt("a kept chunk's bytes do not match their CRC32C"));
        }
        layout::encode_chunk_frame(seq, upload_id, index, bytes)
    }

    /// Renames the written pack over the pack, syncs the directory and swaps the handle's file, index and staged
    /// chunks at once (under the `RwLock`'s write side). Run under the pin set's lock, with the writer mutex held.
    fn replace(&self, w: &mut WriterState, written: Written<F>) -> Result<()> {
        // Never rename over a path that no longer names this handle's file (replaced from outside: not ours).
        let same = {
            let inner = self.read();
            let pack = inner.pack.as_ref().ok_or(Error::PackPoisoned)?;
            self.vfs.same_file(&pack.file, &self.path)
        };
        let renamed = match same {
            Ok(true) => self
                .vfs
                .rename(&written.temp, &self.path)
                .map_err(io_err("rename")),
            Ok(false) => Err(Error::PackIo {
                op: "rename",
                source: io::Error::other("the path no longer names this pack"),
            }),
            Err(e) => Err(io_err("stat")(e)),
        };
        if let Err(e) = renamed {
            // A rename that reported an error may still have happened: unless the path is seen to name this handle's
            // file, a blob bound into it would be lost at the next open, so the handle stops writing.
            let still_ours = {
                let inner = self.read();
                inner
                    .pack
                    .as_ref()
                    .is_some_and(|p| matches!(self.vfs.same_file(&p.file, &self.path), Ok(true)))
            };
            if !still_ours {
                w.poison = Some(Poison::NotBound);
                return Err(e);
            }
            let temp = written.temp.clone();
            drop(written);
            return Err(self.discard(&temp, e));
        }
        // The path names the complete new pack now, or after a crash maybe the old one: both hold every kept blob.
        let synced = self.vfs.sync_dir(parent_of(&self.path));
        let mut guard = self.write();
        let inner: &mut Inner<F> = &mut guard;
        inner.pack = Some(Pack {
            file: written.file,
            header: Header::new(self.db_uuid),
            slot: Slot::A,
            sb: written.sb,
            end: written.end,
            next_frame_seq: written.next_frame_seq,
            index: written.index,
            uploads: written.uploads,
            chunk_frames: written.chunk_frames,
            window: None,
        });
        inner.staged = written.staged;
        if let (Some(mapper), Some(pack)) = (&mut inner.mapper, &mut inner.pack) {
            mapper.remap(pack);
        }
        if let Err(e) = synced {
            // SPEC §18.10 (b): a blob bound now could be lost with the rename by a crash while documents reference
            // it, so the handle stops writing, as after a container compaction (SPEC §4.6).
            w.poison = Some(Poison::NotBound);
            return Err(io_err("sync directory")(e));
        }
        Ok(())
    }

    /// Removes the temporary file of a rewrite that failed with `err`; a failed removal is reported with it (the
    /// next read-write open sweeps the file, ROADMAP 9.2.6).
    fn discard(&self, temp: &Path, err: Error) -> Error {
        match self.vfs.remove_file(temp) {
            Ok(()) => err,
            Err(cleanup) => Error::PackIo {
                op: "rewrite",
                source: io::Error::other(format!(
                    "{err}; removing {} also failed: {cleanup}",
                    temp.display()
                )),
            },
        }
    }
}

/// Where the next frame of the new pack goes.
struct Out<'f, F> {
    file: &'f F,
    end: u64,
    seq: u64,
}

impl<F: ContainerFile> Out<'_, F> {
    /// Writes `frame` at the end and returns its offset.
    fn append(&mut self, frame: &[u8]) -> Result<u64> {
        let offset = self.end;
        self.file
            .write_all_at(frame, offset)
            .map_err(io_err("write the rewrite"))?;
        self.end = offset
            .checked_add(u64_from_usize(frame.len()))
            .ok_or_else(|| corrupt("the pack's offsets are exhausted"))?;
        self.seq = self
            .seq
            .checked_add(1)
            .ok_or_else(|| corrupt("frame_seq exhausted"))?;
        Ok(offset)
    }
}

/// What `pack` keeps and drops (see [`PackStore::rewrite`]), and the bytes of each.
fn plan<F: ContainerFile>(
    pack: &Pack<F>,
    inner: &Inner<F>,
    keep_blob: impl Fn(&BlobId, &UploadId) -> bool,
    keep_upload: impl Fn(&UploadId) -> bool,
) -> Result<Plan> {
    let mut plan = Plan {
        generation: pack.sb.generation,
        frame_bytes: pack
            .end
            .checked_sub(FIRST_FRAME)
            .ok_or_else(|| corrupt("a pack end before its first frame"))?,
        chunk_frames: u64_from_usize(pack.chunk_frames.len()),
        ..Plan::default()
    };
    let mut live = Bytes::default();
    let mut blobs: Vec<(BlobId, Entry)> =
        pack.index.iter().map(|(id, e)| (*id, e.clone())).collect();
    blobs.sort_unstable_by_key(|(_, e)| e.index_off);
    for (id, entry) in blobs {
        if keep_blob(&id, &entry.upload_id) {
            for &offset in &entry.chunks {
                live.chunk(frame_len(pack, offset)?)?;
            }
            live.add(entry.index_len)?;
            plan.blobs.push((id, entry));
        } else {
            plan.dropped_blobs.push((id, entry.upload_id));
        }
    }
    let mut staged: Vec<_> = inner.staged.iter().map(|(k, s)| (*k, *s)).collect();
    staged.sort_unstable_by_key(|(_, s)| s.offset);
    let mut dropped = BTreeSet::new();
    for (key, s) in staged {
        if keep_upload(&key.0) {
            live.chunk(frame_len(pack, s.offset)?)?;
            plan.staged.push((key, s));
        } else {
            dropped.insert(key.0);
        }
    }
    plan.dropped_uploads = dropped.into_iter().collect();
    plan.live_bytes = live.bytes;
    plan.kept_chunk_frames = live.chunks;
    Ok(plan)
}

/// A running total of kept frames.
#[derive(Debug, Default)]
struct Bytes {
    bytes: u64,
    chunks: u64,
}

impl Bytes {
    fn add(&mut self, len: u64) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(len)
            .ok_or_else(|| corrupt("frame lengths overflow"))?;
        Ok(())
    }

    fn chunk(&mut self, len: u64) -> Result<()> {
        self.chunks = self.chunks.saturating_add(1);
        self.add(len)
    }
}

/// The total length of the frame at `offset`, from its header.
fn frame_len<F: ContainerFile>(pack: &Pack<F>, offset: u64) -> Result<u64> {
    let head = pack.bytes_at(offset, u64_from_usize(FRAME_HEADER_LEN))?;
    let header = FrameHeader::decode(&mut head.reader()).map_err(from_format)?;
    header.check_fits(pack.end).map_err(from_format)?;
    Ok(header.total_len())
}

/// Whether the dropped bytes `dead` reach `ratio` of the frames' bytes `frames`; a ratio of 0 (or below) always does.
#[allow(clippy::as_conversions, clippy::cast_precision_loss)] // A ratio of byte counts: f64 rounding is harmless.
fn ratio_reached(dead: u64, frames: u64, ratio: f64) -> bool {
    ratio <= 0.0 || (dead > 0 && dead as f64 >= ratio * frames as f64)
}

/// Whether a pin names something the plan drops: a blob, the upload its manifest names, or a staged upload.
fn dropped_is_pinned(plan: &Plan, view: &dyn PinCheck) -> bool {
    plan.dropped_blobs.iter().any(|(id, upload)| {
        view.is_pinned(GcObject::Blob(*id)) || view.is_pinned(GcObject::Upload(*upload))
    }) || plan
        .dropped_uploads
        .iter()
        .any(|u| view.is_pinned(GcObject::Upload(*u)))
}

/// The report of a sweep of the pack that deleted nothing.
fn report(rewrite: Rewrite) -> SweepReport {
    SweepReport {
        rewrite: Some(rewrite),
        ..SweepReport::default()
    }
}

#[cfg(test)]
mod tests {
    use super::ratio_reached;

    #[test]
    fn the_ratio_counts_dead_bytes_and_zero_forces() {
        assert!(ratio_reached(0, 0, 0.0));
        assert!(ratio_reached(0, 100, 0.0));
        assert!(!ratio_reached(0, 100, 0.5));
        assert!(!ratio_reached(49, 100, 0.5));
        assert!(ratio_reached(50, 100, 0.5));
        assert!(ratio_reached(100, 100, 1.0));
        assert!(!ratio_reached(99, 100, 1.0));
        assert!(!ratio_reached(100, 100, f64::NAN));
    }
}
