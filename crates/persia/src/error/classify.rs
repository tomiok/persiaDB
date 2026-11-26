//! Which [`ErrorCode`] each error of the layers below gets (ROADMAP 10.1.5, ADR-0063).
//!
//! One function per error enum, each an exhaustive `match` with no wildcard arm wherever the enum allows it: a
//! variant added below does not compile here until someone gives it a code. Two kinds of enum do not allow it:
//!
//! - `#[non_exhaustive]` enums of the engine (`QueryError`, `RequestError`, `SearchError`, `RelaxedSearchError`,
//!   `RelaxError`, `ScoreError`, `Interrupt`): a match must have a wildcard. They are all errors of a search
//!   request, so a variant this build does not know is [`ErrorCode::InvalidArgument`] ([`REQUEST_FALLBACK`]);
//!   `Interrupt`'s is [`ErrorCode::Cancelled`].
//! - Errors of `persia-storage` and `persia-format` (the container's error, the storage error, the format's decode
//!   error). `persia` may not depend on those crates (`cargo xtask check-deps`), so it maps them from their `kind()`,
//!   whose enums the engine re-exports (`persia_engine::error_kinds`): exhaustive matches, so a class added or
//!   renamed below does not compile here. Nothing is read from an error's `Debug` or `Display` text.
//!
//! [`FALLBACK`] is [`ErrorCode::Io`]: the code of a failure that fits no other code (an allocation that failed, an
//! internal invariant that did not hold).

use persia_engine::error_kinds::{ContainerErrorKind, FormatErrorKind, StorageErrorKind};

use persia_engine::db::{DatabaseError, RelaxedSearchError};
use persia_engine::document::{DecodeError, DocumentError};
use persia_engine::manifest::{ApplyError, LayoutError, ManifestEncodeError, ManifestError};
use persia_engine::memtable::MemtableError;
use persia_engine::query::QueryError;
use persia_engine::relax::RelaxError;
use persia_engine::schema::{EvolutionError, SchemaError};
use persia_engine::score::ScoreError;
use persia_engine::search::request::{CursorTokenError, QueryRequestError};
use persia_engine::search::{Interrupt, RequestError, SearchError};
use persia_engine::segment::{ReadError, SegmentError};
use persia_engine::tombstone::SidecarError;
use persia_engine::wal::{RecordError, ReplayError, WalError, WalWriteError};

use super::ErrorCode;
use crate::internal::documents::Error as DocumentsError;

/// The code of a failure that fits no other code, and of a variant of an unnameable enum this build does not know
/// (module docs).
pub(crate) const FALLBACK: ErrorCode = ErrorCode::Io;

/// The code of a variant this build does not know of a `#[non_exhaustive]` error of a search request.
pub(crate) const REQUEST_FALLBACK: ErrorCode = ErrorCode::InvalidArgument;

/// An error of a lower layer that has a code.
pub(crate) trait Classified {
    /// Its stable code.
    fn code(&self) -> ErrorCode;
}

/// A `persia_format::Error`'s class (decoding), as its `kind()` gives it.
fn format_kind(kind: FormatErrorKind) -> ErrorCode {
    match kind {
        FormatErrorKind::Corrupt => ErrorCode::Corrupt,
        FormatErrorKind::Unsupported => ErrorCode::Unsupported,
        // An allocation that failed, and the caller errors of the format's readers, which only a bug in the layers
        // between could cause.
        FormatErrorKind::OutOfMemory | FormatErrorKind::InvalidInput => FALLBACK,
    }
}

/// A `persia_storage::container::Error`'s class, as its `kind()` gives it.
fn container_kind(kind: ContainerErrorKind) -> ErrorCode {
    match kind {
        ContainerErrorKind::Corrupt => ErrorCode::Corrupt,
        ContainerErrorKind::Unsupported => ErrorCode::Unsupported,
        ContainerErrorKind::Locked => ErrorCode::Conflict,
        ContainerErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
        ContainerErrorKind::ReadOnly => ErrorCode::ReadOnly,
        ContainerErrorKind::Poisoned => ErrorCode::Poisoned,
        ContainerErrorKind::InvalidInput => ErrorCode::InvalidArgument,
        // An I/O failure (`CompactedNotDurable` also poisons the handle: later calls say so); an allocation, a
        // format reader's caller error or an encoding that failed fit no other code.
        ContainerErrorKind::Io
        | ContainerErrorKind::CompactedNotDurable
        | ContainerErrorKind::OutOfMemory
        | ContainerErrorKind::FormatInput
        | ContainerErrorKind::Encode => FALLBACK,
    }
}

