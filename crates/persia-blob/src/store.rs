//! The contract between the code that writes and reads blobs (`put_blob`, `get_blob_range`, SPEC §9.6) and the
//! places blobs are kept: the local pack file (SPEC §9.3) and the remote layout (SPEC §9.4).
//!
//! A [`BlobStore`] knows how to stage a chunk under an upload id, bind a verified blob manifest, find a bound
//! manifest, and read a staged chunk back. Everything above that (chunking, hashing, the expected id, deduplication,
//! the pin set) is the caller's, written once against this trait (SPEC §9.2, §9.5; ADR-0034).
//!
//! Durability follows SPEC §18.9 as decided for ROADMAP 9.2.2: a blob is durable when [`BlobStore::bind`]
//! returns [`BindOutcome::Bound`]. A store that buffers writes syncs them in `bind`, before it answers, so the
//! caller needs no separate sync, and a document that references the blob may be logged as soon as `bind` returns.

use core::ops::Range;

use persia_format::checksum::crc32c;
use persia_format::widen::{u64_from_usize, usize_from_u32};
use persia_format::{BlobId, BlobRef};

use crate::error::{Error, Result};
use crate::manifest::{BlobManifest, UploadId};

#[cfg(any(test, feature = "testing"))]
mod memory;
#[cfg(any(test, feature = "testing"))]
pub use memory::MemoryBlobStore;

/// The encoding named by errors about a chunk's bytes.
const CHUNK: &str = "blob chunk";

/// What [`BlobStore::bind`] found (SPEC §9.2, §9.4).
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "AlreadyBound means the blob is another upload's: return a BlobRef with its chunk size"]
pub enum BindOutcome {
    /// The manifest is durable and binds its id: the upload's staged chunks are now the blob's.
    Bound,
    /// Another upload bound the id first, so this one is a duplicate (SPEC §9.2). Holds the bound manifest, already
    /// checked to name the id ([`BlobManifest::check_blob_id`]); its chunk size is the one the returned `BlobRef`
    /// carries. The duplicate's staged chunks are the caller's to discard ([`BlobStore::discard_upload`]).
    ///
    /// The bound manifest is **durable**: a store never answers `AlreadyBound` (nor shows a manifest through
    /// [`BlobStore::manifest`] or [`BlobStore::contains`]) for a binding whose own durable write has not finished,
    /// e.g. a pack commit still syncing on another thread, so a document logged on this answer never references a
    /// blob a crash could lose (SPEC §9.5).
    AlreadyBound(BlobManifest),
}

/// A place blobs are kept (SPEC §9.3 locally, §9.4 remotely). Synchronous, like the engine: a remote store wraps a
/// synchronous [`persia_storage::Storage`].
///
/// Every method is safe to call from several threads; the writer that stages and binds is one process (the
/// container's writer, SPEC §9.5), but readers may be many.
pub trait BlobStore: Send + Sync {
    /// Stages chunk `index` of the upload `upload_id` (SPEC §9.2: under the upload id, never under a `blob_id`).
    /// The caller has checked `bytes` against the chunking rules (every chunk but the last is full).
    ///
    /// A staged chunk is never overwritten: staging the same `(upload_id, index)` again with the same bytes
    /// succeeds, with other bytes it is [`Error::StagedChunkConflict`] and changes nothing (SPEC §9.4).
    ///
    /// # Errors
    /// [`Error::StagedChunkConflict`], or the store's own errors (I/O, storage).
    fn stage_chunk(&self, upload_id: UploadId, index: u32, bytes: &[u8]) -> Result<()>;

    /// Stages consecutive chunks `first_index..` of the upload `upload_id`, as [`BlobStore::stage_chunk`] does each:
    /// `Ok` once every one is acknowledged. A store may write them in parallel (ROADMAP 9.3.2); on an error some may
    /// be staged and others not. The caller bounds how many it passes at once, which bounds its memory.
    ///
    /// # Errors
    /// The first error of a chunk's write.
    fn stage_chunks(&self, upload_id: UploadId, first_index: u32, chunks: &[&[u8]]) -> Result<()> {
        let mut index = first_index;
        for bytes in chunks {
            self.stage_chunk(upload_id, index, bytes)?;
            index = index.checked_add(1).ok_or(Error::InvalidManifest {
                what: "more than u32::MAX chunks",
            })?;
        }
        Ok(())
    }

