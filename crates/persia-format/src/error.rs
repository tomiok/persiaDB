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
    /// The input ended before a value was complete. For variable-length values (varints) `needed` is a
    /// lower bound: at least one byte more than was available.
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
    /// A varint used more bytes than its value needs; only the canonical (shortest) encoding is valid.
    #[error("overlong varint (non-canonical encoding)")]
    OverlongVarint,
    /// A varint encodes a value wider than its type, or has more bytes than the type can need.
    #[error("varint does not fit in {bits} bits")]
    VarintOverflow {
        /// Width of the target type (32 or 64).
        bits: u32,
    },
    /// A bit width outside what the codec allows (SPEC §5.2.1).
    #[error("bit width {width} exceeds the maximum of {max}")]
    InvalidBitWidth {
        /// The width read.
        width: u8,
        /// The largest valid width (32 for postings, 64 for FOR blocks).
        max: u8,
    },
    /// A delta-coded or frame-of-reference value decodes past the top of its type (SPEC §5.2.1).
    #[error("decoded value does not fit in {bits} bits")]
    ValueOverflow {
        /// Width of the target type.
        bits: u32,
    },
    /// A postings block's header disagrees with its content (SPEC §5.2.1).
    #[error("block header says last docid {header}, block decodes to {decoded}")]
    LastDocidMismatch {
        /// `last_docid` from the header.
        header: u32,
        /// The last docid actually decoded.
        decoded: u32,
    },
    /// An element count outside its valid range (SPEC §5.2.1).
    #[error("count {count} outside {min}..={max}")]
    InvalidCount {
        /// The count read.
        count: u32,
        /// Smallest valid count.
        min: u32,
        /// Largest valid count.
        max: u32,
    },
}

/// Errors produced while encoding: the caller passed values the format cannot represent.
///
/// Encoders validate everything before writing, so on error nothing has been appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// A sequence that must be strictly increasing is not, at `index`.
    #[error("value at index {index} is not greater than the previous one")]
    NotStrictlyIncreasing {
        /// Index of the first value that is not greater than its predecessor.
        index: usize,
    },
    /// A value needs more bits than the requested width.
    #[error("value at index {index} does not fit in {width} bits")]
    ValueTooWide {
        /// Index of the first value that does not fit.
        index: usize,
        /// The requested width.
        width: u8,
    },
    /// The number of values is outside what the structure can hold.
    #[error("{len} values where {min}..={max} are allowed")]
    InvalidLen {
        /// Values given.
        len: usize,
        /// Smallest valid count.
        min: usize,
        /// Largest valid count.
        max: usize,
    },
}

// `to_u64` is lossless because of this; a wider-pointer target fails to compile instead of truncating.
const _: () = assert!(usize::BITS <= u64::BITS);

/// Lossless `usize` -> `u64` (see the assertion above).
pub(crate) fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
