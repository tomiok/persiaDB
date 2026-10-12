//! Error type for decoding on-disk structures.
//!
//! Every byte read from disk or the network is hostile (SPEC §17): malformed input must surface as
//! [`Error::Corrupt`] (or [`Error::CorruptBlock`] inside a block-framed section, SPEC §5.2), never as a panic.
//! [`Error::is_corruption`] and [`Error::corruption`] treat both alike. The other classes are caller errors
//! ([`Error::OutOfBounds`], [`Error::RangeOutOfBounds`], [`Error::NeedMoreBytes`], [`Error::InvalidInputLen`]), a resource error
//! ([`Error::OutOfMemory`]: the input may be fine), and [`Error::Unsupported`] (a valid input from a newer format).
//!
//! Offsets are absolute positions in the enclosing object so that a report points at the exact byte
//! (CLAUDE.md "Safety & errors"), provided the reader knows where its input starts
//! ([`crate::Reader::with_base_offset`], [`crate::Reader::sub_reader`]).
//!
//! The enums are deliberately *not* `#[non_exhaustive]`: their users are crates in this workspace, and an
//! exhaustive `match` makes every new corruption kind a compile error where errors are mapped. Context about
//! *which* object was being decoded is added by the caller's error type (e.g. `persia-storage`); the one
//! exception is a block's index, which only this crate knows.

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
    /// Block `index` of a block-framed section is corrupt (SPEC §5.2). Errors in the block table itself are
    /// [`Error::Corrupt`].
    #[error("corrupt block {index} at offset {offset}: {reason}")]
    CorruptBlock {
        /// Index of the block within its section.
        index: u32,
        /// Absolute byte offset where decoding failed.
        offset: u64,
        /// What was wrong.
        reason: Corruption,
    },
    /// The caller asked for `index` (a block, or the end of a stream range) but only `len` exist. A caller
    /// error, not corruption.
    #[error("index {index} out of bounds (length {len})")]
    OutOfBounds {
        /// The requested block index or range end (saturating).
        index: u64,
        /// How many exist.
        len: u64,
    },
    /// The caller asked for bytes `start..end` (absolute offsets) of a buffer that holds only
    /// `available_start..available_end`, or `start > end`. A caller error, not corruption.
    #[error("range {start}..{end} outside the available {available_start}..{available_end}")]
    RangeOutOfBounds {
        /// Start of the requested range.
        start: u64,
        /// End of the requested range.
        end: u64,
        /// Absolute offset of the buffer's first byte.
        available_start: u64,
        /// Absolute offset just past the buffer's last byte.
        available_end: u64,
    },
    /// The caller supplied fewer bytes than needed, e.g. a section tail shorter than its block table: supply at
    /// least `needed` and retry. A caller error, not corruption.
    #[error("need {needed} bytes, {available} supplied")]
    NeedMoreBytes {
        /// Bytes needed.
        needed: u64,
        /// Bytes supplied.
        available: u64,
    },
    /// The caller supplied input whose length is outside `min..=max`, e.g. a block slice that is not exactly the
    /// block, or a section tail longer than the section. A caller error, not corruption.
    #[error("input length {len} outside {min}..={max}")]
    InvalidInputLen {
        /// The length supplied.
        len: u64,
        /// Smallest valid length.
        min: u64,
        /// Largest valid length.
        max: u64,
    },
    /// Memory needed to decode could not be allocated (ROADMAP 1.4.4). A resource error, not corruption: the
    /// input may be valid. (A length no allocation could ever hold is corruption instead.)
    #[error("out of memory")]
    OutOfMemory {
        /// Bytes requested, when known (`None`: an allocation inside zstd).
        bytes: Option<u64>,
    },
    /// The input is valid as far as this build can tell but uses a format version or feature it does not support
    /// (SPEC §4.2; ADR-0007): upgrade, do not repair. Neither corruption nor a caller error.
    #[error("unsupported format at offset {offset}: {reason}")]
    Unsupported {
        /// Absolute byte offset of the field that names the unsupported version or feature.
        offset: u64,
        /// What is unsupported.
        reason: Unsupported,
    },
}

impl Error {
    /// Whether the input was corrupt ([`Error::Corrupt`] or [`Error::CorruptBlock`]), as opposed to a caller,
    /// resource or [`Error::Unsupported`] error.
    pub const fn is_corruption(&self) -> bool {
        self.corruption().is_some()
    }