/// A `persia_storage::Error`'s class, as its `kind()` gives it.
fn storage_kind(kind: StorageErrorKind) -> ErrorCode {
    match kind {
        StorageErrorKind::NotFound => ErrorCode::NotFound,
        StorageErrorKind::Conflict => ErrorCode::Conflict,
        StorageErrorKind::Transient => ErrorCode::Transient,
        StorageErrorKind::Corrupt => ErrorCode::Corrupt,
        StorageErrorKind::InvalidInput => ErrorCode::InvalidArgument,
        StorageErrorKind::Unsupported => ErrorCode::Unsupported,
        StorageErrorKind::Io => FALLBACK,
    }
}

/// An id's length outside `1..=512` bytes: empty is malformed, longer is the limit.
fn id_len(len: usize) -> ErrorCode {
    if len == 0 {
        ErrorCode::InvalidArgument
    } else {
        ErrorCode::Limit
    }
}

impl Classified for Interrupt {
    fn code(&self) -> ErrorCode {
        // `#[non_exhaustive]`: every way a search stops early is a cancellation.
        ErrorCode::Cancelled
    }
}

impl Classified for DocumentError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::IdLen { len } => id_len(*len),
            Self::KeywordTooLong { .. }
            | Self::TooDeep { .. }
            | Self::TooManyValues { .. }
            | Self::TooManyFields { .. }
            | Self::TooLarge { .. } => ErrorCode::Limit,
            Self::Json(_)
            | Self::UnknownField { .. }
            | Self::FieldName { .. }
            | Self::ExtraIsSchemaField { .. }
            | Self::UnknownFieldId { .. }
            | Self::WrongType { .. }
            | Self::OutOfRange { .. }
            | Self::InvalidDatetime { .. }
            | Self::NullStored { .. }
            | Self::Blob(_) => ErrorCode::InvalidArgument,
        }
    }
}

impl Classified for SchemaError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::TooManyFields { .. } => ErrorCode::Limit,
            Self::UnsupportedAnalyzer { .. } => ErrorCode::Unsupported,
            Self::Json(_)
            | Self::CollectionName { .. }
            | Self::FieldName { .. }
            | Self::DuplicateField { .. }
            | Self::FieldIds { .. }
            | Self::OutOfRange { .. }
            | Self::UnknownAnalyzer { .. }
            | Self::StaticRankField { .. }
            | Self::Levels { .. }
            | Self::Synonyms { .. } => ErrorCode::InvalidArgument,
        }
    }
}

impl Classified for EvolutionError {
    fn code(&self) -> ErrorCode {
        // Every change SPEC §6.2.4 forbids is the caller's definition being rejected.
        ErrorCode::InvalidArgument
    }
}

impl Classified for DecodeError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Corrupt { .. } => ErrorCode::Corrupt,
            Self::BlobRef(e) => format_kind(e.kind()),
            Self::UnsupportedVersion { .. } => ErrorCode::Unsupported,
        }
    }
}

impl Classified for ReadError {
    fn code(&self) -> ErrorCode {
        match self {
            // A tombstone for a docid the segment lacks: the sidecar is corrupt.
            Self::Corrupt { .. } | Self::Section { .. } | Self::Tombstone { .. } => {
                ErrorCode::Corrupt
            }
            Self::Block { source, .. } => format_kind(source.kind()),
            Self::Format(e) => format_kind(e.kind()),
            Self::Unsupported { .. } => ErrorCode::Unsupported,
            Self::Document { source, .. } => source.code(),
            Self::Interrupted(interrupt) => interrupt.code(),
            // The engine asked a segment for something it does not have: a bug.
            Self::DocOutOfRange { .. }
            | Self::FieldType { .. }
            | Self::OrdinalOutOfRange { .. }
            | Self::ForeignTermInfo
            | Self::UnknownField { .. } => FALLBACK,
        }
    }
}