    /// Binds `manifest`'s id to the chunks staged under its upload id (SPEC §9.2). The caller calls it only after
    /// every `stage_chunk` of those chunks returned `Ok`, and only with a verified id (computed from the bytes).
    ///
    /// [`BindOutcome::Bound`] means the blob is durable: locally its `BlobIndex` frame is published by a synced
    /// superblock commit of the pack (SPEC §9.3, §18.9), remotely `put_if_absent` of its manifest answered `Created`
    /// (SPEC §9.4). [`BindOutcome::AlreadyBound`] means the id was bound before; nothing was written.
    ///
    /// # Errors
    /// - [`Error::is_bind_outcome_unknown`]: the manifest may or may not have been bound (SPEC §9.4: a conditional
    ///   write still in flight may land later). The caller calls `bind` again with the same manifest, with backoff,
    ///   until it gets an answer or its time budget runs out (SPEC §9.4: then `put_blob` fails, binds nothing it
    ///   knows of, and leaves the upload to GC), and never discards this upload's chunks;
    /// - [`Error::Corrupt`] when the bound manifest found does not decode or names another blob;
    /// - the store's own errors, after which the blob is not bound.
    fn bind(&self, manifest: &BlobManifest) -> Result<BindOutcome>;

    /// Stores and binds a small blob in one step: `bytes` is the whole blob, at most [`SMALL_BLOB_MAX`] bytes, already
    /// hashed, so `manifest` (one chunk, or none when empty) names a verified id and nothing needs staging first
    /// (ADR-0034 item 8, ROADMAP 9.3.3). The outcomes and errors are [`BlobStore::bind`]'s. A store with a
    /// single-object form (the remote layout's inline manifest) writes one object; the default stages the chunk under
    /// the manifest's upload id, then binds, so the caller pins that upload id first either way (SPEC §9.5).
    ///
    /// After an unknown outcome the caller retries with `bind_small` and the same bytes, never with [`BlobStore::bind`]:
    /// a store's single-object form stages nothing, so a plain `bind` could bind a manifest with no chunk behind it.
    ///
    /// # Errors
    /// [`BlobStore::bind`]'s, and [`Error::InvalidManifest`] when `bytes` is over [`SMALL_BLOB_MAX`] or is not the
    /// blob `manifest` describes (its length or CRC32C differ).
    fn bind_small(&self, manifest: &BlobManifest, bytes: &[u8]) -> Result<BindOutcome> {
        check_small(manifest, bytes)?;
        if !bytes.is_empty() {
            self.stage_chunk(manifest.upload_id(), 0, bytes)?;
        }
        self.bind(manifest)
    }

    /// The manifest bound to `blob_id`, checked to name it ([`BlobManifest::check_blob_id`]); `None` when the id is
    /// not bound (locally also when there is no pack, SPEC §9.3).
    ///
    /// # Errors
    /// [`Error::Corrupt`] when the stored manifest does not decode or names another blob, or the store's errors.
    fn manifest(&self, blob_id: BlobId) -> Result<Option<BlobManifest>>;

    /// The raw bytes of chunk `index` of the blob `manifest` describes (staged under its upload id). A store may
    /// check them; [`read_chunk_checked`] always does, so callers use that.
    ///
    /// # Errors
    /// [`Error::Corrupt`] when the chunk is missing or damaged, or the store's errors.
    fn read_chunk(&self, manifest: &BlobManifest, index: u32) -> Result<Vec<u8>>;

    /// The raw bytes of chunks `indices` of the blob `manifest` describes, in index order, as
    /// [`BlobStore::read_chunk`] reads each. A store may read them in parallel (ROADMAP 9.3.2). [`read_range`]
    /// calls it once per range and checks every chunk.
    ///
    /// # Errors
    /// The first error of a chunk's read.
    fn read_chunks(&self, manifest: &BlobManifest, indices: Range<u32>) -> Result<Vec<Vec<u8>>> {
        indices
            .map(|index| self.read_chunk(manifest, index))
            .collect()
    }

