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
    /// A read, write, sync or file-system call on the blob pack failed. After a failed write or sync the pack is
    /// poisoned ([`Error::PackPoisoned`]): what reached the disk is unknown until it is reopened (SPEC §4.5).
    #[error("blob pack {op} failed")]
    PackIo {
        /// What was being done.
        op: &'static str,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// An earlier write or sync of this pack handle failed, so it writes nothing more (SPEC §4.5): reopen it, and its
    /// recovery decides what survived.
    #[error("the blob pack stopped writing after a failed write or sync: reopen it")]
    PackPoisoned,
    /// The pack was opened read-only.
    #[error("the blob pack is open read-only")]
    PackReadOnly,
    /// The pack uses a format version or feature this build does not read (SPEC §4.2 rules, §9.3): upgrade, do not
    /// repair.
    #[error("the blob pack's format is newer than this build reads")]
    PackUnsupported(#[source] persia_format::Error),
    /// [`PackStore::refresh`](crate::PackStore::refresh) found another pack at the path than the one this handle
    /// opened: a pack rewrite (SPEC §9.5) replaced it, so the handle must be reopened (SPEC §18.10).
    #[error("the blob pack was replaced (generation {opened}, now {found}): reopen it")]
    PackReplaced {
        /// The generation this handle opened.
        opened: u64,
        /// The generation the path holds now.
        found: u64,
    },
    /// The pack's header names another database than the one opening it (its `db_uuid` differs from the
    /// container's): a pack copied beside another database's container. Its blobs are not this database's.
    #[error("the blob pack belongs to another database")]
    PackOfAnotherDatabase {
        /// The `db_uuid` the pack's header holds.
        found: [u8; 16],
    },
    /// `bind` names a chunk this pack handle has not staged with the manifest's length and CRC32C (a caller's error:
    /// binding it would publish a blob that cannot be read back). Nothing was written.
    #[error("chunk {index} of upload {upload_id} is not staged as the manifest describes it")]
    ChunkNotStaged {
        /// The upload.
        upload_id: UploadId,
        /// The chunk's index.
        index: u32,
    },
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
    /// A `put_blob`'s `bind_budget` is above the cap (a caller's error), found before any byte is read: GC's grace
    /// period must stay far above every budget a writer uses (SPEC §9.4, §9.5).
    #[error("bind budget {budget:?} above the maximum {max:?}")]
    BindBudgetAboveMax {
        /// The budget asked for.
        budget: std::time::Duration,
        /// The cap.
        max: std::time::Duration,
    },
    //
    //

    // -- followups (9.2, 9.4) --
    // Variants of wave 2's pack and put_blob follow-ups (ROADMAP 9.2.6, 9.2.8, 9.2.9, 9.4.7) go here, and only here.
    /// Stored bytes of the blob pack break a rule of their encoding, found in the slot or frame that starts at `offset`
    /// (ROADMAP 9.2.8): the header page at 0, a superblock slot at 4096 or 8192, or a frame from 12288 on. Corruption
    /// like [`Error::Corrupt`] ([`Error::is_corruption`]); [`Error::corrupt_offset`] reads the offset.
    #[error("corrupt {encoding} in the slot or frame at offset {offset}: {what}")]
    CorruptAt {
        /// The encoding the bytes were read as, e.g. `"blob pack"` or `"blob manifest"` (in a `BlobIndex` frame).
        encoding: &'static str,
        /// What was wrong.
        what: &'static str,
        /// The absolute offset in the pack file of the slot or frame holding the bytes.
        offset: u64,
    },
    //

    // -- gc (9.5) --
    // Variants of the mark, the sweep and the pack rewrite (SPEC §9.5; ROADMAP 9.5.1, 9.5.2) go here, and only here.
    //
    //
}

impl Error {
    /// Whether the store refuses every write until it is reopened (the pack after a failed write or sync,
    /// [`Error::PackPoisoned`], SPEC §4.5), also when it is the source of an unknown outcome
    /// ([`Error::BindOutcomeUnknown`]): retrying on this handle cannot answer anything (ROADMAP 9.4.7).
    pub fn is_store_poisoned(&self) -> bool {
        match self {
            Self::PackPoisoned => true,
            Self::BindOutcomeUnknown(source) => source
                .downcast_ref::<Self>()
                .is_some_and(Self::is_store_poisoned),
            _ => false,
        }
    }

    /// Whether stored bytes are corrupt, as opposed to a newer format or a caller's error.
    pub const fn is_corruption(&self) -> bool {
        matches!(self, Self::Corrupt { .. } | Self::CorruptAt { .. })
    }

    /// The offset of the slot or frame a corruption was found in, when it is known ([`Error::CorruptAt`]).
    pub const fn corrupt_offset(&self) -> Option<u64> {
        match self {
            Self::CorruptAt { offset, .. } => Some(*offset),
            _ => None,
        }
    }

    /// Whether this is an error after which a [`BlobStore::bind`](crate::BlobStore::bind) may have bound the
    /// manifest (SPEC §9.4): the caller binds again and never discards the upload.
    pub const fn is_bind_outcome_unknown(&self) -> bool {
        matches!(self, Self::BindOutcomeUnknown(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_pack_is_a_poisoned_store_also_behind_an_unknown_outcome() {
        assert!(Error::PackPoisoned.is_store_poisoned());
        assert!(Error::BindOutcomeUnknown(Box::new(Error::PackPoisoned)).is_store_poisoned());
        let nested = Error::BindOutcomeUnknown(Box::new(Error::BindOutcomeUnknown(Box::new(
            Error::PackPoisoned,
        ))));
        assert!(nested.is_store_poisoned());
        assert!(!Error::PackReadOnly.is_store_poisoned());
        let io = std::io::Error::other("timeout");
        assert!(!Error::BindOutcomeUnknown(Box::new(io)).is_store_poisoned());
    }
}