impl Classified for SidecarError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Corrupt { .. } => ErrorCode::Corrupt,
            Self::UnsupportedVersion { .. } => ErrorCode::Unsupported,
            // The writer planned a sidecar SPEC §8.3 does not allow: a bug.
            Self::Invalid { .. } => FALLBACK,
        }
    }
}

impl Classified for SegmentError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::UnknownAnalyzer { .. } => ErrorCode::InvalidArgument,
            Self::Document(e) => e.code(),
            Self::Full { .. } | Self::TermTooLong { .. } | Self::TooLarge { .. } => {
                ErrorCode::Limit
            }
            // Invariants of the builder (the memtable never adds an id twice, nor flushes an empty segment), and
            // the write options, which this crate does not expose.
            Self::DuplicateId { .. } | Self::Empty | Self::Internal { .. } | Self::Encode(_) => {
                FALLBACK
            }
        }
    }
}

impl Classified for MemtableError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Segment(e) => e.code(),
            Self::Poisoned => ErrorCode::Poisoned,
        }
    }
}

impl Classified for RecordError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::CollectionName { .. } => ErrorCode::InvalidArgument,
            Self::DocumentLen { .. } => ErrorCode::Limit,
            Self::IdLen { len } => id_len(*len),
        }
    }
}

impl Classified for WalError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Corrupt { .. } => ErrorCode::Corrupt,
            Self::UnsupportedVersion { .. } => ErrorCode::Unsupported,
        }
    }
}

impl Classified for ReplayError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Chunk { error, .. } => error.code(),
            Self::UnknownCollection { .. } => ErrorCode::Corrupt,
            Self::Document { error, .. } => error.code(),
            Self::Memtable { error, .. } => error.code(),
            Self::Container(e) => container_kind(e.kind()),
        }
    }
}

impl Classified for ManifestError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Corrupt { .. } => ErrorCode::Corrupt,
            Self::UnsupportedVersion { .. } | Self::UnsupportedAnalyzer { .. } => {
                ErrorCode::Unsupported
            }
        }
    }
}

impl Classified for ManifestEncodeError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::TooLarge { .. } => ErrorCode::Limit,
            // A valid schema always serializes: a bug.
            Self::Schema(_) => FALLBACK,
        }
    }
}

impl Classified for ApplyError {
    fn code(&self) -> ErrorCode {
        // The database plans every commit itself, so a diff that does not apply is a bug.
        FALLBACK
    }
}

impl Classified for LayoutError {
    fn code(&self) -> ErrorCode {
        // A committed manifest with a location its layout does not use.
        ErrorCode::Corrupt
    }
}

impl Classified for DatabaseError {
    fn code(&self) -> ErrorCode {
        // Corruption anywhere, also in a step that failed after the first error (`ThenFailed::later`), is what the
        // caller must learn: a damaged file is never reported as the lesser failure in front of it.
        if self.is_corruption() {
            return ErrorCode::Corrupt;
        }
        match self {
            Self::Container(e) => container_kind(e.kind()),
            Self::Storage(e) => storage_kind(e.kind()),
            Self::Manifest(e) => e.code(),
            Self::ManifestEncode(e) => e.code(),
            Self::Apply(e) => e.code(),
            Self::Layout(e) => e.code(),
            Self::SlotMismatch { .. }
            | Self::SegmentMismatch { .. }
            | Self::SidecarMismatch { .. }
            | Self::LiveTwice { .. }
            | Self::CorruptTail { .. } => ErrorCode::Corrupt,
            Self::OpenSegment { source, .. } | Self::ReadSegment { source, .. } => source.code(),
            Self::Sidecar { source, .. } => source.code(),
            // An analyzer this open lacks: a newer build's, or one the caller did not register (SPEC §6.2.2 cannot
            // tell them apart).
            Self::UnregisteredAnalyzer { .. } => ErrorCode::Unsupported,
            // The file at the path is not the database this reader follows any more.
            Self::RefreshRefused { .. } => ErrorCode::Conflict,
            Self::Segment(e) => e.code(),
            Self::Memtable(e) => e.code(),
            Self::BatchRejected { source, .. } => source.code(),
            Self::Record(e) => e.code(),
            Self::Wal(e) => match e {
                WalWriteError::Log(log) => container_kind(log.kind()),
                WalWriteError::Poisoned => ErrorCode::Poisoned,
                WalWriteError::ChunkFull(_) => ErrorCode::Limit,
            },
            Self::Replay(e) => e.code(),
            Self::NoSuchCollection { .. } => ErrorCode::NotFound,
            Self::CollectionExists { .. } => ErrorCode::AlreadyExists,
            Self::Evolution { source, .. } => source.code(),
            Self::Internal { .. } => FALLBACK,
            Self::MergeThread(_) => ErrorCode::Io,
            // The first failure is the one the operation reports (`DatabaseError::primary`), unless a later one is
            // corruption (above).
            Self::ThenFailed { error, .. } => error.code(),
            Self::Poisoned => ErrorCode::Poisoned,
        }
    }
}

