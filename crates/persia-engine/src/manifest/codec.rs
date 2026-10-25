//! The manifest encoding (SPEC §8.7, ADR-0024): fixed-width little-endian fields, the schemas as stored JSON, and a
//! CRC32C trailer. Everything but the schema text has exactly one encoding.

use std::collections::{BTreeMap, BTreeSet};

use persia_format::checksum::crc32c;
use persia_format::{Reader, Writer};

use super::{
    CollectionEntry, EntryProblem, Location, MAX_MANIFEST_LEN, Manifest, SegmentEntry, SegmentId,
    Tombstones,
};
use crate::schema::{CollectionSchema, SchemaError};

/// `"PMNF"`: the first bytes of every manifest.
pub(super) const MAGIC: [u8; 4] = *b"PMNF";
/// The encoding version this build writes and reads.
pub(super) const VERSION: u32 = 1;

const LOCATION_OBJECT: u8 = 1;
const LOCATION_FRAME: u8 = 2;
const TOMBSTONES_NONE: u8 = 0;
const TOMBSTONES_SOME: u8 = 1;

/// Magic and version.
const HEAD_LEN: usize = 8;
/// The CRC32C trailer.
const TRAILER_LEN: usize = 4;
/// The smallest collection: schema length and segment count.
const MIN_COLLECTION_LEN: usize = 8;
/// The smallest segment entry: id, `doc_count`, size, an object location, no tombstones.
const MIN_SEGMENT_LEN: usize = 30;

/// Why a manifest could not be decoded.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ManifestError {
    /// The bytes are not a valid manifest.
    #[error("corrupt manifest at offset {offset}: {reason}")]
    Corrupt {
        /// Offset from the start of the encoding.
        offset: u64,
        /// What was wrong.
        reason: ManifestCorruption,
    },
    /// An encoding version above the one this build reads.
    #[error("manifest encoding version {version} (this build reads {VERSION})")]
    UnsupportedVersion {
        /// The version read.
        version: u32,
    },
}

impl ManifestError {
    /// Whether the input was corrupt, as opposed to valid input from a newer format.
    pub fn is_corruption(&self) -> bool {
        matches!(self, Self::Corrupt { .. })
    }
}

/// What is wrong with a corrupt manifest (SPEC §8.7).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ManifestCorruption {
    /// The input ended inside a value, or a length runs past it.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// The input is longer than [`MAX_MANIFEST_LEN`].
    #[error("{len} bytes, at most {MAX_MANIFEST_LEN}")]
    TooLarge {
        /// Its length.
        len: usize,
    },
    /// The input does not start with the manifest magic.
    #[error("not a manifest (bad magic)")]
    Magic,
    /// The CRC32C trailer does not match.
    #[error("checksum mismatch: stored {stored:#010x}, computed {computed:#010x}")]
    ChecksumMismatch {
        /// The stored checksum.
        stored: u32,
        /// The checksum of the bytes.
        computed: u32,
    },
    /// Encoding version 0.
    #[error("encoding version 0")]
    ZeroVersion,
    /// Bytes between the last collection and the trailer.
    #[error("trailing bytes after the last collection")]
    TrailingBytes,
    /// A stored schema is not valid UTF-8, not a valid definition, or lacks its ids or analyzer versions.
    #[error("schema: {0}")]
    Schema(SchemaError),
    /// A collection's name is not above the previous collection's.
    #[error("collection {name:?} is out of order or repeated")]
    CollectionOrder {
        /// The name.
        name: String,
    },
    /// A segment id appears twice.
    #[error("segment {id} appears twice")]
    DuplicateSegment {
        /// The id.
        id: SegmentId,
    },
    /// An unknown location tag.
    #[error("unknown location tag {tag}")]
    LocationTag {
        /// The tag.
        tag: u8,
    },
    /// A tombstone presence byte other than 0 or 1.
    #[error("tombstone presence byte {tag}, not 0 or 1")]
    TombstonesTag {
        /// The byte.
        tag: u8,
    },
    /// A segment entry breaks a rule of SPEC §8.7.1.
    #[error("segment entry: {0}")]
    Entry(EntryProblem),
}

/// Why a manifest could not be encoded.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ManifestEncodeError {
    /// The encoding would exceed [`MAX_MANIFEST_LEN`].
    #[error("manifest would be {len} bytes, at most {MAX_MANIFEST_LEN}")]
    TooLarge {
        /// Its length (saturating).
        len: usize,
    },
    /// A schema could not be serialized, which a valid schema does not cause.
    #[error("schema: {0}")]
    Schema(SchemaError),
}

