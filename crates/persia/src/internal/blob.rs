//! `put_blob`, `get_blob` and `get_blob_range` over a [`BlobStore`] (SPEC §9.2, §9.5, §9.6; ADR-0034, ADR-0051).
//!
//! [`Blobs`] is the writer's handle on its blob store: the store, the pin set GC honors (SPEC §9.5) and the source of
//! upload ids. [`Blobs::put_blob`] reads a blob, stages its chunks under a fresh upload id, and binds it once its id
//! is known and checked; what it returns is durable (the store's `bind` answered, SPEC §18.9 as decided for
//! ROADMAP 9.2.2) and pinned until the caller drops the returned [`Pins`].

use core::ops::Range;
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::sync::{Arc, Mutex, PoisonError};

use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobRef, BlobStore, Chunk, ChunkSize, Error, Result,
    SMALL_BLOB_MAX, UploadId, blob_id_of, read_range,
};
use persia_engine::db::SegmentIds;

use super::pins::{PinSet, Pinned, Pins};

/// The MIME type of a blob whose caller names none.
pub const DEFAULT_MIME: &str = "application/octet-stream";

/// The bytes of chunks `put_blob` stages in one [`BlobStore::stage_chunks`] call, and `get_blob` reads in one
/// [`BlobStore::read_chunks`] call, at most (at least one chunk): what bounds their memory beside one chunk.
pub const WINDOW_BYTES: u64 = 32 << 20;

/// The most chunks in one window, whatever their size.
pub const WINDOW_MAX_CHUNKS: u32 = 64;

/// How many times `put_blob` calls `bind` with the same manifest while the outcome is unknown, by default (ADR-0051).
pub const DEFAULT_BIND_ATTEMPTS: u32 = 16;

/// What `put_blob` stores beside the bytes (SPEC §9.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutBlobOptions {
    /// The blob's MIME type: 1..=255 bytes of printable ASCII (SPEC §9.1). [`DEFAULT_MIME`] by default.
    pub mime: String,
    /// The blob's user metadata (SPEC §9.1 limits). Searchable metadata belongs in document fields.
    pub user_meta: BTreeMap<String, String>,
    /// The id the caller expects the bytes to hash to; a mismatch is [`Error::BlobIdMismatch`] and binds nothing.
    pub expected_id: Option<BlobId>,
    /// The chunk size to store a new blob with, at least [`ChunkSize::MIN`] (SPEC §9.2); 4 MiB by default. A
    /// duplicate keeps the bound blob's.
    pub chunk_size: ChunkSize,
    /// How many times `bind` is called while its outcome is unknown before `put_blob` gives up (at least once).
    pub bind_attempts: u32,
}

impl Default for PutBlobOptions {
    fn default() -> Self {
        Self {
            mime: DEFAULT_MIME.to_owned(),
            user_meta: BTreeMap::new(),
            expected_id: None,
            chunk_size: ChunkSize::DEFAULT,
            bind_attempts: DEFAULT_BIND_ATTEMPTS,
        }
    }
}

/// The writer's blob store with its pin set and upload ids (see the module docs).
///
/// ```
/// use std::sync::Arc;
/// use persia::internal::blob::{Blobs, PutBlobOptions};
/// use persia_blob::{BlobStore, MemoryBlobStore, blob_id_of};
///
/// let blobs = Blobs::new(Arc::new(MemoryBlobStore::new()));
/// let (blob, pins) = blobs.put_blob(&b"hello, blobs"[..], &PutBlobOptions::default())?;
/// assert_eq!(blob.blob_id(), blob_id_of(b"hello, blobs"));
/// assert!(blobs.store().contains(&blob)?);
/// assert_eq!(blobs.get_blob_range(&blob, 7..12)?, b"blobs");
/// drop(pins); // nothing references the blob: GC may now collect it
/// # Ok::<(), persia_blob::Error>(())
/// ```
#[derive(Debug)]
pub struct Blobs<S: ?Sized> {
    pins: Arc<PinSet>,
    ids: Mutex<SegmentIds>,
    store: Arc<S>,
}

impl<S: BlobStore + ?Sized> Blobs<S> {
    /// The handle on `store`, drawing upload ids at random (SPEC §9.2: like a segment id, §8.7).
    pub fn new(store: Arc<S>) -> Self {
        Self::with_upload_ids(store, SegmentIds::random())
    }

    /// The handle on `store`, drawing upload ids from `ids` ([`SegmentIds::seeded`] in tests).
    pub fn with_upload_ids(store: Arc<S>, ids: SegmentIds) -> Self {
        Self {
            pins: Arc::new(PinSet::new()),
            ids: Mutex::new(ids),
            store,
        }
    }