    /// Drops the chunks staged under `upload_id`, best-effort (SPEC §9.2: a duplicate's or a mismatched upload's
    /// chunks are deleted remotely, GC is the backstop, and left to the pack rewrite locally, where this does
    /// nothing). Never called for an upload whose binding outcome was once unknown.
    ///
    /// # Errors
    /// The store's errors; the caller may ignore them, since GC reclaims what is left.
    fn discard_upload(&self, upload_id: UploadId) -> Result<()>;

    /// Whether the blob `blob_ref` refers to is bound here and its manifest matches it
    /// ([`BlobManifest::check_ref`]): what `put(doc, blob)` checks before it logs a document with a `BlobRef` it
    /// was given (SPEC §9.6).
    ///
    /// # Errors
    /// [`Error::Corrupt`] when a manifest is bound under the id but does not match `blob_ref`, or the store's errors.
    fn contains(&self, blob_ref: &BlobRef) -> Result<bool> {
        match self.manifest(blob_ref.blob_id())? {
            None => Ok(false),
            Some(manifest) => manifest.check_ref(blob_ref).map(|()| true),
        }
    }
}

/// The most bytes [`BlobStore::bind_small`] takes: 64 KiB, the chunk size floor (SPEC §9.2), so a small blob is at
/// most one chunk at any chunk size a writer uses (ROADMAP 9.3.3).
pub const SMALL_BLOB_MAX: u64 = 64 << 10;

/// Chunk `index` of the blob `manifest` describes, checked against it: its length against the chunking rules and its
/// CRC32C against the manifest's (SPEC §9.3, §9.4: a mismatch is corruption).
///
/// # Errors
/// [`Error::Corrupt`] when `index` is past the last chunk or the bytes do not match, or the store's errors.
pub fn read_chunk_checked<S: BlobStore + ?Sized>(
    store: &S,
    manifest: &BlobManifest,
    index: u32,
) -> Result<Vec<u8>> {
    expected_chunk(manifest, index)?;
    let bytes = store.read_chunk(manifest, index)?;
    check_chunk(manifest, index, &bytes)?;
    Ok(bytes)
}

/// Bytes `range` of the blob `blob_ref` refers to (SPEC §9.6 `get_blob_range`). Looks the manifest up, checks it
/// against `blob_ref` ([`BlobManifest::check_ref`]), then reads every chunk the range touches whole, in one
/// [`BlobStore::read_chunks`], and checks each as [`read_chunk_checked`] does. An empty range in bounds reads nothing.
///
/// # Errors
/// - [`Error::BlobNotFound`] when the id is not bound;
/// - [`Error::RangeOutOfBounds`] when `range` is inverted or ends past the blob;
/// - [`Error::Corrupt`] when the manifest does not match `blob_ref` or a chunk does not match the manifest;
/// - the store's errors.
pub fn read_range<S: BlobStore + ?Sized>(
    store: &S,
    blob_ref: &BlobRef,
    range: Range<u64>,
) -> Result<Vec<u8>> {
    let Some(manifest) = store.manifest(blob_ref.blob_id())? else {
        return Err(Error::BlobNotFound {
            blob_id: blob_ref.blob_id(),
        });
    };
    manifest.check_ref(blob_ref)?;
    read_manifest_range(store, &manifest, range)
}

