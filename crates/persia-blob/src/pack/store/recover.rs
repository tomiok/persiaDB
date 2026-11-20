//! Opening a pack: the index rebuilt by a scan of the committed frames, the roll-forward of a complete tail bind,
//! and the truncation of the rest (SPEC §9.3, ADR-0049 item 9).

use super::{
    Access, BLOB_PACK, BlobId, BlobManifest, ContainerFile, Entry, Error, FIRST_FRAME_OFFSET,
    FRAME_HEADER_LEN, FrameHeader, FrameKind, HashMap, MIN_FRAME_LEN, Pack, PackMode, PackRecovery,
    Reader, Result, StreamScanner, at, commit_slot, corrupt_at, decode_frame, decode_index_payload,
    decode_prefix, from_format_at, io, io_err, layout, u64_from_usize, usize_from_u32,
};

/// The prefix of the pack in `file`: header and active slot.
pub(super) fn read_prefix<F: ContainerFile>(file: &F) -> Result<layout::Prefix> {
    let mut prefix = vec![0; usize::try_from(FIRST_FRAME_OFFSET).unwrap_or(12288)];
    file.read_exact_at(&mut prefix, 0).map_err(io_err("read"))?;
    decode_prefix(&prefix)
}

/// Opens the pack in `file` (SPEC §9.3, ADR-0049 item 9): header, active slot against the file length, the index
/// rebuilt by scanning `12288..log_end`; a read-write open then truncates to `log_end` and syncs.
pub(super) fn recover<F: ContainerFile>(
    file: F,
    mode: PackMode,
    db_uuid: [u8; 16],
) -> Result<(Pack<F>, PackRecovery)> {
    let len = file.file_len().map_err(io_err("stat"))?;
    if len < FIRST_FRAME_OFFSET {
        return Err(corrupt_at(
            "a file shorter than a pack's header and slots",
            0,
        ));
    }
    let p = read_prefix(&file)?;
    if p.header.db_uuid != db_uuid {
        // Serving, or collecting, another database's blobs would hand out bytes its documents never referenced.
        return Err(Error::PackOfAnotherDatabase {
            found: p.header.db_uuid,
        });
    }
    if mode == PackMode::ReadWrite && p.header.access_with(&BLOB_PACK) == Access::ReadOnly {
        return Err(Error::PackReadOnly);
    }
    // Taken after reading the slots: a writer appends its frames before the slot that covers them.
    let file_len = file.file_len().map_err(io_err("stat"))?;
    p.superblock
        .check_bounds(file_len)
        .map_err(at(p.slot.offset()))?;
    let mut pack = Pack {
        file,
        header: p.header,
        slot: p.slot,
        sb: p.superblock,
        end: FIRST_FRAME_OFFSET,
        next_frame_seq: 1,
        index: HashMap::new(),
        uploads: HashMap::new(),
        chunk_frames: Vec::new(),
        window: None,
    };
    let mut last_index = None;
    scan_committed(
        &mut pack,
        p.superblock.log_end,
        &mut last_index,
        &mut Vec::new(),
    )?;
    check_scan(&pack, last_index)?;
    let mut recovery = PackRecovery::default();
    if mode == PackMode::ReadWrite {
        roll_forward(&mut pack, file_len, &mut recovery)?;
        if file_len > pack.end {
            // Never below `log_end`, so frames a reader mapped stay valid (SPEC §4.7).
            pack.file.set_len(pack.end).map_err(io_err("truncate"))?;
            recovery.truncated_from = Some(file_len);
        }
        // What was read may be only in the page cache of a writer that died before syncing: make it durable before
        // anything is built on it (SPEC §4.5.1), and before a rolled-forward slot names it.
        pack.file.sync_data().map_err(io_err("sync"))?;
        if recovery.rolled_forward != 0 {
            commit_slot(&mut pack).map_err(|e| match e {
                Error::BindOutcomeUnknown(source) => Error::PackIo {
                    op: "write superblock",
                    source: io::Error::other(source),
                },
                other => other,
            })?;
        }
    }
    Ok((pack, recovery))
}

