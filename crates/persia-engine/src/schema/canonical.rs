//! Canonical encoding of a schema's index definition and the schema hash (SPEC §6.2.5, ADR-0022).
//!
//! Frozen: a segment written today must still verify against the same schema after any upgrade, so the golden
//! vectors in the tests must never change. A new option gets a new tag and is written only when its value differs
//! from the behavior before it existed; anything else is a new `ENCODING_VERSION`.

use std::fmt;

use persia_format::Writer;
use persia_format::hash::xxh3_128;

use super::{Field, FieldKind};

const ENCODING_VERSION: u8 = 1;

// Field type tags.
const TYPE_TEXT: u8 = 1;
const TYPE_KEYWORD: u8 = 2;
const TYPE_I64: u8 = 3;
const TYPE_F64: u8 = 4;
const TYPE_BOOL: u8 = 5;
const TYPE_DATETIME: u8 = 6;
const TYPE_STORED: u8 = 7;

// Option tags, written in ascending order.
const OPT_ANALYZER: u8 = 1;
const OPT_POSITIONS: u8 = 2;
const OPT_FACET: u8 = 3;
const OPT_STORED: u8 = 4;

/// XXH3-128 of a schema's canonical index definition (SPEC §6.2.5); recorded in every segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaHash(u128);

impl SchemaHash {
    /// The `encoding_version` of the canonical encoding this build hashes (SPEC §6.2.5); segments record it.
    pub const ENCODING_VERSION: u8 = ENCODING_VERSION;

    /// Wraps a hash read from a segment.
    pub const fn from_u128(value: u128) -> Self {
        Self(value)
    }

    /// The hash as a number (segments store it little-endian).
    pub const fn get(self) -> u128 {
        self.0
    }
}

impl fmt::Display for SchemaHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// The hash of `name` with `fields` (a schema, or the first fields of one: SPEC §6.2.5).
pub(super) fn hash(name: &str, fields: &[Field]) -> SchemaHash {
    SchemaHash(xxh3_128(&encode(name, fields)))
}

pub(super) fn encode(name: &str, fields: &[Field]) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_u8(ENCODING_VERSION);
    write_str(&mut w, name);
    write_len(&mut w, fields.len());
    for field in fields {
        write_str(&mut w, field.name());
        let (tag, options) = match field.kind() {
            FieldKind::Text(_) => (TYPE_TEXT, 3),
            FieldKind::Keyword(_) => (TYPE_KEYWORD, 2),
            FieldKind::I64 { .. } => (TYPE_I64, 1),
            FieldKind::F64 { .. } => (TYPE_F64, 1),
            FieldKind::Bool { .. } => (TYPE_BOOL, 1),
            FieldKind::Datetime { .. } => (TYPE_DATETIME, 1),
            FieldKind::Stored => (TYPE_STORED, 0),
        };
        w.write_u8(tag);
        w.write_varint_u32(options);
        match field.kind() {
            FieldKind::Text(o) => {
                w.write_u8(OPT_ANALYZER);
                write_str(&mut w, o.analyzer.name());
                w.write_varint_u32(o.analyzer.version());
                write_flag(&mut w, OPT_POSITIONS, o.positions);
            }
            FieldKind::Keyword(o) => write_flag(&mut w, OPT_FACET, o.facet),
            _ => {}
        }
        if !matches!(field.kind(), FieldKind::Stored) {
            write_flag(&mut w, OPT_STORED, field.kind().stored());
        }
    }
    w.into_inner()
}

fn write_flag(w: &mut Writer, tag: u8, value: bool) {
    w.write_u8(tag);
    w.write_u8(u8::from(value));
}

fn write_str(w: &mut Writer, s: &str) {
    write_len(w, s.len());
    w.write_bytes(s.as_bytes());
}

/// Lengths here are bounded by validation (names ≤ 64 bytes, ≤ 1024 fields), so the saturation never happens.
fn write_len(w: &mut Writer, len: usize) {
    w.write_varint_u32(u32::try_from(len).unwrap_or(u32::MAX));
}
