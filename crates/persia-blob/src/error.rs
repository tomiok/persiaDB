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
}