pub(super) fn encode(manifest: &Manifest) -> Result<Vec<u8>, ManifestEncodeError> {
    let too_large = |len| ManifestEncodeError::TooLarge { len };
    let mut w = Writer::new();
    w.write_bytes(&MAGIC);
    w.write_u32(VERSION);
    w.write_u64(manifest.seq);
    w.write_u64(manifest.fence_token);
    w.write_u64(manifest.wal_start);
    // A count that does not fit a `u32` means at least that many entries of input, far above the limit.
    let count = manifest.collections.len();
    let at_least = |w: &Writer, n: usize, each: usize| {
        too_large(w.len().saturating_add(n.saturating_mul(each)))
    };
    w.write_u32(u32::try_from(count).map_err(|_| at_least(&w, count, MIN_COLLECTION_LEN))?);
    for collection in manifest.collections.values() {
        let schema = collection
            .schema
            .to_json()
            .map_err(ManifestEncodeError::Schema)?;
        w.write_u32(u32::try_from(schema.len()).map_err(|_| at_least(&w, schema.len(), 1))?);
        w.write_bytes(schema.as_bytes());
        let count = collection.segments.len();
        w.write_u32(u32::try_from(count).map_err(|_| at_least(&w, count, MIN_SEGMENT_LEN))?);
        if w.len() > MAX_MANIFEST_LEN {
            return Err(too_large(w.len()));
        }
        for segment in &collection.segments {
            write_segment(&mut w, segment);
            if w.len() > MAX_MANIFEST_LEN {
                return Err(too_large(w.len()));
            }
        }
    }
    let crc = crc32c(w.as_slice());
    w.write_u32(crc);
    if w.len() > MAX_MANIFEST_LEN {
        return Err(too_large(w.len()));
    }
    Ok(w.into_inner())
}

fn write_segment(w: &mut Writer, s: &SegmentEntry) {
    w.write_u128(s.id.get());
    w.write_u32(s.doc_count);
    w.write_u64(s.size);
    write_location(w, s.location);
    match s.tombstones {
        None => w.write_u8(TOMBSTONES_NONE),
        Some(t) => {
            w.write_u8(TOMBSTONES_SOME);
            w.write_u64(t.version);
            w.write_u32(t.deleted);
            write_location(w, t.location);
        }
    }
}

fn write_location(w: &mut Writer, location: Location) {
    match location {
        Location::Object => w.write_u8(LOCATION_OBJECT),
        Location::Frame { offset, len } => {
            w.write_u8(LOCATION_FRAME);
            w.write_u64(offset);
            w.write_u64(len);
        }
    }
}

pub(super) fn decode(bytes: &[u8]) -> Result<Manifest, ManifestError> {
    if bytes.len() > MAX_MANIFEST_LEN {
        return Err(corrupt(
            0,
            ManifestCorruption::TooLarge { len: bytes.len() },
        ));
    }
    let mut head = Reader::new(bytes);
    if head.read_array::<4>().or_eof()? != MAGIC {
        return Err(corrupt(0, ManifestCorruption::Magic));
    }
    // The trailer is fixed for every version, so a damaged version field reads as corruption, not as a newer format.
    let body_end = bytes
        .len()
        .checked_sub(TRAILER_LEN)
        .filter(|&end| end >= HEAD_LEN)
        .ok_or(corrupt(
            len_u64(bytes.len()),
            ManifestCorruption::UnexpectedEof,
        ))?;
    let (covered, trailer) = bytes.split_at(body_end);
    let stored = Reader::with_base_offset(trailer, len_u64(body_end))
        .read_u32()
        .or_eof()?;
    let computed = crc32c(covered);
    if stored != computed {
        return Err(corrupt(
            len_u64(body_end),
            ManifestCorruption::ChecksumMismatch { stored, computed },
        ));
    }
    let version = head.read_u32().or_eof()?;
    match version {
        0 => return Err(corrupt(4, ManifestCorruption::ZeroVersion)),
        VERSION => {}
        _ => return Err(ManifestError::UnsupportedVersion { version }),
    }
    let body = covered.get(HEAD_LEN..).unwrap_or_default();
    let mut r = Reader::with_base_offset(body, len_u64(HEAD_LEN));
    let manifest = decode_body(&mut r)?;
    if !r.is_empty() {
        return Err(corrupt(r.offset(), ManifestCorruption::TrailingBytes));
    }
    Ok(manifest)
}

