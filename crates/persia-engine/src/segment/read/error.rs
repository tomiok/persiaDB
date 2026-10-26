//! Errors of the segment reader (SPEC §5.1, §5.3, ROADMAP 5.3).

use crate::document::DecodeError;
use crate::schema::{FieldId, FieldType};

use super::super::SectionKind;

/// Why a segment, or a part of it, could not be read.
///
/// [`ReadError::is_corruption`] tells corrupt bytes from caller errors ([`ReadError::DocOutOfRange`],
/// [`ReadError::FieldType`], [`ReadError::UnknownField`]) and from valid input of a newer format
/// ([`ReadError::Unsupported`]).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ReadError {
    /// The locator, footer or section table breaks a rule of SPEC §5.1; `offset` is absolute (the segment's
    /// [`persia_format::ByteSource`] base included).
    #[error("corrupt segment at offset {offset}: {reason}")]
    Corrupt {
        /// Where the offending bytes are.
        offset: u64,
        /// What was wrong.
        reason: SegmentCorruption,
    },
    /// A section's content breaks a rule of SPEC §5.3. `position` is a stream position in a block-framed section,
    /// an offset into the payload of a raw one (SPEC §5.2).
    #[error("corrupt {kind:?} section (field {field}) at position {position}: {reason}")]
    Section {
        /// The section's kind.
        kind: SectionKind,
        /// Its field id ([`super::super::SEGMENT_FIELD`] for a segment-level section).
        field: u32,
        /// Where in the section.
        position: u64,
        /// What was wrong.
        reason: SegmentCorruption,
    },
    /// A block-framed section's block table or one of its blocks could not be decoded (SPEC §5.2): corruption with
    /// an absolute offset and the block index, or [`persia_format::Error::OutOfMemory`].
    #[error("{kind:?} section (field {field}): {source}")]
    Block {
        /// The section's kind.
        kind: SectionKind,
        /// Its field id.
        field: u32,
        /// What failed.
        source: persia_format::Error,
    },
    /// A primitive outside any section failed for a reason other than corruption (a bug if it ever happens: the
    /// footer is decoded from bytes already in memory).
    #[error(transparent)]
    Format(persia_format::Error),
    /// The segment is valid as far as this build can tell, but uses a format version or feature it does not
    /// read (SPEC §5.1, §15): upgrade, do not repair.
    #[error("unsupported segment at offset {offset}: {reason}")]
    Unsupported {
        /// Absolute offset of the field naming the version or feature.
        offset: u64,
        /// What is unsupported.
        reason: UnsupportedSegment,
    },
    /// A `DocStore` row does not decode (SPEC §6.4.2).
    #[error("document {docid}: {source}")]
    Document {
        /// The document's docid.
        docid: u32,
        /// Why its row does not decode.
        source: DecodeError,
    },
    /// A tombstone set names a docid the segment does not have (SPEC §8.3): the tombstone sidecar is corrupt, not
    /// the segment, so [`ReadError::is_corruption`] is false (it is about the segment's bytes).
    #[error("tombstone for docid {docid}, but the segment has {doc_count} documents")]
    Tombstone {
        /// The docid.
        docid: u32,
        /// The segment's document count.
        doc_count: u32,
    },
    /// The caller asked for a docid at or above the segment's document count.
    #[error("docid {docid} out of range (the segment has {doc_count} documents)")]
    DocOutOfRange {
        /// The docid asked for.
        docid: u32,
        /// The segment's document count.
        doc_count: u32,
    },
    /// The caller asked a field for something its type does not have (e.g. the postings of an `i64` field).
    #[error("field {field} is a {field_type} field, which has no {what}")]
    FieldType {
        /// The field.
        field: FieldId,
        /// Its type.
        field_type: FieldType,
        /// What was asked for.
        what: &'static str,
    },
    /// The caller asked for a term ordinal at or above the field's term count.
    #[error("term ordinal {ordinal} out of range (the field has {len} terms)")]
    OrdinalOutOfRange {
        /// The ordinal asked for.
        ordinal: u64,
        /// The field's term count.
        len: u64,
    },
    /// The caller passed a [`super::TermInfo`] that another field or segment returned.
    #[error("term info of another field or segment")]
    ForeignTermInfo,
    /// The caller named a field the segment's schema does not have.
    #[error("field {field} is not in the segment's schema")]
    UnknownField {
        /// The field.
        field: FieldId,
    },
}

impl ReadError {
    /// Whether the segment's bytes are corrupt, as opposed to a caller, resource or unsupported-format error.
    pub fn is_corruption(&self) -> bool {
        match self {
            Self::Corrupt { .. } | Self::Section { .. } => true,
            Self::Block { source, .. } => source.is_corruption(),
            Self::Format(e) => e.is_corruption(),
            Self::Document { source, .. } => source.is_corruption(),
            Self::Unsupported { .. }
            | Self::DocOutOfRange { .. }
            | Self::FieldType { .. }
            | Self::UnknownField { .. }
            | Self::OrdinalOutOfRange { .. }
            | Self::ForeignTermInfo
            | Self::Tombstone { .. } => false,
        }
    }
}