impl Classified for persia_blob::Error {
    fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidChunking(_)
            | Self::ChunkSizeBelowMin { .. }
            | Self::RangeOutOfBounds { .. }
            | Self::InvalidResume { .. }
            | Self::BlobIdMismatch { .. }
            | Self::InvalidBlobRef(_)
            | Self::BindBudgetAboveMax { .. }
            | Self::ResumeUnchecked
            | Self::BlobSizeMismatch { .. } => ErrorCode::InvalidArgument,
            Self::Read(_) | Self::PackIo { .. } => ErrorCode::Io,
            Self::Corrupt { .. } | Self::CorruptAt { .. } => ErrorCode::Corrupt,
            Self::UnsupportedVersion { .. }
            | Self::UnsupportedFlags { .. }
            | Self::PackUnsupported(_)
            | Self::ConditionalPutUnsupported
            | Self::ResumeUnsupported => ErrorCode::Unsupported,
            Self::BlobNotFound { .. } => ErrorCode::NotFound,
            // Another writer's bytes or lock, or a pack that is not this database's any more.
            Self::StagedChunkConflict { .. }
            | Self::PackReplaced { .. }
            | Self::PackOfAnotherDatabase { .. }
            | Self::PackLocked => ErrorCode::Conflict,
            // Binding again is safe (the blob is content-addressed), unless the store stopped writing.
            Self::BindOutcomeUnknown(_) => {
                if self.is_store_poisoned() {
                    ErrorCode::Poisoned
                } else {
                    ErrorCode::Transient
                }
            }
            Self::PackPoisoned => ErrorCode::Poisoned,
            Self::PackReadOnly => ErrorCode::ReadOnly,
            Self::Storage(e) => storage_kind(e.kind()),
            // What stopped the upload decides; the wrapper only adds that it can be resumed.
            Self::UploadInterrupted { source, .. } => source.code(),
            // "Open again": the lease's file raced a writer's sweep.
            Self::LeaseContended { .. } => ErrorCode::Transient,
            Self::Container(e) => container_kind(e.kind()),
            // Misuse of the store by the layer above it: a bug.
            Self::ChunkerFailed | Self::InvalidManifest { .. } | Self::ChunkNotStaged { .. } => {
                FALLBACK
            }
        }
    }
}

impl Classified for DocumentsError {
    fn code(&self) -> ErrorCode {
        if self.is_corruption() {
            return ErrorCode::Corrupt;
        }
        match self {
            Self::Blob(e) => e.code(),
            Self::Database(e) => e.code(),
            Self::Open { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                ErrorCode::NotFound
            }
            Self::Open { .. } => ErrorCode::Io,
            Self::BlobNotStored { .. } => ErrorCode::NotFound,
            Self::PackExists { .. } => ErrorCode::AlreadyExists,
            // The file at the path is not the database this reader follows any more (or its lease is elsewhere).
            Self::RefreshedElsewhere { .. }
            | Self::RefreshedOlder { .. }
            | Self::RefreshDiverged { .. }
            | Self::LeaseElsewhere { .. } => ErrorCode::Conflict,
            // A writer committed between the documents' open and the lease: open again.
            Self::OpenedBeforeLease { .. } => ErrorCode::Transient,
            // Only for a container opened without a path, which this crate never does: a bug.
            Self::NoContainerPath => FALLBACK,
        }
    }
}

