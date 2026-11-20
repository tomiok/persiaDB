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
use std::time::Duration;

use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobRef, BlobStore, Chunk, ChunkSize, Error, Result,
    SMALL_BLOB_MAX, UploadId, blob_id_of, read_manifest_range_into, read_range,
};
use persia_engine::db::SegmentIds;

use super::pins::{PinSet, Pinned, Pins};
use super::retry::{RetryClock, SystemRetryClock, backoff};

/// The MIME type of a blob whose caller names none.
pub const DEFAULT_MIME: &str = "application/octet-stream";

/// The bytes of chunks `put_blob` stages in one [`BlobStore::stage_chunks`] call, and `get_blob` reads in one
/// [`BlobStore::read_chunks`] call, at most (at least one chunk): what bounds their memory beside one chunk.
pub const WINDOW_BYTES: u64 = 32 << 20;

/// The most chunks in one window, whatever their size.
pub const WINDOW_MAX_CHUNKS: u32 = 64;

/// How long `put_blob` keeps retrying a bind whose outcome is unknown, by default (SPEC §9.4, ADR-0051).
pub const DEFAULT_BIND_BUDGET: Duration = Duration::from_secs(60);

/// The longest `bind_budget` `put_blob` accepts (SPEC §9.4). A cap holds across restarts, where a recorded maximum
/// would not. GC's grace period must be at least twice this, strictly above every budget: an attempt started just
/// before the deadline can land its `put_if_absent` after the budget ends, and a grace of only this long could sweep
/// those chunks while that write is still in flight. `gc` (ROADMAP 9.5.2, #108) rejects a shorter grace.
pub const MAX_BIND_BUDGET: Duration = Duration::from_hours(1);

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
    /// How long a bind whose outcome is unknown is retried, with backoff ([`super::retry::backoff`]), before
    /// `put_blob` fails with the unknown outcome; at least one attempt is made. [`DEFAULT_BIND_BUDGET`] by default, at
    /// most [`MAX_BIND_BUDGET`].
    pub bind_budget: Duration,
}

impl Default for PutBlobOptions {
    fn default() -> Self {
        Self {
            mime: DEFAULT_MIME.to_owned(),
            user_meta: BTreeMap::new(),
            expected_id: None,
            chunk_size: ChunkSize::DEFAULT,
            bind_budget: DEFAULT_BIND_BUDGET,
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
    clock: Arc<dyn RetryClock>,
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
            clock: Arc::new(SystemRetryClock::new()),
            store,
        }
    }