    /// The store.
    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    /// The pin set GC honors (SPEC §9.5).
    pub fn pin_set(&self) -> &Arc<PinSet> {
        &self.pins
    }

    /// Stores the blob `reader` yields (SPEC §9.2, §9.6) and returns its reference, with the bound blob's chunk size
    /// for a duplicate, and the pins of its upload id and blob id, which keep GC off them until dropped (SPEC §9.5).
    ///
    /// The upload id is pinned before any byte is read. A blob of at most [`SMALL_BLOB_MAX`] bytes is hashed whole,
    /// then stored and bound by [`BlobStore::bind_small`]; a larger one is cut by a [`Chunker`](persia_blob::Chunker) and staged a window
    /// at a time ([`WINDOW_BYTES`]). Once the reader ends the id is checked against `expected_id`, pinned, and bound;
    /// a bind whose outcome is unknown is retried with the same manifest up to `bind_attempts` times, and its upload
    /// is never discarded. A duplicate's chunks, and a mismatched upload's, are discarded best-effort.
    ///
    /// # Errors
    /// - [`Error::ChunkSizeBelowMin`] for a chunk size below [`ChunkSize::MIN`], however it was built, and
    ///   [`Error::InvalidBlobRef`] for an invalid `mime` or `user_meta`, before any byte is read;
    /// - [`Error::Read`] when the reader fails, [`Error::InvalidChunking`] for a blob of more than `u32::MAX` chunks;
    /// - [`Error::BlobIdMismatch`] when the bytes do not hash to `expected_id` (nothing bound);
    /// - an error with [`Error::is_bind_outcome_unknown`] when every attempt left the outcome unknown: the blob may be
    ///   bound later, and nothing references it (its upload is left to GC);
    /// - [`Error::Corrupt`] when the manifest bound under the id does not name it or another size;
    /// - the store's errors, after which nothing this call returned references the blob.
    pub fn put_blob(
        &self,
        mut reader: impl Read,
        options: &PutBlobOptions,
    ) -> Result<(BlobRef, Pins)> {
        let chunk_size = options.chunk_size;
        if chunk_size.get() < ChunkSize::MIN {
            return Err(Error::ChunkSizeBelowMin {
                chunk_size: chunk_size.get(),
                min: ChunkSize::MIN,
            });
        }
        // The rules of `mime` and `user_meta` checked on an empty blob, so a bad option reads and stages nothing.
        reference(blob_id_of(&[]), 0, chunk_size, options)?;
        let upload_id = UploadId::new(self.next_upload_id());
        // SPEC §9.5: pinned before the first chunk write.
        let mut pins = self.pins.pin([Pinned::Upload(upload_id)]);

        let mut head = Vec::new();
        (&mut reader)
            .take(SMALL_BLOB_MAX.saturating_add(1))
            .read_to_end(&mut head)
            .map_err(Error::Read)?;
        let small = u64::try_from(head.len()).is_ok_and(|len| len <= SMALL_BLOB_MAX);
        let (manifest, outcome) = if small {
            let (crcs, blob) = chunk_crcs(&head[..], chunk_size, |_| Ok(()))?;
            let manifest = self.checked_manifest(blob, upload_id, crcs, options)?;
            pins.add(Pinned::Blob(manifest.blob_id()));
            let outcome = Self::bind(options, || self.store.bind_small(&manifest, &head));
            (manifest, outcome)
        } else {
            let window = window_chunks(chunk_size);
            let mut staged: Vec<Chunk> = Vec::new();
            let bytes = io::Cursor::new(head).chain(reader);
            let (crcs, blob) = chunk_crcs(bytes, chunk_size, |chunk| {
                staged.push(chunk);
                if staged.len() >= window {
                    self.stage(upload_id, &mut staged)?;
                }
                Ok(())
            })?;
            self.stage(upload_id, &mut staged)?;
            let manifest = self.checked_manifest(blob, upload_id, crcs, options)?;
            pins.add(Pinned::Blob(manifest.blob_id()));
            let outcome = Self::bind(options, || self.store.bind(&manifest));
            (manifest, outcome)
        };
        let blob_ref = self.bound(&manifest, outcome, options)?;
        Ok((blob_ref, pins))
    }