/// Bytes `range` of the blob `manifest` describes, a manifest the caller already looked up and checked against the
/// `BlobRef` it reads by (as [`read_range`] does): a streaming reader keeps it and reads window after window with no
/// further lookup, so a blob of many small chunks costs one manifest read, not one per window (a manifest is
/// `76 + 4·n` bytes, SPEC §9.1.1). Reads every chunk the range touches whole, in one [`BlobStore::read_chunks`], and
/// checks each as [`read_chunk_checked`] does. An empty range in bounds reads nothing.
///
/// # Errors
/// - [`Error::RangeOutOfBounds`] when `range` is inverted or ends past the blob;
/// - [`Error::Corrupt`] when a chunk does not match the manifest;
/// - the store's errors.
pub fn read_manifest_range<S: BlobStore + ?Sized>(
    store: &S,
    manifest: &BlobManifest,
    range: Range<u64>,
) -> Result<Vec<u8>> {
    if range.start > range.end || range.end > manifest.size() {
        return Err(Error::RangeOutOfBounds {
            start: range.start,
            end: range.end,
            size: manifest.size(),
        });
    }
    let mut out = Vec::with_capacity(usize::try_from(range.end.abs_diff(range.start)).unwrap_or(0));
    if range.is_empty() {
        return Ok(out);
    }
    let chunk_size = u64::from(manifest.chunk_size().get());
    let index_of = |offset: u64| {
        u32::try_from(offset.checked_div(chunk_size).unwrap_or(0))
            .map_err(|_| corrupt("a chunk index does not fit in a u32"))
    };
    let first = index_of(range.start)?;
    let last = index_of(range.end.saturating_sub(1))?;
    let end = last
        .checked_add(1)
        .ok_or_else(|| corrupt("a chunk index does not fit in a u32"))?;
    for index in first..end {
        expected_chunk(manifest, index)?;
    }
    let chunks = store.read_chunks(manifest, first..end)?;
    if chunks.len() != usize_from_u32(end.abs_diff(first)) {
        return Err(corrupt(
            "a store returned another number of chunks than asked for",
        ));
    }
    for (index, bytes) in (first..end).zip(&chunks) {
        let chunk = check_chunk(manifest, index, bytes)?;
        let from = range.start.max(chunk.start).saturating_sub(chunk.start);
        let to = range.end.min(chunk.end).saturating_sub(chunk.start);
        let part = usize::try_from(from)
            .ok()
            .zip(usize::try_from(to).ok())
            .and_then(|(from, to)| bytes.get(from..to))
            .ok_or_else(|| corrupt("a chunk is shorter than its range"))?;
        out.extend_from_slice(part);
    }
    Ok(out)
}

/// The byte range and CRC32C chunk `index` must have; `Corrupt` past the last chunk.
fn expected_chunk(manifest: &BlobManifest, index: u32) -> Result<(Range<u64>, u32)> {
    match (
        manifest.chunk_range(index),
        manifest.chunk_crcs().get(usize_from_u32(index)),
    ) {
        (Some(range), Some(&crc)) => Ok((range, crc)),
        _ => Err(corrupt("a chunk past the blob's last one was asked for")),
    }
}

/// Checks chunk `index`'s bytes against `manifest`; returns its byte range in the blob.
fn check_chunk(manifest: &BlobManifest, index: u32, bytes: &[u8]) -> Result<Range<u64>> {
    let (range, crc) = expected_chunk(manifest, index)?;
    if range.end.checked_sub(range.start) != Some(u64_from_usize(bytes.len())) {
        return Err(corrupt("a chunk's length differs from its manifest's"));
    }
    if crc32c(bytes) != crc {
        return Err(corrupt("a chunk's CRC32C differs from its manifest's"));
    }
    Ok(range)
}

/// [`BlobStore::bind_small`]'s precondition: `bytes` is the whole blob `manifest` describes, and small.
pub(crate) fn check_small(manifest: &BlobManifest, bytes: &[u8]) -> Result<()> {
    let len = u64_from_usize(bytes.len());
    let invalid = |what| Err(Error::InvalidManifest { what });
    if len > SMALL_BLOB_MAX {
        return invalid("a small blob over SMALL_BLOB_MAX bytes");
    }
    if len != manifest.size() {
        return invalid("a small blob's bytes differ in length from its manifest's size");
    }
    if bytes.is_empty() {
        return Ok(());
    }
    if manifest.chunk_count() != 1 || manifest.chunk_crcs().first() != Some(&crc32c(bytes)) {
        return invalid("a small blob's bytes are not its manifest's single chunk");
    }
    Ok(())
}

