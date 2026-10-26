//! The manifest: the versioned snapshot a commit publishes, listing each collection's schema, its live segments and
//! their tombstone sidecars (SPEC §8.7, ADR-0024).
//!
//! A [`Manifest`] is valid by construction: it is only built by [`Manifest::empty`], [`Manifest::decode`] and
//! [`Manifest::apply`], which all check SPEC §8.7.1. A commit plans its changes as a [`ManifestDiff`] and applies
//! it to the latest manifest; [`Manifest::diff`] recovers the diff between two manifests.
//!
//! ```
//! use persia_engine::manifest::{
//!     CollectionDiff, Location, Manifest, ManifestDiff, SegmentEntry, SegmentId,
//! };
//! use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
//!
//! let schema = SchemaBuilder::new("docs").field("title", FieldKind::Text(TextOptions::default())).build()?;
//! let segment = SegmentEntry {
//!     id: SegmentId::new(7),
//!     doc_count: 100,
//!     size: 4096,
//!     location: Location::Object,
//!     tombstones: None,
//! };
//! let mut diff = ManifestDiff::default();
//! diff.collections.insert("docs".into(), CollectionDiff::Create { schema, segments: vec![segment] });
//!
//! let first = Manifest::empty().apply(&diff)?;
//! assert_eq!(first.seq(), 1);
//! let bytes = first.encode()?;
//! assert_eq!(Manifest::decode(&bytes)?, first);
//! assert_eq!(Manifest::empty().diff(&first)?, diff);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod codec;
mod diff;

use std::collections::BTreeMap;
use std::fmt;

use persia_format::frame::MIN_FRAME_LEN;
use persia_format::hash::xxh3_128;
use persia_format::superblock::FIRST_FRAME_OFFSET;

use crate::schema::CollectionSchema;

pub use codec::{ManifestCorruption, ManifestEncodeError, ManifestError};
pub use diff::{ApplyError, CollectionChange, CollectionDiff, ManifestDiff, UpdateProblem};

/// Longest encoded manifest, in bytes (SPEC §8.7).
pub const MAX_MANIFEST_LEN: usize = 64 << 20;
/// Most documents in one segment (SPEC §5.4).
pub const MAX_SEGMENT_DOCS: u32 = 1 << 31;

/// The id of a segment, unique within a database (SPEC §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegmentId(u128);

impl SegmentId {
    /// Wraps an id.
    pub const fn new(id: u128) -> Self {
        Self(id)
    }

    /// The id as a number.
    pub const fn get(self) -> u128 {
        self.0
    }
}

/// 32 lowercase hex digits.
impl fmt::Display for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// Where the bytes of a segment or tombstone sidecar are (SPEC §8.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// An object whose key follows from the ids (remote layout, SPEC §10.4).
    Object,
    /// A frame of the local container (SPEC §4.4): the offset of its first byte and its total length.
    Frame {
        /// Offset of the frame in the container; a multiple of 8, at least 12288.
        offset: u64,
        /// Total length of the frame; a multiple of 8, at least 32.
        len: u64,
    },
}

/// The current tombstone sidecar of a segment (SPEC §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tombstones {
    /// The sidecar's version: 1 for the first, one more for each rewrite.
    pub version: u64,
    /// How many of the segment's documents are deleted: `1..=doc_count`.
    pub deleted: u32,
    /// Where the sidecar is.
    pub location: Location,
}

/// A live segment of a collection (SPEC §8.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentEntry {
    /// The segment's id.
    pub id: SegmentId,
    /// Documents in the segment: `1..=MAX_SEGMENT_DOCS`.
    pub doc_count: u32,
    /// Length of the segment's bytes (SPEC §5.1); at least 1.
    pub size: u64,
    /// Where the segment is.
    pub location: Location,
    /// Its deletions; `None` when it has none.
    pub tombstones: Option<Tombstones>,
}

/// A rule of SPEC §8.7.1 that one [`SegmentEntry`] breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EntryProblem {
    /// `doc_count` is 0 or above [`MAX_SEGMENT_DOCS`].
    #[error("doc_count {doc_count} outside 1..={MAX_SEGMENT_DOCS}")]
    DocCount {
        /// The count.
        doc_count: u32,
    },
    /// `size` is 0.
    #[error("segment size 0")]
    EmptySegment,
    /// A frame location that cannot hold a frame (SPEC §4.4).
    #[error("frame location {offset}+{len} is not an aligned frame after the header")]
    FrameLocation {
        /// The offset.
        offset: u64,
        /// The length.
        len: u64,
    },
    /// A tombstone sidecar with version 0.
    #[error("tombstone sidecar version 0")]
    TombstoneVersion,
    /// The deleted count is 0 or above the segment's document count.
    #[error("{deleted} deleted documents in a segment of {doc_count}")]
    DeletedCount {
        /// The deleted count.
        deleted: u32,
        /// The segment's document count.
        doc_count: u32,
    },
}