    /// Pins the blob `blob_ref` refers to, then checks that this store holds it ([`BlobStore::contains`]): what
    /// `put(doc, blob)` does with a `BlobRef` it was given, before it logs the document (SPEC §9.6). `None` when the
    /// store does not hold it (and nothing stays pinned).
    ///
    /// # Errors
    /// As [`BlobStore::contains`]: [`Error::Corrupt`] when the manifest bound under the id does not match `blob_ref`.
    pub fn pin_contained(&self, blob_ref: &BlobRef) -> Result<Option<Pins>> {
        // Pinned before the lookup (SPEC §9.5), so GC cannot sweep it between the check and the document's log.
        let pins = self.pins.pin([Pinned::Blob(blob_ref.blob_id())]);
        Ok(self.store.contains(blob_ref)?.then_some(pins))
    }

    /// Bytes `range` of the blob `blob_ref` refers to (SPEC §9.6), every chunk read checked ([`read_range`]).
    ///
    /// # Errors
    /// As [`read_range`]: [`Error::BlobNotFound`], [`Error::RangeOutOfBounds`], [`Error::Corrupt`], the store's.
    pub fn get_blob_range(&self, blob_ref: &BlobRef, range: Range<u64>) -> Result<Vec<u8>> {
        read_range(&*self.store, blob_ref, range)
    }

    /// A reader of the whole blob `blob_ref` refers to (SPEC §9.6 `get_blob`), which reads it a window of chunks at
    /// a time ([`WINDOW_BYTES`]), each checked, never the whole blob at once.
    ///
    /// # Errors
    /// [`Error::BlobNotFound`] when the id is not bound, [`Error::Corrupt`] when its manifest does not match
    /// `blob_ref`, or the store's errors. Errors while reading come from [`Read::read`] as [`io::Error`]s whose
    /// inner error is the [`Error`].
    pub fn get_blob(&self, blob_ref: &BlobRef) -> Result<BlobReader<'_, S>> {
        let Some(manifest) = self.store.manifest(blob_ref.blob_id())? else {
            return Err(Error::BlobNotFound {
                blob_id: blob_ref.blob_id(),
            });
        };
        manifest.check_ref(blob_ref)?;
        let chunk = u64::from(manifest.chunk_size().get());
        let window = chunk.saturating_mul(u64::from(window_chunk_count(manifest.chunk_size())));
        Ok(BlobReader {
            store: &*self.store,
            blob_ref: blob_ref.clone(),
            window,
            offset: 0,
            buf: Vec::new(),
            pos: 0,
        })
    }

    fn next_upload_id(&self) -> u128 {
        // The generator's state is one counter: a panic elsewhere leaves it usable.
        self.ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_id()
    }

    /// Stages `chunks` (consecutive, from the first one's index) in one call, then empties it.
    fn stage(&self, upload_id: UploadId, chunks: &mut Vec<Chunk>) -> Result<()> {
        let Some(first) = chunks.first().map(Chunk::index) else {
            return Ok(());
        };
        let bytes: Vec<&[u8]> = chunks.iter().map(Chunk::bytes).collect();
        // A failed stage leaves the upload's chunks: GC (or a resume, ROADMAP 9.3.2) takes them.
        self.store.stage_chunks(upload_id, first, &bytes)?;
        chunks.clear();
        Ok(())
    }

    /// The blob's manifest once its id is checked against `expected_id`; a mismatch discards the upload.
    fn checked_manifest(
        &self,
        blob: persia_blob::ChunkedBlob,
        upload_id: UploadId,
        crcs: Vec<u32>,
        options: &PutBlobOptions,
    ) -> Result<BlobManifest> {
        if let Some(expected) = options.expected_id
            && expected != blob.blob_id
        {
            self.discard(upload_id);
            return Err(Error::BlobIdMismatch {
                expected,
                actual: blob.blob_id,
            });
        }
        BlobManifest::new(blob.blob_id, blob.size, blob.chunk_size, upload_id, crcs)
    }

    /// `bind` called until its outcome is known, at most `options.bind_attempts` times; with whether an outcome was
    /// ever unknown, after which the upload is never discarded (SPEC §9.4).
    fn bind(
        options: &PutBlobOptions,
        bind: impl Fn() -> Result<BindOutcome>,
    ) -> (Result<BindOutcome>, bool) {
        let mut unknown = false;
        let mut attempts = options.bind_attempts.max(1);
        loop {
            match bind() {
                Err(e) if e.is_bind_outcome_unknown() => {
                    unknown = true;
                    attempts = attempts.saturating_sub(1);
                    if attempts == 0 {
                        return (Err(e), true);
                    }
                }
                other => return (other, unknown),
            }
        }
    }

    /// The `BlobRef` of a bound blob, from `bind`'s outcome (SPEC §9.2: a duplicate carries the bound chunk size).
    fn bound(
        &self,
        manifest: &BlobManifest,
        (outcome, unknown): (Result<BindOutcome>, bool),
        options: &PutBlobOptions,
    ) -> Result<BlobRef> {
        match outcome? {
            BindOutcome::Bound => reference(
                manifest.blob_id(),
                manifest.size(),
                manifest.chunk_size(),
                options,
            ),
            BindOutcome::AlreadyBound(bound) => {
                bound.check_blob_id(manifest.blob_id())?;
                if bound.size() != manifest.size() {
                    return Err(Error::Corrupt {
                        encoding: "blob manifest",
                        what: "the manifest bound under the id has another size than the blob's bytes",
                    });
                }
                // The bound manifest may be this upload's own, from an attempt whose outcome was unknown: its chunks
                // are the blob's then. After an unknown outcome nothing is discarded anyway (SPEC §9.4).
                if !unknown && bound.upload_id() != manifest.upload_id() {
                    self.discard(manifest.upload_id());
                }
                reference(bound.blob_id(), bound.size(), bound.chunk_size(), options)
            }
        }
    }

    /// Discards `upload_id`'s chunks, best-effort (SPEC §9.2: GC is the backstop).
    fn discard(&self, upload_id: UploadId) {
        if let Err(error) = self.store.discard_upload(upload_id) {
            tracing::warn!(%upload_id, %error, "discarding a blob upload failed; GC will reclaim its chunks");
        }
    }
}

