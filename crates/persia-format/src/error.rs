//! Error type for decoding on-disk structures.
//!
//! Every byte read from disk or the network is hostile (SPEC §17): malformed input must surface as
//! [`Error::Corrupt`], never as a panic. Offsets are absolute positions in the enclosing object so that a
//! report points at the exact byte (CLAUDE.md "Safety & errors").

/// Result alias for this crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors produced while decoding Persia's on-disk formats.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The input does not match the format at `offset`.
    #[error("corrupt data at offset {offset}: {reason}")]
    Corrupt {
        /// Absolute byte offset where decoding failed.
        offset: u64,
        /// What was wrong.
        reason: Corruption,
    },
}

/// The specific way input was corrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Corruption {
    /// The input ended before a value was complete.
    #[error("unexpected end of input: needed {needed} bytes, {available} available")]
    UnexpectedEof {
        /// Bytes the value needed.
        needed: u64,
        /// Bytes that were left.
        available: u64,
    },
}

/// Lossless on every supported target (pointers are at most 64 bits); saturates instead of panicking otherwise.
pub(crate) fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