impl Location {
    fn check(self) -> Result<(), EntryProblem> {
        match self {
            Self::Object => Ok(()),
            Self::Frame { offset, len } => {
                let aligned = offset % 8 == 0 && len % 8 == 0;
                let fits = offset.checked_add(len).is_some();
                if aligned && fits && offset >= FIRST_FRAME_OFFSET && len >= MIN_FRAME_LEN as u64 {
                    Ok(())
                } else {
                    Err(EntryProblem::FrameLocation { offset, len })
                }
            }
        }
    }
}

impl SegmentEntry {
    /// Checks the rules of SPEC §8.7.1 that concern this entry alone.
    ///
    /// # Errors
    /// The first rule broken.
    pub fn check(&self) -> Result<(), EntryProblem> {
        if self.doc_count == 0 || self.doc_count > MAX_SEGMENT_DOCS {
            return Err(EntryProblem::DocCount {
                doc_count: self.doc_count,
            });
        }
        if self.size == 0 {
            return Err(EntryProblem::EmptySegment);
        }
        self.location.check()?;
        if let Some(t) = self.tombstones {
            if t.version == 0 {
                return Err(EntryProblem::TombstoneVersion);
            }
            if t.deleted == 0 || t.deleted > self.doc_count {
                return Err(EntryProblem::DeletedCount {
                    deleted: t.deleted,
                    doc_count: self.doc_count,
                });
            }
            t.location.check()?;
        }
        Ok(())
    }
}

/// A collection in a manifest: its current schema and live segments, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectionEntry {
    schema: CollectionSchema,
    segments: Vec<SegmentEntry>,
}

impl CollectionEntry {
    /// The collection's name.
    pub fn name(&self) -> &str {
        self.schema.name()
    }

    /// The current schema.
    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// Live segments, oldest first (in the order commits added them).
    pub fn segments(&self) -> &[SegmentEntry] {
        &self.segments
    }

    /// The live segment with id `id`.
    pub fn segment(&self, id: SegmentId) -> Option<&SegmentEntry> {
        self.segments.iter().find(|s| s.id == id)
    }
}

/// A versioned snapshot of a database (SPEC §8.7).
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    seq: u64,
    fence_token: u64,
    wal_start: u64,
    /// Keyed by collection name.
    collections: BTreeMap<String, CollectionEntry>,
}

impl Manifest {
    /// The state before the first commit: `seq` 0, no collections, zero `fence_token` and `wal_start`.
    pub fn empty() -> Self {
        Self {
            seq: 0,
            fence_token: 0,
            wal_start: 0,
            collections: BTreeMap::new(),
        }
    }

    /// The commit that published this manifest.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The fencing token of the writer that committed it (SPEC §11.3).
    pub fn fence_token(&self) -> u64 {
        self.fence_token
    }

    /// The first WAL position still needed by recovery (SPEC §8.2, §8.7).
    pub fn wal_start(&self) -> u64 {
        self.wal_start
    }

    /// Collections in ascending byte order of their names.
    pub fn collections(&self) -> impl ExactSizeIterator<Item = &CollectionEntry> {
        self.collections.values()
    }

    /// The collection named `name`.
    pub fn collection(&self, name: &str) -> Option<&CollectionEntry> {
        self.collections.get(name)
    }

    /// The encoding of SPEC §8.7.
    ///
    /// # Errors
    /// [`ManifestEncodeError::TooLarge`] if it would exceed [`MAX_MANIFEST_LEN`]. A decoded manifest can: its schemas
    /// are written in the engine's JSON form, which may be longer than the stored one (`1` becomes `1.0`).
    pub fn encode(&self) -> Result<Vec<u8>, ManifestEncodeError> {
        codec::encode(self)
    }

    /// Decodes and checks an encoded manifest (SPEC §8.7).
    ///
    /// # Errors
    /// [`ManifestError::Corrupt`] if `bytes` break any rule of SPEC §8.7, [`ManifestError::UnsupportedVersion`]
    /// for an encoding version this build does not read, [`ManifestError::UnsupportedAnalyzer`] for a schema naming an
    /// analyzer version it does not have.
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestError> {
        codec::decode(bytes)
    }
}

/// XXH3-128 of an encoded manifest (SPEC §8.7): identifies a commit's bytes as stored. Decoding and re-encoding may
/// change the schema text, so whoever compares hashes (a replica, SPEC §10.5) copies the stored bytes, never a
/// re-encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManifestHash(u128);

impl ManifestHash {
    /// The hash of `encoded`, the bytes as stored.
    pub fn of(encoded: &[u8]) -> Self {
        Self(xxh3_128(encoded))
    }

    /// The hash as a number.
    pub const fn get(self) -> u128 {
        self.0
    }
}

impl fmt::Display for ManifestHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}