impl Classified for QueryError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::TermTooLong { .. }
            | Self::TooManyTerms { .. }
            | Self::TooManyClauses { .. }
            | Self::TooManyExpansions { .. }
            | Self::TooDeep { .. } => ErrorCode::Limit,
            Self::UnknownField { .. }
            | Self::UnknownFieldId { .. }
            | Self::WrongFieldType { .. }
            | Self::NoPositions { .. }
            | Self::EmptyPrefix { .. }
            | Self::InvalidPhrase { .. }
            | Self::EmptyCombined
            | Self::DuplicateField { .. }
            | Self::InvalidBoost { .. }
            | Self::InvalidScore { .. }
            | Self::InvalidDistance { .. }
            | Self::NoExpansions
            | Self::UnknownAnalyzer { .. }
            | Self::InvalidFieldSpec { .. }
            | Self::FilterSyntax(_)
            | Self::FilterValue { .. }
            | Self::InvalidRange { .. } => ErrorCode::InvalidArgument,
            // `#[non_exhaustive]`.
            _ => REQUEST_FALLBACK,
        }
    }
}

impl Classified for ScoreError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Field { .. } | Self::Params { .. } | Self::NoFields => ErrorCode::InvalidArgument,
            Self::Read(e) => e.code(),
            // `#[non_exhaustive]`.
            _ => REQUEST_FALLBACK,
        }
    }
}

impl Classified for SearchError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Score(e) => e.code(),
            // The engine planned an expansion without the terms to expand over: a bug.
            Self::NeedsTerms { .. } => FALLBACK,
            Self::Query(e) => e.code(),
            Self::Read(e) => e.code(),
            Self::Interrupted(interrupt) => interrupt.code(),
            // `#[non_exhaustive]`.
            _ => REQUEST_FALLBACK,
        }
    }
}

impl Classified for RequestError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::TooManySortKeys { .. }
            | Self::Window { .. }
            | Self::TooManyFacets { .. }
            | Self::FacetSize { .. } => ErrorCode::Limit,
            Self::SortField { .. }
            | Self::Cursor { .. }
            | Self::CursorView { .. }
            | Self::CursorProximity { .. }
            | Self::CursorRelax { .. }
            | Self::CursorRequest { .. }
            | Self::CursorStep { .. }
            | Self::FacetField { .. }
            | Self::Projection { .. } => ErrorCode::InvalidArgument,
            // Invariants of a search's own hits: a bug.
            Self::MismatchedSortValue | Self::HitSegment { .. } => FALLBACK,
            Self::Read(e) => e.code(),
            Self::Interrupted(interrupt) => interrupt.code(),
            // `#[non_exhaustive]`.
            _ => REQUEST_FALLBACK,
        }
    }
}

impl Classified for RelaxedSearchError {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Plan(e) => e.code(),
            Self::Request(e) => e.code(),
            // `#[non_exhaustive]`.
            _ => REQUEST_FALLBACK,
        }
    }
}

impl Classified for QueryRequestError {
    fn code(&self) -> ErrorCode {
        if self.interrupt().is_some() {
            return ErrorCode::Cancelled;
        }
        match self {
            // The request JSON is not a request; a part built for another schema is the caller's mix-up.
            Self::Json(_) | Self::Schema { .. } | Self::ProximityRelaxed => {
                ErrorCode::InvalidArgument
            }
            Self::Query(e) => e.code(),
            Self::Relax(e) => e.code(),
            Self::Cursor(e) => e.code(),
            Self::Request(e) => e.code(),
            Self::Plan(e) => e.code(),
            // `#[non_exhaustive]`.
            _ => REQUEST_FALLBACK,
        }
    }
}

impl Classified for CursorTokenError {
    fn code(&self) -> ErrorCode {
        // Every way a token is not one this build reads is the caller's: cut, changed, too long, another version.
        ErrorCode::InvalidArgument
    }
}

impl Classified for RelaxError {
    fn code(&self) -> ErrorCode {
        // `#[non_exhaustive]`, and every variant is a `relax` setting being rejected.
        REQUEST_FALLBACK
    }
}

#[cfg(test)]
mod tests;