    /// The same handle, with bind retries timed and waited through `clock`
    /// ([`ManualRetryClock`](super::retry::ManualRetryClock) in tests).
    #[must_use]
    pub fn with_retry_clock(mut self, clock: Arc<dyn RetryClock>) -> Self {
        self.clock = clock;
        self
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
    /// a bind whose outcome is unknown is retried with the same manifest (with `bind_small` for a small blob), with
    /// backoff, within `bind_budget`, and its upload is never discarded. A duplicate's chunks, and a mismatched upload's, are discarded best-effort.
    ///
    /// # Errors
    /// - [`Error::ChunkSizeBelowMin`] for a chunk size below [`ChunkSize::MIN`], however it was built, and
    ///   [`Error::InvalidBlobRef`] for an invalid `mime` or `user_meta`, and [`Error::BindBudgetAboveMax`] for a
    ///   `bind_budget` above [`MAX_BIND_BUDGET`], before any byte is read;
    /// - [`Error::Read`] when the reader fails, [`Error::InvalidChunking`] for a blob of more than `u32::MAX` chunks;
    /// - [`Error::BlobIdMismatch`] when the bytes do not hash to `expected_id` (nothing bound);
    /// - an error with [`Error::is_bind_outcome_unknown`] when the outcome stayed unknown for the whole `bind_budget`: the blob may be
    ///   bound later, and nothing references it (its upload is left to GC). Also before the budget runs out when the
    ///   store refuses every write until it is reopened: that error has [`Error::is_store_poisoned`] too, and only
    ///   reopening the store tells whether the blob was bound (ROADMAP 9.4.7);
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
        if options.bind_budget > MAX_BIND_BUDGET {
            return Err(Error::BindBudgetAboveMax {
                budget: options.bind_budget,
                max: MAX_BIND_BUDGET,
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
            let manifest = self.checked_manifest(blob, upload_id, crcs, options, false)?;
            pins.add(Pinned::Blob(manifest.blob_id()));
            // An unknown outcome is retried with `bind_small` and the same bytes, never `bind` (the contract).
            let outcome = self.bind(upload_id, options, || {
                self.store.bind_small(&manifest, &head)
            });
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
            let manifest = self.checked_manifest(blob, upload_id, crcs, options, true)?;
            pins.add(Pinned::Blob(manifest.blob_id()));
            let outcome = self.bind(upload_id, options, || self.store.bind(&manifest));
            (manifest, outcome)
        };
        let blob_ref = self.bound(&manifest, outcome, options, &mut pins)?;
        Ok((blob_ref, pins))
    }

    /// Pins the blob `blob_ref` refers to, then checks that this store holds it ([`BlobStore::contains`]): what
    /// `put(doc, blob)` does with a `BlobRef` it was given, before it logs the document (SPEC §9.6). `None` when the
    /// store does not hold it (and nothing stays pinned).
    ///
    /// # Errors
    /// As [`BlobStore::contains`]: [`Error::Corrupt`] when the manifest bound under the id does not match `blob_ref`.
    pub fn pin_contained(&self, blob_ref: &BlobRef) -> Result<Option<Pins>> {
        // Pinned before the lookup (SPEC §9.5), so GC cannot sweep it between the check and the document's log. The
        // lookup is `BlobStore::contains`'s (the manifest, checked against the reference), and pins the upload the
        // manifest names too, so its chunks are pinned beside it (ADR-0051 item 5).
        let mut pins = self.pins.pin([Pinned::Blob(blob_ref.blob_id())]);
        let Some(manifest) = self.store.manifest(blob_ref.blob_id())? else {
            return Ok(None);
        };
        manifest.check_ref(blob_ref)?;
        pins.add(Pinned::Upload(manifest.upload_id()));
        Ok(Some(pins))
    }

    /// Bytes `range` of the blob `blob_ref` refers to (SPEC §9.6), every chunk read checked ([`read_range`]). The
    /// blob is pinned while it is read, so a GC sweep cannot take it halfway (SPEC §9.5).
    ///
    /// # Errors
    /// As [`read_range`]: [`Error::BlobNotFound`], [`Error::RangeOutOfBounds`], [`Error::Corrupt`], the store's.
    pub fn get_blob_range(&self, blob_ref: &BlobRef, range: Range<u64>) -> Result<Vec<u8>> {
        // A blob pin alone protects its chunks too: the sweep keeps an upload a pinned blob's manifest names
        // (ADR-0051 item 5).
        let _pins = self.pins.pin([Pinned::Blob(blob_ref.blob_id())]);
        read_range(&*self.store, blob_ref, range)
    }

    /// A reader of the whole blob `blob_ref` refers to (SPEC §9.6 `get_blob`), which reads it a window of chunks at
    /// a time ([`WINDOW_BYTES`]), each checked, never the whole blob at once. The manifest is looked up and checked
    /// once, here; every window reads by it ([`read_manifest_range_into`], with one window buffer
    /// and one store scratch buffer for the whole stream). The reader pins the blob and its upload until
    /// it is dropped, so a GC sweep never takes a blob mid-stream (SPEC §9.5).
    ///
    /// # Errors
    /// [`Error::BlobNotFound`] when the id is not bound, [`Error::Corrupt`] when its manifest does not match
    /// `blob_ref`, or the store's errors. Errors while reading come from [`Read::read`] as [`io::Error`]s whose
    /// inner error is the [`Error`].
    pub fn get_blob(&self, blob_ref: &BlobRef) -> Result<BlobReader<'_, S>> {
        // Pinned before the lookup, and its upload after it, as `pin_contained` does (ADR-0051 item 5).
        let mut pins = self.pins.pin([Pinned::Blob(blob_ref.blob_id())]);
        let Some(manifest) = self.store.manifest(blob_ref.blob_id())? else {
            return Err(Error::BlobNotFound {
                blob_id: blob_ref.blob_id(),
            });
        };
        manifest.check_ref(blob_ref)?;
        pins.add(Pinned::Upload(manifest.upload_id()));
        let chunk = u64::from(manifest.chunk_size().get());
        let window = chunk.saturating_mul(u64::from(window_chunk_count(manifest.chunk_size())));
        Ok(BlobReader {
            store: &*self.store,
            blob_ref: blob_ref.clone(),
            manifest,
            window,
            offset: 0,
            buf: Vec::new(),
            pos: 0,
            scratch: Vec::new(),
            _pins: pins,
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

    /// The blob's manifest once its id is checked against `expected_id`; a mismatch discards the upload when it
    /// `staged` chunks (the small path stages nothing before `bind_small`).
    fn checked_manifest(
        &self,
        blob: persia_blob::ChunkedBlob,
        upload_id: UploadId,
        crcs: Vec<u32>,
        options: &PutBlobOptions,
        staged: bool,
    ) -> Result<BlobManifest> {
        if let Some(expected) = options.expected_id
            && expected != blob.blob_id
        {
            if staged {
                self.discard(upload_id);
            }
            return Err(Error::BlobIdMismatch {
                expected,
                actual: blob.blob_id,
            });
        }
        BlobManifest::new(blob.blob_id, blob.size, blob.chunk_size, upload_id, crcs)
    }

    /// `bind` called until its outcome is known (SPEC §9.4, ADR-0051 item 3), with whether an outcome was ever
    /// unknown, after which the upload is never discarded. A plain error ends the loop only while no attempt was
    /// unknown; after one, an error is no answer (the earlier write may still land), so the loop goes on. Attempts
    /// are spaced by [`backoff`]; no attempt starts at or after `options.bind_budget` past the first (a wait that
    /// would reach that deadline is not waited), and the unknown error is returned then. At least one attempt is made.
    /// An unknown outcome from a store that refuses every write until reopened ([`Error::is_store_poisoned`]), or such
    /// a store's error after an unknown outcome, ends the loop at once, still unknown ([`still_unknown`]).
    fn bind(
        &self,
        upload_id: UploadId,
        options: &PutBlobOptions,
        bind: impl Fn() -> Result<BindOutcome>,
    ) -> (Result<BindOutcome>, bool) {
        let start = self.clock.now();
        // The budget left now; `None` once it is used up: no attempt starts at or after the deadline.
        let left = || {
            let elapsed = self.clock.now().saturating_sub(start);
            options
                .bind_budget
                .checked_sub(elapsed)
                .filter(|l| !l.is_zero())
        };
        let give_up = |unknown: Option<Error>| {
            let unknown =
                unknown.unwrap_or_else(|| Error::BindOutcomeUnknown("no attempt answered".into()));
            (Err(unknown), true)
        };
        let mut unknown: Option<Error> = None;
        let mut attempt = 0_u32;
        loop {
            match bind() {
                Ok(outcome) => return (Ok(outcome), unknown.is_some()),
                // A store that refuses every write until it is reopened (the pack after a failed sync, SPEC §4.5)
                // answers nothing more on this handle: the outcome stays unknown, so the retry ends now instead of at
                // the end of the budget (ROADMAP 9.4.7, ADR-0051 item 3). Only a reopen tells what was bound.
                Err(e)
                    if e.is_store_poisoned()
                        && (unknown.is_some() || e.is_bind_outcome_unknown()) =>
                {
                    return (Err(still_unknown(e, unknown.as_ref())), true);
                }
                Err(e) if e.is_bind_outcome_unknown() => unknown = Some(e),
                Err(e) if unknown.is_none() => return (Err(e), false),
                // Still unknown: this answer says nothing of the earlier attempt's write.
                Err(_) => {}
            }
            // A wait that reaches the deadline would only end in giving up: give up now.
            let wait = backoff(upload_id.get(), attempt);
            if left().is_none_or(|left| wait >= left) {
                return give_up(unknown);
            }
            self.clock.sleep(wait);
            // The wait may have taken longer than asked: checked again before the next attempt starts.
            if left().is_none() {
                return give_up(unknown);
            }
            attempt = attempt.saturating_add(1);
        }
    }

    /// The `BlobRef` of a bound blob, from `bind`'s outcome (SPEC §9.2: a duplicate carries the bound chunk size).
    fn bound(
        &self,
        manifest: &BlobManifest,
        (outcome, unknown): (Result<BindOutcome>, bool),
        options: &PutBlobOptions,
        pins: &mut Pins,
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
                // The bound blob's chunks are under its own upload: pinned beside its id (ADR-0051 item 5).
                if bound.upload_id() != manifest.upload_id() {
                    pins.add(Pinned::Upload(bound.upload_id()));
                }
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

/// `poisoned` (an error with [`Error::is_store_poisoned`]) as an unknown outcome: itself when it already is one, or
/// wrapped in [`Error::BindOutcomeUnknown`], so the caller learns both that nothing is known and that the store must
/// be reopened. The earlier unknown outcome, if any, holds the root cause (the failed sync): it is logged, since the
/// returned error must carry the poisoned refusal for [`Error::is_store_poisoned`] to hold.
fn still_unknown(poisoned: Error, first: Option<&Error>) -> Error {
    if let Some(first) = first {
        tracing::warn!(
            error = %first,
            source = ?std::error::Error::source(first),
            "a blob bind's outcome is unknown and its store refuses every write until reopened"
        );
    }
    if poisoned.is_bind_outcome_unknown() {
        poisoned
    } else {
        Error::BindOutcomeUnknown(Box::new(poisoned))
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
    /// Its manifest, looked up and checked against `blob_ref` once, at open: every window reads by it.
    manifest: BlobManifest,
    /// Bytes per window: a whole number of chunks.
    window: u64,
    /// Where the next window starts.
    offset: u64,
    buf: Vec<u8>,
    pos: usize,
    /// The store's read buffer (`BlobStore::read_chunks_checked`), kept for the stream: at most one chunk frame.
    scratch: Vec<u8>,
    /// The blob and its upload, pinned while the stream lives (SPEC §9.5).
    _pins: Pins,
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
            // The window buffer and the store's scratch are kept across windows: neither is allocated (nor, for a
            // pack read with `pread`, zero-filled) again (ROADMAP 9.2.9).
            self.buf.clear();
            self.pos = 0;
            read_manifest_range_into(
                self.store,
                &self.manifest,
                self.offset..end,
                &mut self.scratch,
                &mut self.buf,
            )
            .map_err(|e| {
                self.buf.clear();
                io::Error::other(e)
            })?;
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
