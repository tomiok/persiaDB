//! Error type for decoding on-disk structures.
//!
//! Every byte read from disk or the network is hostile (SPEC §17): malformed input must surface as
//! [`Error::Corrupt`], never as a panic. Offsets are absolute positions in the enclosing object so that a
//! report points at the exact byte (CLAUDE.md "Safety & errors"), provided the reader knows where its input
//! starts ([`crate::Reader::with_base_offset`], [`crate::Reader::sub_reader`]).
//!
//! The enums are deliberately *not* `#[non_exhaustive]`: their users are crates in this workspace, and an
//! exhaustive `match` makes every new corruption kind a compile error where errors are mapped. Context about
//! *which* object was being decoded is added by the caller's error type (e.g. `persia-storage`), not here.

/// Result alias for this crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors produced while decoding Persia's on-disk formats.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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
///
/// Variants carry only fixed-size data, so the type stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Corruption {
    /// The input ended before a value was complete.
    #[error("unexpected end of input: needed {needed} bytes, {available} available")]
    UnexpectedEof {
        /// Bytes the value needed.
        needed: u64,
        /// Bytes that were left.
        available: u64,
    },
    /// Bytes the format requires to be zero (alignment padding, reserved fields; SPEC §4.1, §4.3, §4.4) were not;
    /// `Error::Corrupt::offset` points at the first offending byte.
    #[error("non-zero byte {value:#04x} where zeros are required")]
    NonZeroReserved {
        /// The offending byte.
        value: u8,
    },
}

// `to_u64` is lossless because of this; a wider-pointer target fails to compile instead of truncating.
const _: () = assert!(usize::BITS <= u64::BITS);

/// Lossless `usize` -> `u64` (see the assertion above).
pub(crate) fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