const fn corrupt(what: &'static str) -> Error {
    Error::Corrupt {
        encoding: CHUNK,
        what,
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::{ChunkSize, blob_id_of};

    fn manifest_of(data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
        let chunk_size = ChunkSize::from_encoding(chunk).unwrap();
        let crcs = data.chunks(chunk as usize).map(crc32c).collect();
        BlobManifest::new(
            blob_id_of(data),
            data.len() as u64,
            chunk_size,
            UploadId::new(upload),
            crcs,
        )
        .unwrap()
    }

    fn reference(m: &BlobManifest) -> BlobRef {
        BlobRef::new(
            m.blob_id(),
            m.size(),
            "application/octet-stream",
            m.chunk_size().get(),
            BTreeMap::new(),
        )
        .unwrap()
    }

    fn stored(store: &MemoryBlobStore, data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
        let m = manifest_of(data, chunk, upload);
        for (i, c) in data.chunks(chunk as usize).enumerate() {
            store.stage_chunk(m.upload_id(), i as u32, c).unwrap();
        }
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        m
    }

    #[test]
    fn every_range_reads_the_bytes_it_covers() {
        let store = MemoryBlobStore::new();
        let data: Vec<u8> = (0..23u8).collect();
        let m = stored(&store, &data, 5, 1);
        let r = reference(&m);
        for start in 0..=23u64 {
            for end in start..=23u64 {
                let got = read_range(&store, &r, start..end).unwrap();
                assert_eq!(got, &data[start as usize..end as usize], "{start}..{end}");
            }
        }
    }

    #[test]
    fn a_range_past_the_end_or_inverted_is_rejected() {
        let store = MemoryBlobStore::new();
        let m = stored(&store, b"0123456789", 4, 1);
        let r = reference(&m);
        #[allow(clippy::reversed_empty_ranges)]
        for range in [0..11, 5..4, 11..11] {
            assert!(matches!(
                read_range(&store, &r, range),
                Err(Error::RangeOutOfBounds { .. })
            ));
        }
        assert_eq!(read_range(&store, &r, 10..10).unwrap(), b"");
    }

    #[test]
    fn an_empty_blob_binds_and_reads_nothing() {
        let store = MemoryBlobStore::new();
        let m = stored(&store, b"", 4, 1);
        assert_eq!(m.chunk_count(), 0);
        assert_eq!(read_range(&store, &reference(&m), 0..0).unwrap(), b"");
    }

    #[test]
    fn a_second_upload_of_the_same_bytes_is_a_duplicate() {
        let store = MemoryBlobStore::new();
        let first = stored(&store, b"same bytes", 4, 1);
        let second = manifest_of(b"same bytes", 3, 2);
        for (i, c) in b"same bytes".chunks(3).enumerate() {
            store.stage_chunk(second.upload_id(), i as u32, c).unwrap();
        }
        assert_eq!(
            store.bind(&second).unwrap(),
            BindOutcome::AlreadyBound(first.clone())
        );
        store.discard_upload(second.upload_id()).unwrap();
        assert_eq!(store.staged_chunk_count(second.upload_id()), 0);
        assert_eq!(store.staged_chunk_count(first.upload_id()), 3);
        assert!(store.contains(&reference(&first)).unwrap());
    }

    #[test]
    fn a_staged_chunk_is_never_overwritten() {
        let store = MemoryBlobStore::new();
        let upload = UploadId::new(9);
        store.stage_chunk(upload, 0, b"abcd").unwrap();
        store.stage_chunk(upload, 0, b"abcd").unwrap();
        assert!(matches!(
            store.stage_chunk(upload, 0, b"abce"),
            Err(Error::StagedChunkConflict { index: 0, .. })
        ));
        let m = manifest_of(b"abcd", 4, 9);
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        assert_eq!(read_range(&store, &reference(&m), 0..4).unwrap(), b"abcd");
    }

    #[test]
    fn an_unbound_id_is_not_found_and_not_contained() {
        let store = MemoryBlobStore::new();
        let r = reference(&manifest_of(b"never stored", 4, 1));
        assert!(!store.contains(&r).unwrap());
        assert!(matches!(
            read_range(&store, &r, 0..1),
            Err(Error::BlobNotFound { .. })
        ));
    }

    #[test]
    fn a_reference_that_differs_from_the_manifest_is_corrupt() {
        let store = MemoryBlobStore::new();
        let m = stored(&store, b"0123456789", 4, 1);
        let wrong_size = BlobRef::new(m.blob_id(), 11, "a/b", 4, BTreeMap::new()).unwrap();
        let wrong_chunk = BlobRef::new(m.blob_id(), 10, "a/b", 5, BTreeMap::new()).unwrap();
        for r in [wrong_size, wrong_chunk] {
            assert!(store.contains(&r).unwrap_err().is_corruption());
            assert!(read_range(&store, &r, 0..1).unwrap_err().is_corruption());
        }
    }

    #[test]
    fn a_chunk_that_differs_from_the_manifest_is_corrupt() {
        let store = MemoryBlobStore::new();
        let data = b"0123456789";
        // Staged with other bytes than the manifest's CRCs and length describe.
        let m = manifest_of(data, 4, 1);
        store.stage_chunk(m.upload_id(), 0, b"0123").unwrap();
        store.stage_chunk(m.upload_id(), 1, b"4567").unwrap();
        store.stage_chunk(m.upload_id(), 2, b"8").unwrap();
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        let r = reference(&m);
        assert_eq!(read_range(&store, &r, 0..8).unwrap(), b"01234567");
        assert!(read_range(&store, &r, 8..10).unwrap_err().is_corruption());
        let m2 = manifest_of(b"abcdefgh", 4, 2);
        store.stage_chunk(m2.upload_id(), 0, b"abcd").unwrap();
        store.stage_chunk(m2.upload_id(), 1, b"abcX").unwrap();
        assert_eq!(store.bind(&m2).unwrap(), BindOutcome::Bound);
        assert!(
            read_range(&store, &reference(&m2), 4..5)
                .unwrap_err()
                .is_corruption()
        );
    }

    #[test]
    fn a_small_blob_binds_in_one_step_and_reads_back() {
        let store = MemoryBlobStore::new();
        let data = vec![7_u8; SMALL_BLOB_MAX as usize];
        let m = manifest_of(&data, ChunkSize::MIN, 3);
        assert_eq!(store.bind_small(&m, &data).unwrap(), BindOutcome::Bound);
        assert_eq!(
            read_range(&store, &reference(&m), 0..m.size()).unwrap(),
            data
        );
        let empty = manifest_of(b"", ChunkSize::MIN, 4);
        assert_eq!(store.bind_small(&empty, b"").unwrap(), BindOutcome::Bound);
        assert_eq!(store.staged_chunk_count(empty.upload_id()), 0);
        assert_eq!(
            store.bind_small(&m, &data).unwrap(),
            BindOutcome::AlreadyBound(m.clone())
        );
    }

    #[test]
    fn bind_small_rejects_bytes_that_are_not_its_small_blob() {
        let store = MemoryBlobStore::new();
        let big = vec![1_u8; SMALL_BLOB_MAX as usize + 1];
        let m = manifest_of(&big, ChunkSize::MAX, 1);
        assert!(matches!(
            store.bind_small(&m, &big),
            Err(Error::InvalidManifest { .. })
        ));
        let m = manifest_of(b"abcd", 4, 2);
        for bytes in [&b"abc"[..], b"abce"] {
            assert!(matches!(
                store.bind_small(&m, bytes),
                Err(Error::InvalidManifest { .. })
            ));
        }
        assert_eq!(store.bound_ids(), Vec::new());
        assert_eq!(store.staged_chunk_count(m.upload_id()), 0);
    }

    #[test]
    fn stage_chunks_stages_each_from_its_first_index() {
        let store = MemoryBlobStore::new();
        let data = b"0123456789";
        let m = manifest_of(data, 4, 5);
        store
            .stage_chunks(m.upload_id(), 1, &[b"4567", b"89"])
            .unwrap();
        store.stage_chunks(m.upload_id(), 0, &[b"0123"]).unwrap();
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        assert_eq!(read_range(&store, &reference(&m), 0..10).unwrap(), data);
    }

    #[test]
    fn a_chunk_index_past_the_last_is_corrupt() {
        let store = MemoryBlobStore::new();
        let m = stored(&store, b"0123", 4, 1);
        assert!(
            read_chunk_checked(&store, &m, 1)
                .unwrap_err()
                .is_corruption()
        );
    }
}
