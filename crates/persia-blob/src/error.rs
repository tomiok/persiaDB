//! Errors of the blob store.

use persia_format::BlobId;
use persia_format::blob::BlobRefDefect;

use crate::manifest::UploadId;

/// Result alias for this crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors produced by the blob store.
///
/// Errors about stored bytes name the encoding they were read as (`"blob manifest"`, SPEC §9.1.1), so later
/// encodings of this crate (the pack file, SPEC §9.3) share the variants.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A chunk size or a blob size breaks a rule of SPEC §9.1: the chunk size is outside `1..=64 MiB`, or the blob
    /// needs more than `u32::MAX` chunks of it.
    #[error("invalid chunking: {0}")]
    InvalidChunking(BlobRefDefect),
    /// A writer's chunk size is below [`ChunkSize::MIN`](crate::ChunkSize::MIN) (SPEC §9.2): its blob manifest would
    /// need 4 bytes per chunk, 16 GiB for a 4 GiB blob at 1 byte. A caller's error; stored bytes are read with the
    /// encoding's range ([`ChunkSize::from_encoding`](crate::ChunkSize::from_encoding)).
    #[error("chunk size {chunk_size} below the minimum {min}")]
    ChunkSizeBelowMin {
        /// The chunk size asked for.
        chunk_size: u32,
        /// [`ChunkSize::MIN`](crate::ChunkSize::MIN).
        min: u32,
    },
    /// Reading the blob's bytes from the caller's reader failed.
    #[error("reading the blob failed")]
    Read(#[source] std::io::Error),
    /// A [`Chunker`](crate::Chunker) that already returned an error was called again.
    #[error("the chunker failed earlier")]
    ChunkerFailed,
    /// Stored bytes break a rule of their encoding.
    #[error("corrupt {encoding}: {what}")]
    Corrupt {
        /// The encoding the bytes were read as, e.g. `"blob manifest"`.
        encoding: &'static str,
        /// What was wrong.
        what: &'static str,
    },
    /// Stored bytes have an encoding version above the one this build reads.
    #[error("{encoding} encoding version {version} is newer than this build reads")]
    UnsupportedVersion {
        /// The encoding the bytes were read as.
        encoding: &'static str,
        /// The version read.
        version: u32,
    },
    /// Stored bytes set `flags` bits this build does not know: a newer writer's layout.
    #[error("{encoding} flags {flags:#x} include bits this build does not know")]
    UnsupportedFlags {
        /// The encoding the bytes were read as.
        encoding: &'static str,
        /// The flags read.
        flags: u32,
    },
    /// A blob manifest the rules of SPEC §9.1.1 do not allow was to be built.
    #[error("invalid blob manifest: {what}")]
    InvalidManifest {
        /// The rule it breaks.
        what: &'static str,
    },

    // The `BlobStore` contract (crate::store, SPEC §9.2–§9.6).
    /// No blob is bound under the id a `BlobRef` names (SPEC §9.6: `get_blob` of a dangling reference fails).
    #[error("blob {blob_id} is not stored")]
    BlobNotFound {
        /// The id looked up.
        blob_id: BlobId,
    },
    /// A range to read is inverted or ends past the blob (a caller's error).
    #[error("range {start}..{end} is not within the blob's {size} bytes")]
    RangeOutOfBounds {
        /// The range's start.
        start: u64,
        /// The range's end.
        end: u64,
        /// The blob's size.
        size: u64,
    },
    /// A chunk was staged again under the same upload and index with other bytes: a staged chunk is never
    /// overwritten (SPEC §9.2, §9.4), so the upload must restart under a new upload id.
    #[error("chunk {index} of upload {upload_id} is already staged with other bytes")]
    StagedChunkConflict {
        /// The upload.
        upload_id: UploadId,
        /// The chunk's index.
        index: u32,
    },
    /// Binding a manifest failed in a way that leaves it unknown whether the manifest was bound (SPEC §9.4: an
    /// error from `put_if_absent`). The caller binds the same manifest again until it gets an answer and never
    /// discards the upload's chunks.
    #[error("whether the blob was bound is unknown")]
    BindOutcomeUnknown(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    // -- pack (9.2) --
    // Variants of the local pack file (crate::pack, SPEC §9.3) go here, and only here.
    //
    //

    // -- remote (9.3) --
    // Variants of the remote layout (crate::remote, SPEC §9.4) go here, and only here.
    /// A storage operation of the remote layout failed (SPEC §9.4, §10.1). Corruption a backend reports is
    /// [`Error::Corrupt`] instead.
    #[error("blob storage failed")]
    Storage(#[source] persia_storage::Error),
    /// The backend has no conditional `put_if_absent`, so it cannot stage or bind blobs (SPEC §9.4: `Unsupported`).
    #[error("the storage backend has no conditional put, so it cannot store blobs")]
    ConditionalPutUnsupported,
    /// A resumed upload does not continue what was stored (SPEC §9.4): a [`Chunker`](crate::Chunker) resumed at chunk `k`
    /// needs a hasher fed exactly the `k` full chunks before it. A caller's error.
    #[error("invalid resume: {what}")]
    InvalidResume {
        /// What does not fit.
        what: &'static str,
    },
    //
    //

    // -- documents (9.4) --
    // Variants of put_blob and get_blob_range (SPEC §9.6) go here, and only here.
    /// The id computed from a blob's bytes is not the `expected_id` the caller passed (SPEC §9.6, ADR-0034 item 5):
    /// a caller's error, not corruption. Nothing was bound.
    #[error("the blob's bytes hash to {actual}, not the expected {expected}")]
    BlobIdMismatch {
        /// The id the caller expected.
        expected: BlobId,
        /// The SHA-256 of the bytes read.
        actual: BlobId,
    },
    /// A `put_blob`'s `mime` or `user_meta` breaks a rule of SPEC §9.1 (a caller's error), found before any byte is
    /// read.
    #[error("the blob reference put_blob's mime and user_meta describe is invalid")]
    InvalidBlobRef(#[source] persia_format::EncodeError),
}

impl Error {
    /// Whether stored bytes are corrupt, as opposed to a newer format or a caller's error.
    pub const fn is_corruption(&self) -> bool {
        matches!(self, Self::Corrupt { .. })
    }

    /// Whether this is an error after which a [`BlobStore::bind`](crate::BlobStore::bind) may have bound the
    /// manifest (SPEC §9.4): the caller binds again and never discards the upload.
    pub const fn is_bind_outcome_unknown(&self) -> bool {
        matches!(self, Self::BindOutcomeUnknown(_))
    }
}
