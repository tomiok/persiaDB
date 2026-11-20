//! The checksum scan of the pack that `persia verify` runs by default (SPEC §9.5; ROADMAP 9.5.3; ADR-0053).

use std::collections::HashSet;

use persia_format::checksum::crc32c;

use super::{
    ContainerFile, FIRST_FRAME_OFFSET, FRAME_HEADER_LEN, FrameHeader, FrameKind, PackStore, Result,
    at, corrupt_at, decode_chunk_payload, decode_frame, decode_index_payload, from_format_at,
    u64_from_usize,
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
    /// `BlobIndex` frames among them, each manifest decoded.
    pub indexes: u64,
    /// The bytes of those frames.
    pub bytes: u64,
}

impl<F: ContainerFile + 'static> PackStore<F> {
    /// Reads every committed frame of the pack (`12288..log_end`) whole and checks it (SPEC §9.5, ADR-0053): the frame
    /// itself (SPEC §4.4: its header and the CRC over its payload), then a `BlobChunk` frame's head and its bytes
    /// against the CRC32C it stores, orphan chunks included, and a `BlobIndex` frame's manifest and chunk offsets.
    /// What reads check only for the chunks they read, and an open does not check at all. `None` when there is no
    /// pack. The frames after `log_end` (chunks this handle staged, never acknowledged) are not read.
    ///
    /// One frame in memory at a time; the read side is held throughout, so a writer's bind waits to publish.
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
    /// [`Error::CorruptAt`](crate::Error::CorruptAt) naming the first damaged frame; [`Error::PackUnsupported`](
    /// crate::Error::PackUnsupported) for unknown frame flags; [`Error::PackIo`](crate::Error::PackIo).
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
            let bytes = pack.bytes_at(offset, total)?;
            match header.kind() {
                FrameKind::BlobChunk => {
                    let (_, payload) =
                        decode_frame(bytes.as_slice(), offset, FrameKind::BlobChunk)?;
                    let (chunk, data) = decode_chunk_payload(payload).map_err(at(offset))?;
                    if crc32c(data) != chunk.crc {
                        return Err(corrupt_at(
                            "a chunk whose bytes do not match the CRC32C it stores",
                            offset,
                        ));
                    }
                    scan.chunks = scan.chunks.saturating_add(1);
                    if !named.contains(&offset) {
                        scan.orphan_chunks = scan.orphan_chunks.saturating_add(1);
                    }
                }
                FrameKind::BlobIndex => {
                    let (_, payload) =
                        decode_frame(bytes.as_slice(), offset, FrameKind::BlobIndex)?;
                    decode_index_payload(payload, offset)?;
                    scan.indexes = scan.indexes.saturating_add(1);
                }
                _ => {
                    return Err(corrupt_at(
                        "a committed frame of a kind no pack holds",
                        offset,
                    ));
                }
            }
            scan.frames = scan.frames.saturating_add(1);
            scan.bytes = scan.bytes.saturating_add(total);
            // `check_fits` bounded the frame by `log_end`.
            offset = offset.saturating_add(total);
        }
        Ok(Some(scan))
    }
}