/// The CRC32C of each chunk of the blob `reader` yields, and its summary; `sink` gets each chunk in order.
fn chunk_crcs(
    reader: impl Read,
    chunk_size: ChunkSize,
    mut sink: impl FnMut(Chunk) -> Result<()>,
) -> Result<(Vec<u32>, persia_blob::ChunkedBlob)> {
    let mut crcs = Vec::new();
    let blob = persia_blob::chunk_all(reader, chunk_size, |chunk| {
        crcs.push(chunk.crc32c());
        sink(chunk)
    })?;
    Ok((crcs, blob))
}

/// The `BlobRef` of the blob `blob_id` with `options`' `mime` and `user_meta`.
fn reference(
    blob_id: BlobId,
    size: u64,
    chunk_size: ChunkSize,
    options: &PutBlobOptions,
) -> Result<BlobRef> {
    BlobRef::new(
        blob_id,
        size,
        options.mime.clone(),
        chunk_size.get(),
        options.user_meta.clone(),
    )
    .map_err(Error::InvalidBlobRef)
}

/// How many chunks of `chunk_size` make a window: [`WINDOW_BYTES`] of them, at least one, at most
/// [`WINDOW_MAX_CHUNKS`].
fn window_chunk_count(chunk_size: ChunkSize) -> u32 {
    u32::try_from(WINDOW_BYTES / u64::from(chunk_size.get()))
        .unwrap_or(WINDOW_MAX_CHUNKS)
        .clamp(1, WINDOW_MAX_CHUNKS)
}

fn window_chunks(chunk_size: ChunkSize) -> usize {
    usize::try_from(window_chunk_count(chunk_size)).unwrap_or(1)
}

/// A blob being read (from [`Blobs::get_blob`]): one window of checked chunks in memory at a time.
#[derive(Debug)]
pub struct BlobReader<'a, S: ?Sized> {
    store: &'a S,
    blob_ref: BlobRef,
    /// Bytes per window: a whole number of chunks.
    window: u64,
    /// Where the next window starts.
    offset: u64,
    buf: Vec<u8>,
    pos: usize,
}

impl<S: ?Sized> BlobReader<'_, S> {
    /// The reference of the blob being read.
    pub fn blob_ref(&self) -> &BlobRef {
        &self.blob_ref
    }
}

impl<S: BlobStore + ?Sized> Read for BlobReader<'_, S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            let size = self.blob_ref.size();
            if self.offset >= size || out.is_empty() {
                return Ok(0);
            }
            let end = self.offset.saturating_add(self.window).min(size);
            self.buf = read_range(self.store, &self.blob_ref, self.offset..end)
                .map_err(io::Error::other)?;
            self.offset = end;
            self.pos = 0;
        }
        let rest = self.buf.get(self.pos..).unwrap_or_default();
        let n = rest.len().min(out.len());
        if let (Some(to), Some(from)) = (out.get_mut(..n), rest.get(..n)) {
            to.copy_from_slice(from);
        }
        self.pos = self.pos.saturating_add(n);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_a_whole_number_of_chunks_within_its_bounds() {
        let count = |c| window_chunk_count(ChunkSize::new(c).unwrap());
        assert_eq!(count(4 << 20), 8);
        assert_eq!(count(ChunkSize::MAX), 1);
        assert_eq!(count(ChunkSize::MIN), WINDOW_MAX_CHUNKS);
    }
}
