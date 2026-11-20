//! The checksum scan of the pack that `persia verify` runs by default (SPEC §9.5; ROADMAP 9.5.3; ADR-0053).

use std::collections::HashSet;

use persia_format::checksum::crc32c;

use super::{
    BlobManifest, ChunkHead, ContainerFile, FIRST_FRAME_OFFSET, FRAME_HEADER_LEN, FrameHeader,
    FrameKind, PackStore, Result, at, corrupt_at, decode_chunk_payload, decode_frame,
    decode_index_payload, expected_chunk, from_format_at, layout, u64_from_usize,
};

/// What [`PackStore::scan_checksums`] checked: every committed frame of the pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct PackScan {
    /// Committed frames (`12288..log_end`), each read whole and checked.
    pub frames: u64,
    /// `BlobChunk` frames among them, each chunk's bytes checked against the CRC32C it stores.
    pub chunks: u64,
    /// Of those, the chunks no bound blob names: staged by a writer that never bound them, or a duplicate's (SPEC
    /// §9.3: orphans, which the pack rewrite drops).
    pub orphan_chunks: u64,
    /// `BlobIndex` frames among them, each manifest decoded and every chunk it names checked against it.
    pub indexes: u64,
    /// The bytes of those frames.
    pub bytes: u64,
}

impl<F: ContainerFile + 'static> PackStore<F> {
    /// Reads every committed frame of the pack (`12288..log_end`) whole and checks it (SPEC §9.5, ADR-0053): the frame
    /// itself (SPEC §4.4: its header and the CRC over its payload), then a `BlobChunk` frame's head and its bytes
    /// against the CRC32C it stores, orphan chunks included, and a `BlobIndex` frame's manifest and chunk offsets,
    /// each offset a chunk frame seen before it whose head is chunk `i` of that manifest: its upload, index `i`, the
    /// length of the manifest's chunk `i` and the manifest's CRC32C for it (the rule an open's roll-forward applies,
    /// ADR-0049 item 9). So a valid index resealed to name another blob's valid chunk is found, which an open does not
    /// check for committed frames, and a read only for the chunks it reads. `None` when there is no pack. The frames
    /// after `log_end` (chunks this handle staged, never acknowledged) are not read.
    ///
    /// One frame in memory at a time, and a frame no longer than its kind allows (a chunk frame of 64 MiB, an index
    /// naming every chunk frame seen so far), so a damaged length never sizes a read; plus the head of every chunk
    /// frame (40 bytes each). The read side is held throughout, so a writer's bind waits to publish.
    ///
    /// ```
    /// use persia_blob::pack::{PackStore, pack_path};
    /// use persia_blob::{BindOutcome, BlobManifest, BlobStore, ChunkSize, UploadId, blob_id_of};
    /// use persia_format::checksum::crc32c;
    /// use persia_storage::container::Container;
    ///
    /// # if cfg!(miri) { return Ok(()); } // a real pack file: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let container = Container::create(dir.path().join("app.persia"))?;
    /// let path = pack_path(&dir.path().join("app.persia"));
    /// let store = PackStore::open_writer(&path, &container.writer_proof().ok_or("read-only")?)?;
    /// assert_eq!(store.scan_checksums()?, None); // no pack yet
    /// let m = BlobManifest::new(blob_id_of(b"scanned"), 7, ChunkSize::new(ChunkSize::MIN)?, UploadId::new(1),
    ///     vec![crc32c(b"scanned")])?;
    /// store.stage_chunk(m.upload_id(), 0, b"scanned")?;
    /// assert_eq!(store.bind(&m)?, BindOutcome::Bound);
    /// let scan = store.scan_checksums()?.ok_or("a pack")?;
    /// assert_eq!((scan.frames, scan.chunks, scan.indexes, scan.orphan_chunks), (2, 1, 1, 0));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`Error::CorruptAt`](crate::Error::CorruptAt) naming the first damaged frame (for an index naming a chunk that
    /// is not the manifest's, the index frame); [`Error::PackUnsupported`](crate::Error::PackUnsupported) for unknown
    /// frame flags; [`Error::PackIo`](crate::Error::PackIo).
    pub fn scan_checksums(&self) -> Result<Option<PackScan>> {
        let inner = self.read();
        let Some(pack) = &inner.pack else {
            return Ok(None);
        };
        let named: HashSet<u64> = pack
            .index
            .values()
            .flat_map(|e| e.chunks.iter().copied())
            .collect();
        let log_end = pack.sb.log_end;
        let head_len = u64_from_usize(FRAME_HEADER_LEN);
        let mut scan = PackScan::default();
        // Every chunk frame's head by offset, in offset order (the scan is sequential): what an index is checked by.
        let mut heads: Vec<(u64, ChunkHead)> = Vec::new();
        let mut offset = FIRST_FRAME_OFFSET;
        while offset < log_end {
            if offset.checked_add(head_len).is_none_or(|end| end > log_end) {
                return Err(corrupt_at(
                    "the committed frames end inside a frame",
                    offset,
                ));
            }
            let head = pack.bytes_at(offset, head_len)?;
            let header =
                FrameHeader::decode(&mut head.reader()).map_err(|e| from_format_at(e, offset))?;
            header
                .check_fits(log_end)
                .map_err(|e| from_format_at(e, offset))?;
            let total = header.total_len();
            let longest = match header.kind() {
                FrameKind::BlobChunk => layout::chunk_frame_len(layout::MAX_CHUNK_LEN),
                FrameKind::BlobIndex => max_index_frame_len(u64_from_usize(heads.len())),
                _ => {
                    return Err(corrupt_at(
                        "a committed frame of a kind no pack holds",
                        offset,
                    ));
                }
            };
            // Before the read: a damaged length must not size an allocation.
            if total > longest {
                return Err(corrupt_at(
                    "a committed frame longer than any frame of its kind",
                    offset,
                ));
            }
            let bytes = pack.bytes_at(offset, total)?;
            if header.kind() == FrameKind::BlobChunk {
                let (_, payload) = decode_frame(bytes.as_slice(), offset, FrameKind::BlobChunk)?;
                let (chunk, data) = decode_chunk_payload(payload).map_err(at(offset))?;
                if crc32c(data) != chunk.crc {
                    return Err(corrupt_at(
                        "a chunk whose bytes do not match the CRC32C it stores",
                        offset,
                    ));
                }
                heads.push((offset, chunk));
                scan.chunks = scan.chunks.saturating_add(1);
                if !named.contains(&offset) {
                    scan.orphan_chunks = scan.orphan_chunks.saturating_add(1);
                }
            } else {
                let (_, payload) = decode_frame(bytes.as_slice(), offset, FrameKind::BlobIndex)?;
                let (manifest, chunks) = decode_index_payload(payload, offset)?;
                if !index_matches(&heads, &manifest, &chunks) {
                    return Err(corrupt_at(
                        "an index names a chunk frame that is not its manifest's chunk",
                        offset,
                    ));
                }
                scan.indexes = scan.indexes.saturating_add(1);
            }
            scan.frames = scan.frames.saturating_add(1);
            scan.bytes = scan.bytes.saturating_add(total);
            // `check_fits` bounded the frame by `log_end`.
            offset = offset.saturating_add(total);
        }
        Ok(Some(scan))
    }
}