fn decode_body(r: &mut Reader<'_>) -> Result<Manifest, ManifestError> {
    let seq = r.read_u64().or_eof()?;
    let fence_token = r.read_u64().or_eof()?;
    let wal_start = r.read_u64().or_eof()?;
    let count = r.read_u32().or_eof()?;
    let mut collections = BTreeMap::new();
    let mut ids = BTreeSet::new();
    let mut previous: Option<String> = None;
    for _ in 0..count {
        let at = r.offset();
        let schema = read_schema(r)?;
        let name = schema.name().to_owned();
        if previous.as_ref().is_some_and(|p| *p >= name) {
            return Err(corrupt(at, ManifestCorruption::CollectionOrder { name }));
        }
        let segment_count = r.read_u32().or_eof()?;
        // No capacity from the count: it is untrusted, and each entry is at least MIN_SEGMENT_LEN bytes of input.
        let mut segments = Vec::new();
        for _ in 0..segment_count {
            let at = r.offset();
            let segment = read_segment(r)?;
            segment
                .check()
                .map_err(|p| corrupt(at, ManifestCorruption::Entry(p)))?;
            if !ids.insert(segment.id) {
                return Err(corrupt(
                    at,
                    ManifestCorruption::DuplicateSegment { id: segment.id },
                ));
            }
            segments.push(segment);
        }
        previous = Some(name.clone());
        collections.insert(name, CollectionEntry { schema, segments });
    }
    Ok(Manifest {
        seq,
        fence_token,
        wal_start,
        collections,
    })
}

fn read_schema(r: &mut Reader<'_>) -> Result<CollectionSchema, ManifestError> {
    let len = r.read_u32().or_eof()?;
    let at = r.offset();
    let len = usize::try_from(len).map_err(|_| corrupt(at, ManifestCorruption::UnexpectedEof))?;
    let text = r.read_bytes(len).or_eof()?;
    let schema_error = |e| corrupt(at, ManifestCorruption::Schema(e));
    let text = std::str::from_utf8(text)
        .map_err(|_| schema_error(SchemaError::Json("not valid UTF-8".to_owned())))?;
    CollectionSchema::from_stored_json(text).map_err(schema_error)
}

fn read_segment(r: &mut Reader<'_>) -> Result<SegmentEntry, ManifestError> {
    let id = SegmentId::new(r.read_u128().or_eof()?);
    let doc_count = r.read_u32().or_eof()?;
    let size = r.read_u64().or_eof()?;
    let location = read_location(r)?;
    let at = r.offset();
    let tombstones = match r.read_u8().or_eof()? {
        TOMBSTONES_NONE => None,
        TOMBSTONES_SOME => Some(Tombstones {
            version: r.read_u64().or_eof()?,
            deleted: r.read_u32().or_eof()?,
            location: read_location(r)?,
        }),
        tag => return Err(corrupt(at, ManifestCorruption::TombstonesTag { tag })),
    };
    Ok(SegmentEntry {
        id,
        doc_count,
        size,
        location,
        tombstones,
    })
}

fn read_location(r: &mut Reader<'_>) -> Result<Location, ManifestError> {
    let at = r.offset();
    match r.read_u8().or_eof()? {
        LOCATION_OBJECT => Ok(Location::Object),
        LOCATION_FRAME => Ok(Location::Frame {
            offset: r.read_u64().or_eof()?,
            len: r.read_u64().or_eof()?,
        }),
        tag => Err(corrupt(at, ManifestCorruption::LocationTag { tag })),
    }
}

fn corrupt(offset: u64, reason: ManifestCorruption) -> ManifestError {
    ManifestError::Corrupt { offset, reason }
}

/// The reader only fails when the input ends early.
trait OrEof<T> {
    fn or_eof(self) -> Result<T, ManifestError>;
}

impl<T> OrEof<T> for persia_format::Result<T> {
    fn or_eof(self) -> Result<T, ManifestError> {
        self.map_err(|e| {
            let offset = e.corruption().map_or(0, |(offset, _)| offset);
            corrupt(offset, ManifestCorruption::UnexpectedEof)
        })
    }
}

fn len_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}