/// A read-write open's look at the frames after `log_end` (ADR-0049 item 9): the longest run of whole, valid frames
/// that continues the `frame_seq` run and ends in a `BlobIndex` frame whose chunks are all in the pack is kept, and
/// committed by the caller. That is a bind cut after its data sync and before its slot (an orphan blob, harmless), or
/// one whose synced slot was lost (a damaged newer slot: its blob was acknowledged, and documents may reference it).
/// Everything after it is left for the caller to truncate: a torn tail, staged chunks, garbage.
fn roll_forward<F: ContainerFile>(
    pack: &mut Pack<F>,
    file_len: u64,
    recovery: &mut PackRecovery,
) -> Result<()> {
    let mut scanner = StreamScanner::new(pack.end, file_len).expect_frame_seq(pack.next_frame_seq);
    let mut read = |offset, buf: &mut [u8]| pack.file.read_exact_at(buf, offset);
    let mut tail = Vec::new();
    while let Some(header) = scanner.next_frame(&mut read).map_err(io_err("read"))? {
        tail.push(header);
    }
    let committed_chunks = pack.chunk_frames.len();
    let mut keep = None;
    let mut entries = Vec::new();
    let mut pending = Vec::new();
    for header in &tail {
        let offset = header.offset();
        match header.kind() {
            FrameKind::BlobChunk => pending.push(offset),
            FrameKind::BlobIndex => {
                pack.chunk_frames.append(&mut pending);
                let Ok(bytes) = pack.bytes_at(offset, header.total_len()) else {
                    break;
                };
                let Ok((_, payload)) = decode_frame(&bytes, offset, FrameKind::BlobIndex) else {
                    break;
                };
                let Ok((manifest, chunks)) = decode_index_payload(payload, offset) else {
                    break;
                };
                let id = manifest.blob_id();
                let known = |o: &u64| pack.chunk_frames.binary_search(o).is_ok();
                let fresh = !pack.index.contains_key(&id)
                    && !pack.uploads.contains_key(&manifest.upload_id())
                    && entries.iter().all(|(e, entry): &(BlobId, Entry)| {
                        *e != id && entry.upload_id != manifest.upload_id()
                    });
                if !fresh
                    || !chunks.iter().all(known)
                    || !chunks_match(pack, &manifest, &chunks, file_len)
                {
                    break;
                }
                entries.push((
                    id,
                    Entry {
                        index_off: offset,
                        index_len: header.total_len(),
                        upload_id: manifest.upload_id(),
                        chunks: chunks.into_boxed_slice(),
                    },
                ));
                keep = Some((
                    header.offset().saturating_add(header.total_len()),
                    header.frame_seq(),
                    pack.chunk_frames.len(),
                    entries.len(),
                ));
            }
            _ => break,
        }
    }
    let Some((end, last_seq, chunk_count, entry_count)) = keep else {
        pack.chunk_frames.truncate(committed_chunks);
        return Ok(());
    };
    pack.chunk_frames.truncate(chunk_count);
    entries.truncate(entry_count);
    for (id, entry) in entries {
        pack.uploads.insert(entry.upload_id, id);
        pack.index.insert(id, entry);
        recovery.rolled_forward = recovery.rolled_forward.saturating_add(1);
    }
    pack.end = end;
    pack.next_frame_seq = last_seq.saturating_add(1);
    Ok(())
}

/// Checks what the scan found against the slot: `blob_count` index frames, the last one at `index_off`.
pub(super) fn check_scan<F>(pack: &Pack<F>, last_index: Option<(u64, u64)>) -> Result<()> {
    let sb = &pack.sb;
    // Found by comparing the frames with the slot: located at the slot.
    let corrupt = |what| corrupt_at(what, pack.slot.offset());
    if u64_from_usize(pack.index.len()) != sb.blob_count {
        return Err(corrupt(
            "the committed frames hold another number of blobs than the slot",
        ));
    }
    let named = (sb.blob_count != 0).then_some((sb.index_off, sb.index_len));
    if last_index != named {
        return Err(corrupt(
            "the last committed index frame is not the one the slot names",
        ));
    }
    if pack.next_frame_seq != sb.next_frame_seq {
        return Err(corrupt(
            "the committed frames end at another frame_seq than the slot's",
        ));
    }
    Ok(())
}

/// Walks the frames from `pack.end` to `to` by their headers (ADR-0049 item 9), expecting `pack.next_frame_seq` at
/// the first and one more at each: locates every `BlobChunk` frame and reads, validates and indexes every
/// `BlobIndex` frame. Frames below `to` are committed, so anything invalid is corruption. Records the last index
/// frame in `last_index`.
pub(super) fn scan_committed<F: ContainerFile>(
    pack: &mut Pack<F>,
    to: u64,
    last_index: &mut Option<(u64, u64)>,
    added: &mut Vec<BlobId>,
) -> Result<()> {
    let mut head = [0_u8; FRAME_HEADER_LEN];
    while pack.end < to {
        let offset = pack.end;
        if to.saturating_sub(offset) < u64_from_usize(MIN_FRAME_LEN) {
            return Err(corrupt_at(
                "the committed frames end inside a frame",
                offset,
            ));
        }
        pack.file
            .read_exact_at(&mut head, offset)
            .map_err(io_err("read"))?;
        let header = FrameHeader::decode(&mut Reader::with_base_offset(&head, offset))
            .map_err(|e| from_format_at(e, offset))?;
        header
            .check_fits(to)
            .map_err(|e| from_format_at(e, offset))?;
        if header.frame_seq() != pack.next_frame_seq {
            return Err(corrupt_at(
                "a committed frame out of the frame_seq run",
                offset,
            ));
        }
        match header.kind() {
            FrameKind::BlobChunk => {
                // A chunk frame holds its 32-byte head and at least one byte (SPEC §9.3).
                if header.payload_len() <= u64_from_usize(layout::CHUNK_HEAD_LEN) {
                    return Err(corrupt_at(
                        "a committed chunk frame shorter than a chunk",
                        offset,
                    ));
                }
                pack.chunk_frames.push(offset);
            }
            FrameKind::BlobIndex => {
                added.push(index_frame(pack, offset, header.total_len())?);
                *last_index = Some((offset, header.total_len()));
            }
            // No pack writer emits another kind: a new one would come with a pack `incompat_flags` bit (SPEC §15), so
            // here it is a damaged kind byte, never a frame to skip (ADR-0049 item 9).
            _ => {
                return Err(corrupt_at(
                    "a committed frame of a kind no pack holds",
                    offset,
                ));
            }
        }
        // `check_fits` bounded the frame by `to`.
        pack.end = offset.saturating_add(header.total_len());
        pack.next_frame_seq = pack.next_frame_seq.saturating_add(1);
    }
    Ok(())
}