    /// The absolute offset and reason of a corruption error; `None` for the caller, resource and unsupported
    /// errors.
    pub const fn corruption(&self) -> Option<(u64, Corruption)> {
        match *self {
            Self::Corrupt { offset, reason } | Self::CorruptBlock { offset, reason, .. } => {
                Some((offset, reason))
            }
            Self::OutOfBounds { .. }
            | Self::RangeOutOfBounds { .. }
            | Self::NeedMoreBytes { .. }
            | Self::InvalidInputLen { .. }
            | Self::OutOfMemory { .. }
            | Self::Unsupported { .. } => None,
        }
    }
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
    /// A full postings block's `last_docid` is too small to end 128 docids that follow the previous block
    /// (SPEC §5.2.1).
    #[error("block last docid {last_docid} is below the smallest possible {min}")]
    LastDocidOutOfRange {
        /// `last_docid` from the header.
        last_docid: u32,
        /// The smallest valid value (`prev + 128`, or 127 for a list's first block; may exceed `u32::MAX`).
        min: u64,
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
    /// A stored CRC32C does not match the data it covers (SPEC §4.1); `Error::Corrupt::offset` points at the
    /// stored checksum.
    #[error("checksum mismatch: stored {stored:#010x}, computed {computed:#010x}")]
    ChecksumMismatch {
        /// The checksum read from the input.
        stored: u32,
        /// The checksum of the bytes actually read.
        computed: u32,
    },
    /// A `codec` byte with no assigned codec (SPEC §5.2).
    #[error("unknown codec {codec}")]
    UnknownCodec {
        /// The byte read.
        codec: u8,
    },
    /// A unit declares more uncompressed bytes than the reader allows (decompression-bomb guard, SPEC §5.2).
    #[error("uncompressed length {len} exceeds the limit of {max}")]
    UncompressedTooLarge {
        /// The declared uncompressed length.
        len: u64,
        /// The caller's limit.
        max: u64,
    },
    /// A payload expands, or declares that it expands, to a length other than the one in its header (SPEC §5.2).
    #[error("payload expands to {actual} bytes, header says {expected}")]
    UncompressedLenMismatch {
        /// The uncompressed length from the header.
        expected: u64,
        /// The length the payload has or declares.
        actual: u64,
    },
    /// A compressed payload is malformed: for zstd, not exactly one valid frame with a declared content size
    /// (SPEC §5.2).
    #[error("malformed compressed payload")]
    InvalidCompressedData,
    /// A block table's `block_size` is outside `1..=max` (SPEC §5.2).
    #[error("block size {size} outside 1..={max}")]
    InvalidBlockSize {
        /// The size read.
        size: u32,
        /// The largest valid size.
        max: u32,
    },
    /// A block table's `block_count` disagrees with `ceil(total_len / block_size)` (SPEC §5.2).
    #[error("block count {count}, but the stream length implies {expected}")]
    BlockCountMismatch {
        /// `block_count` from the table.
        count: u32,
        /// The count implied by `total_len` and `block_size`.
        expected: u64,
    },
    /// A block table's `block_end` is outside `min..=max`: blocks must be at least a header long and end
    /// where the table starts (SPEC §5.2).
    #[error("block end {end} outside {min}..={max}")]
    InvalidBlockEnd {
        /// The `block_end` read.
        end: u64,
        /// Smallest valid end.
        min: u64,
        /// Largest valid end.
        max: u64,
    },
    /// A block header's `stored_len` or `uncompressed_len` disagrees with the block table (SPEC §5.2).
    #[error("block header says {header} bytes, the block table implies {table}")]
    BlockLenMismatch {
        /// The length from the block header.
        header: u64,
        /// The length the block table implies.
        table: u64,
    },
    /// The container header does not start with the magic bytes `PERSIA\0\1` (SPEC §4.2): not a Persia container.
    #[error("bad magic: not a Persia container")]
    BadMagic,
    /// A header's `page_size` is not a power of two in `min..=max` (SPEC §4.2).
    #[error("page size {size} is not a power of two in {min}..={max}")]
    InvalidPageSize {
        /// The size read.
        size: u32,
        /// Smallest valid size.
        min: u32,
        /// Largest valid size.
        max: u32,
    },
    /// A frame does not start with the magic bytes `PFR1` (SPEC §4.4).
    #[error("bad frame magic")]
    BadFrameMagic,
    /// Neither superblock slot is valid (SPEC §4.3).
    #[error("no valid superblock slot")]
    NoValidSuperblock,
    /// Both superblock slots are valid with the same `seq` but different contents, so neither can be trusted
    /// (SPEC §4.3; ADR-0007).
    #[error("both superblock slots have seq {seq} but differ")]
    SuperblockConflict {
        /// The shared `seq`.
        seq: u64,
    },
}

/// What a valid input uses that this build does not support (SPEC §4.2, §15; ADR-0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    /// A `format_major` other than the one this build reads.
    #[error("format major version {major} (this build reads {supported})")]
    FormatMajor {
        /// The major version read.
        major: u16,
        /// The major version this build reads.
        supported: u16,
    },
    /// `incompat_flags` bits this build does not know.
    #[error("incompatible feature flags {flags:#x}")]
    IncompatFlags {
        /// The unknown bits only.
        flags: u64,
    },
    /// Frame `flags` bits this build does not know, in a valid frame (SPEC §4.4; ADR-0009).
    #[error("unknown frame flags {flags:#x}")]
    FrameFlags {
        /// The unknown bits only.
        flags: u32,
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
    /// A [`crate::checksummed::ChecksummedLayout`] wrote a region of the wrong size: a bug in that
    /// implementation, not in the caller's values.
    #[error("layout wrote {actual} bytes, its LEN is {expected}")]
    LayoutLen {
        /// The layout's `LEN`.
        expected: usize,
        /// Bytes `encode_fields` actually wrote.
        actual: usize,
    },
    /// A compression level the codec does not accept.
    #[error("compression level {level} outside {min}..={max}")]
    InvalidCompressionLevel {
        /// The requested level.
        level: i32,
        /// Lowest valid level.
        min: i32,
        /// Highest valid level.
        max: i32,
    },
    /// The compressor failed internally (out of memory); the input itself is always compressible.
    #[error("compression failed")]
    CompressionFailed,
    /// A block size outside `1..=max` (SPEC §5.2).
    #[error("block size {size} outside 1..={max}")]
    InvalidBlockSize {
        /// The requested size.
        size: u32,
        /// The largest valid size.
        max: u32,
    },
    /// A block-framed section would need more than `u32::MAX` blocks (SPEC §5.2).
    #[error("too many blocks")]
    TooManyBlocks,
    /// A page size that is not a power of two in `min..=max` (SPEC §4.2).
    #[error("page size {size} is not a power of two in {min}..={max}")]
    InvalidPageSize {
        /// The requested size.
        size: u32,
        /// Smallest valid size.
        min: u32,
        /// Largest valid size.
        max: u32,
    },
    /// Feature flag bits this build does not know; a writer sets only bits it knows (SPEC §4.2).
    #[error("unknown feature flags {flags:#x}")]
    UnknownFlags {
        /// The unknown bits only.
        flags: u64,
    },
    /// A frame whose total length would overflow `u64` (SPEC §4.4).
    #[error("frame payload of {payload_len} bytes is too large")]
    FrameTooLarge {
        /// The requested payload length.
        payload_len: u64,
    },
    /// A streamed frame payload that does not match the length declared in its header (SPEC §4.4).
    #[error("frame payload is {actual} bytes, its header declares {declared}")]
    PayloadLenMismatch {
        /// `payload_len` written in the header.
        declared: u64,
        /// Bytes given: the total so far when it exceeds `declared` (saturating at `u64::MAX`), the final total when
        /// it falls short.
        actual: u64,
    },
}

// `to_u64` is lossless because of this; a wider-pointer target fails to compile instead of truncating.
const _: () = assert!(usize::BITS <= u64::BITS);

/// Lossless `usize` -> `u64` (see the assertion above).
pub(crate) fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_classes() {
        let reason = Corruption::InvalidCompressedData;
        let corrupt = Error::Corrupt { offset: 3, reason };
        let block = Error::CorruptBlock {
            index: 1,
            offset: 3,
            reason,
        };
        for e in [&corrupt, &block] {
            assert!(e.is_corruption());
            assert_eq!(e.corruption(), Some((3, reason)));
        }
        for e in [
            Error::OutOfBounds { index: 5, len: 4 },
            Error::RangeOutOfBounds {
                start: 1,
                end: 9,
                available_start: 2,
                available_end: 8,
            },
            Error::OutOfMemory { bytes: Some(8) },
            Error::OutOfMemory { bytes: None },
            Error::Unsupported {
                offset: 8,
                reason: Unsupported::FormatMajor {
                    major: 2,
                    supported: 1,
                },
            },
            Error::NeedMoreBytes {
                needed: 2,
                available: 1,
            },
            Error::InvalidInputLen {
                len: 3,
                min: 0,
                max: 2,
            },
        ] {
            assert!(!e.is_corruption());
            assert_eq!(e.corruption(), None);
        }
    }
}
