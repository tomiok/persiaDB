//! Errors of the blob store.

use persia_format::blob::BlobRefDefect;

/// Result alias for this crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors produced by the blob store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A chunk size or a blob size breaks a rule of SPEC §9.1: the chunk size is outside `1..=64 MiB`, or the blob
    /// needs more than `u32::MAX` chunks of it.
    #[error("invalid chunking: {0}")]
    InvalidChunking(#[from] BlobRefDefect),
    /// Reading the blob's bytes from the caller's reader failed.
    #[error("reading the blob failed: {0}")]
    Read(#[source] std::io::Error),
    /// A [`Chunker`](crate::Chunker) that already returned an error was called again.
    #[error("the chunker failed earlier")]
    ChunkerFailed,
    /// Stored bytes break a rule of their format: a blob manifest (SPEC §9.1.1).
    #[error("corrupt blob manifest: {what}")]
    Corrupt {
        /// What was wrong.
        what: &'static str,
    },
    /// A blob manifest encoding version above the one this build reads.
    #[error(
        "blob manifest encoding version {version} (this build reads {})",
        crate::manifest::VERSION
    )]
    UnsupportedVersion {
        /// The version read.
        version: u32,
    },
    /// A blob manifest sets `flags` bits this build does not know: a newer writer's layout.
    #[error(
        "blob manifest flags {flags:#x} (this build knows {:#x})",
        crate::manifest::KNOWN_FLAGS
    )]
    UnsupportedFlags {
        /// The flags read.
        flags: u32,
    },
    /// A blob manifest the rules of SPEC §9.1.1 do not allow was to be built.
    #[error("invalid blob manifest: {what}")]
    InvalidManifest {
        /// The rule it breaks.
        what: &'static str,
    },
}

impl Error {
    /// Whether stored bytes are corrupt, as opposed to a newer format or a caller's error.
    pub const fn is_corruption(&self) -> bool {
        matches!(self, Self::Corrupt { .. })
    }
}