/// The rule a corrupt segment breaks (SPEC §5.1, §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SegmentCorruption {
    /// A primitive did not decode (short input, a bad varint, width, count, delta, ...; SPEC §5.2.1).
    #[error(transparent)]
    Format(persia_format::Corruption),
    /// The segment does not end with the magic `PSEG`.
    #[error("bad magic: not a segment")]
    BadMagic,
    /// `footer_len` outside `98..=1 MiB`, past the segment start, or a footer start that is not 8-aligned.
    #[error("footer length {len} does not fit the segment")]
    FooterLen {
        /// The `footer_len` read.
        len: u32,
    },
    /// The footer's CRC32C does not match.
    #[error("footer checksum mismatch: stored {stored:#010x}, computed {computed:#010x}")]
    FooterChecksum {
        /// The stored CRC.
        stored: u32,
        /// The CRC of the footer bytes.
        computed: u32,
    },
    /// `doc_count` outside `1..=2^31` (SPEC §5.4).
    #[error("doc count {count} outside 1..=2^31")]
    DocCount {
        /// The count read.
        count: u32,
    },
    /// The segment covers more fields than the current schema has (SPEC §6.2.5).
    #[error("segment covers {recorded} fields, the schema has {current}")]
    SchemaFieldCount {
        /// `schema_field_count`.
        recorded: u32,
        /// Fields of the schema passed to open.
        current: usize,
    },
    /// The schema hash differs from the schema's over the recorded field count (SPEC §6.2.5).
    #[error("schema hash {recorded:#034x}, the schema's is {expected:#034x}")]
    SchemaHash {
        /// The hash read.
        recorded: u128,
        /// The schema's hash over `schema_field_count` fields.
        expected: u128,
    },
    /// Field statistics that do not list exactly the `text` and `keyword` fields, in order, or with
    /// `docs_with_field > doc_count`, or that disagree with the field's sections.
    #[error("field statistics of field {field}")]
    FieldStats {
        /// The field id read (or expected).
        field: u32,
    },
    /// A `section_kind` with no assigned meaning (SPEC §5.3).
    #[error("unknown section kind {kind}")]
    UnknownSectionKind {
        /// The code read.
        kind: u16,
    },
    /// A section under a field of the wrong type, at or above `schema_field_count`, or a segment-level kind under
    /// a field (or the reverse, SPEC §5.3).
    #[error("section kind {kind} under field {field}")]
    SectionField {
        /// The kind's code.
        kind: u16,
        /// The field id.
        field: u32,
    },
    /// The section table is not in strictly ascending `(field_id, section_kind)` order.
    #[error("section table out of order")]
    SectionOrder,
    /// A section that is misaligned, overlaps the previous one, leaves a gap of 8 bytes or more, or runs into the
    /// footer (SPEC §5.1).
    #[error("section at offset {offset}, length {len} does not fit the layout")]
    SectionBounds {
        /// Its offset, relative to the segment start.
        offset: u64,
        /// Its length.
        len: u64,
    },
    /// A section SPEC §5.3 "Presence" requires is missing, or one it rules out is there.
    #[error("section kind {kind} of field {field} is {}", if *.present { "not allowed" } else { "missing" })]
    Presence {
        /// The kind's code.
        kind: u16,
        /// The field id.
        field: u32,
        /// Whether the section is there (and should not be).
        present: bool,
    },
    /// `min_id` or `max_id` outside `1..=512` bytes, or `min_id > max_id`.
    #[error("invalid min_id/max_id")]
    IdRange,
    /// Bytes where the structure must end (after `max_id`, after a column's last part, a list's data, ...).
    #[error("trailing bytes")]
    TrailingBytes,
    /// A raw section's CRC32C does not match its payload (SPEC §5.3).
    #[error("raw section checksum mismatch: stored {stored:#010x}, computed {computed:#010x}")]
    RawChecksum {
        /// The stored CRC.
        stored: u32,
        /// The CRC of the payload.
        computed: u32,
    },
    /// An FST that does not open, or whose key count disagrees with the records it indexes.
    #[error("invalid FST")]
    Fst,
    /// A value the layout of SPEC §5.3 rules out; `what` names the rule.
    #[error("{what}")]
    Invalid {
        /// The rule.
        what: &'static str,
    },
}

impl From<persia_format::Corruption> for SegmentCorruption {
    fn from(reason: persia_format::Corruption) -> Self {
        Self::Format(reason)
    }
}

/// What a valid segment uses that this build does not read (SPEC §5.1, §15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UnsupportedSegment {
    /// A `format_major` other than [`super::super::FORMAT_MAJOR`].
    #[error("format major version {major}")]
    FormatMajor {
        /// The version read.
        major: u16,
    },
    /// `incompat_flags` with bits this build does not know.
    #[error("incompatible feature flags {flags:#x}")]
    IncompatFlags {
        /// The flags read.
        flags: u64,
    },
    /// A schema `encoding_version` this build has no encoder for (SPEC §6.2.5).
    #[error("schema encoding version {version}")]
    SchemaEncoding {
        /// The version read.
        version: u8,
    },
}