/// Reads, validates and indexes the `BlobIndex` frame at `offset`.
fn index_frame<F: ContainerFile>(pack: &mut Pack<F>, offset: u64, len: u64) -> Result<BlobId> {
    let bytes = pack.bytes_at(offset, len)?;
    let (_, payload) = decode_frame(&bytes, offset, FrameKind::BlobIndex)?;
    let (manifest, chunks) = decode_index_payload(payload, offset)?;
    if chunks
        .iter()
        .any(|o| pack.chunk_frames.binary_search(o).is_err())
    {
        return Err(corrupt_at(
            "an index names a chunk frame the pack does not hold",
            offset,
        ));
    }
    let blob_id = manifest.blob_id();
    if pack.index.contains_key(&blob_id) || pack.uploads.contains_key(&manifest.upload_id()) {
        return Err(corrupt_at("a blob or an upload bound twice", offset));
    }
    pack.uploads.insert(manifest.upload_id(), blob_id);
    pack.index.insert(
        blob_id,
        Entry {
            index_off: offset,
            index_len: len,
            upload_id: manifest.upload_id(),
            chunks: chunks.into_boxed_slice(),
        },
    );
    Ok(blob_id)
}

/// Whether every chunk frame `chunks` names is chunk `i` of `manifest`: its frame holds exactly `32 + len` payload
/// bytes, and its head names the manifest's upload, index `i`, the chunk's length and the manifest's CRC32C. What a
/// roll-forward checks before it commits an index it found after `log_end` (ADR-0049 item 9); the frames' CRCs were
/// checked by the scan, or are committed.
fn chunks_match<F: ContainerFile>(
    pack: &Pack<F>,
    manifest: &BlobManifest,
    chunks: &[u64],
    file_len: u64,
) -> bool {
    let head_len = FRAME_HEADER_LEN.saturating_add(layout::CHUNK_HEAD_LEN);
    chunks.iter().zip(0_u32..).all(|(&offset, index)| {
        let expected = manifest
            .chunk_range(index)
            .and_then(|r| r.end.checked_sub(r.start))
            .zip(manifest.chunk_crcs().get(usize_from_u32(index)).copied());
        let Some((len, crc)) = expected else {
            return false;
        };
        let Ok(bytes) = pack.bytes_at(offset, u64_from_usize(head_len)) else {
            return false;
        };
        let Ok(frame) = FrameHeader::decode(&mut bytes.reader()) else {
            return false;
        };
        let head = bytes
            .as_slice()
            .get(FRAME_HEADER_LEN..)
            .and_then(|h| layout::decode_chunk_head(h).ok());
        frame.kind() == FrameKind::BlobChunk
            && frame.payload_len() == u64_from_usize(layout::CHUNK_HEAD_LEN).saturating_add(len)
            && head.is_some_and(|h| {
                h.upload_id == manifest.upload_id()
                    && h.index == index
                    && u64::from(h.len) == len
                    && h.crc == crc
            })
            && whole_frame_valid(pack, offset, file_len)
    })
}

/// Whether the frame at `offset` passes every check of SPEC §4.4, its CRC over the whole payload included, read in
/// bounded pieces: a committed chunk frame is otherwise only checked when it is read, and a roll-forward must not
/// publish a blob a later read finds damaged.
fn whole_frame_valid<F: ContainerFile>(pack: &Pack<F>, offset: u64, file_len: u64) -> bool {
    let mut read = |at, buf: &mut [u8]| pack.file.read_exact_at(buf, at);
    matches!(
        StreamScanner::new(offset, file_len).check_frame(&mut read),
        Ok(Ok(header)) if header.offset() == offset
    )
}