/// Whether every chunk frame `chunks` names is in `heads` and is chunk `i` of `manifest`: the upload, index `i`, the
/// length of chunk `i` and its CRC32C in the manifest. The frame's bytes were checked against that CRC when it was
/// scanned.
fn index_matches(heads: &[(u64, ChunkHead)], manifest: &BlobManifest, chunks: &[u64]) -> bool {
    chunks.iter().zip(0_u32..).all(|(&offset, index)| {
        let found = heads
            .binary_search_by_key(&offset, |&(o, _)| o)
            .ok()
            .and_then(|i| heads.get(i));
        match (found, expected_chunk(manifest, index)) {
            (Some((_, head)), Ok(expected)) => *head == expected,
            _ => false,
        }
    })
}

/// The longest `BlobIndex` frame that can name at most `chunks` chunk frames (SPEC §9.3): an 8-byte head, the
/// largest manifest of that many chunks (`76 + 4·n`, or for one chunk an inline manifest of `80 + 64 MiB`, SPEC
/// §9.1.1) padded to 8, and 8 bytes per offset. An index names distinct chunk frames before it, so `chunks` is the
/// number of chunk frames scanned so far. Saturates: a bound that large is never reached by a frame within the file.
fn max_index_frame_len(chunks: u64) -> u64 {
    let plain = chunks.saturating_mul(4).saturating_add(76);
    let inline = if chunks >= 1 {
        u64::from(layout::MAX_CHUNK_LEN).saturating_add(80)
    } else {
        0
    };
    let manifest = plain.max(inline).saturating_add(7) & !7;
    let payload = u64_from_usize(layout::INDEX_HEAD_LEN)
        .saturating_add(manifest)
        .saturating_add(chunks.saturating_mul(8));
    persia_format::frame::frame_len(payload).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_index_frame_bounds_every_encoded_one() {
        use crate::{ChunkSize, UploadId, blob_id_of};
        for n in [0_u32, 1, 2, 3, 17, 1000] {
            let size = u64::from(n) * u64::from(ChunkSize::MIN);
            let m = BlobManifest::new(
                blob_id_of(if n == 0 { b"" } else { b"x" }),
                size,
                ChunkSize::new(ChunkSize::MIN).unwrap(),
                UploadId::new(1),
                vec![0; persia_format::widen::usize_from_u32(n)],
            )
            .unwrap();
            let offsets: Vec<u64> = (0..u64::from(n)).map(|i| 12288 + 8 * i).collect();
            let frame = layout::encode_index_frame(1, &m, &offsets).unwrap();
            assert!(
                u64_from_usize(frame.len()) <= max_index_frame_len(u64::from(n)),
                "{n}"
            );
        }
        assert_eq!(max_index_frame_len(0), layout::MIN_INDEX_FRAME_LEN);
        assert_eq!(max_index_frame_len(u64::MAX), u64::MAX);
    }

    use std::path::Path;
    use std::sync::Arc;

    use persia_storage::container::Container;
    use persia_storage::container::sim::SimFile;
    use persia_storage::vfs::Vfs;
    use persia_storage::vfs::sim::{SimFs, SimFsFile};

    use crate::{BindOutcome, BlobStore, ChunkSize, UploadId, blob_id_of};

    const PACK: &str = "/app.persia.blobs";

    /// A pack of `[orphan chunk][chunk][index]` on a simulated file system, the writer's handle open.
    fn pack() -> (SimFs, PackStore<SimFsFile>) {
        let fs = SimFs::new();
        let container = Container::create_on(SimFile::new(), [3; 16]).unwrap();
        let store = PackStore::open_writer_in(
            Arc::new(fs.clone()),
            PACK,
            &container.writer_proof().unwrap(),
        )
        .unwrap();
        store.stage_chunk(UploadId::new(1), 0, b"orphan").unwrap();
        let m = BlobManifest::new(
            blob_id_of(b"bound"),
            5,
            ChunkSize::new(ChunkSize::MIN).unwrap(),
            UploadId::new(2),
            vec![crc32c(b"bound")],
        )
        .unwrap();
        store.stage_chunk(m.upload_id(), 0, b"bound").unwrap();
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        (fs, store)
    }

    /// Writes `kind` and `payload_len` into the header of the first frame, and moves the handle's `log_end` far out, so
    /// the frame fits it: a damaged length the frame checks pass up to the read.
    fn claim(fs: &SimFs, store: &PackStore<SimFsFile>, kind: u8, payload_len: u64) {
        let file = fs.open(Path::new(PACK), true).unwrap();
        let at = FIRST_FRAME_OFFSET;
        file.write_all_at(&[kind], at + 4).unwrap();
        file.write_all_at(&payload_len.to_le_bytes(), at + 8)
            .unwrap();
        store.write().pack.as_mut().unwrap().sb.log_end = 1 << 40;
    }

    /// A chunk frame claiming more than 64 MiB, or an index frame longer than one naming every chunk frame before it,
    /// is corrupt at its offset before anything is read: no allocation is sized by it (ROADMAP 9.5.3).
    #[test]
    fn a_frame_longer_than_its_kind_allows_is_corrupt_before_its_read() {
        for (kind, payload_len) in [
            (FrameKind::BlobChunk.to_u8(), 80 << 20),
            (FrameKind::BlobIndex.to_u8(), 200),
        ] {
            let (fs, store) = pack();
            claim(&fs, &store, kind, payload_len);
            let err = store.scan_checksums().unwrap_err();
            assert!(
                matches!(
                    err,
                    crate::Error::CorruptAt {
                        what: "a committed frame longer than any frame of its kind",
                        offset: FIRST_FRAME_OFFSET,
                        ..
                    }
                ),
                "kind {kind}: {err}"
            );
        }
    }
}
