//! Tombstone sidecars (SPEC §8.3, ADR-0030): the deleted documents of one segment, kept beside its immutable bytes.
//!
//! A sidecar holds the **full** set of a segment's deleted docids at one version, so reading one is enough. Version 1
//! is a segment's first sidecar and each rewrite is one more; a new version is always a superset of the one before it
//! (deletions are never undone), which [`TombstoneSidecar::check_follows`] checks before a commit publishes it
//! (SPEC §8.7.2: the manifest sees only the counts).
//!
//! ```text
//! 0   4  magic        bytes "PTMB" (0x50 0x54 0x4D 0x42)
//! 4   4  version      u32 = 1 (the encoding)
//! 8   16 segment_id   u128
//! 24  8  sidecar      u64  (the sidecar's version, ≥ 1)
//! 32  4  doc_count    u32  (the segment's, 1..=2^31)
//! 36  4  deleted      u32  (1..=doc_count: the bitmap's cardinality)
//! 40  ..  the deleted docids: a roaring bitmap, portable serialization; every docid below doc_count
//! ..  4  crc32c       u32  over every byte before it
//! ```
//!
//! ```
//! use persia_engine::manifest::SegmentId;
//! use persia_engine::tombstone::TombstoneSidecar;
//! use roaring::RoaringBitmap;
//!
//! let first = TombstoneSidecar::new(SegmentId::new(7), 1, 10, RoaringBitmap::from_iter([3]))?;
//! first.check_follows(0, &RoaringBitmap::new())?;
//! let second = TombstoneSidecar::new(SegmentId::new(7), 2, 10, first.deleted() | &RoaringBitmap::from_iter([1]))?;
//! second.check_follows(first.version(), first.deleted())?;
//! assert_eq!((second.version(), second.deleted_count()), (2, 2));
//! assert_eq!(TombstoneSidecar::decode(&second.encode())?, second);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use persia_format::checksum::crc32c;
use persia_format::{Reader, Writer};
use roaring::RoaringBitmap;

use crate::manifest::{MAX_SEGMENT_DOCS, SegmentId};

/// `"PTMB"`: the first bytes of every sidecar.
pub const MAGIC: [u8; 4] = *b"PTMB";
/// The encoding version this build writes and reads.
pub const VERSION: u32 = 1;
/// The largest encoding: what this writer produces for any segment (each container in its smallest kind, at most
/// 8 KiB: `2^28` bytes for `2^31` documents, plus descriptions, offsets, header and trailer). Anything longer is
/// corruption, even a valid bitmap with needlessly large run containers.
pub const MAX_SIDECAR_LEN: usize = (1 << 28) + (1 << 20);

/// Magic, version, segment id, sidecar version, `doc_count`, `deleted`.
const HEAD_LEN: usize = 40;
/// The CRC32C trailer.
const TRAILER_LEN: usize = 4;

/// Why a sidecar could not be decoded or built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SidecarError {
    /// The bytes are not a valid sidecar.
    #[error("corrupt tombstone sidecar: {what}")]
    Corrupt {
        /// What was wrong.
        what: &'static str,
    },
    /// An encoding version above the one this build reads.
    #[error("tombstone sidecar encoding version {version} (this build reads {VERSION})")]
    UnsupportedVersion {
        /// The version read.
        version: u32,
    },
    /// A sidecar the rules of SPEC §8.3 do not allow was to be built; nothing was written.
    #[error("invalid tombstone sidecar: {what}")]
    Invalid {
        /// The rule it breaks.
        what: &'static str,
    },
}

impl SidecarError {
    /// Whether stored bytes are corrupt, as opposed to a newer format or a rejected build.
    pub fn is_corruption(&self) -> bool {
        matches!(self, Self::Corrupt { .. })
    }
}

/// One version of a segment's tombstone sidecar (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct TombstoneSidecar {
    segment_id: SegmentId,
    version: u64,
    doc_count: u32,
    deleted: RoaringBitmap,
}

impl TombstoneSidecar {
    /// The sidecar `version` of segment `segment_id`, which has `doc_count` documents, deleting `deleted` (stored
    /// with run containers where they are smaller).
    ///
    /// # Errors
    /// [`SidecarError::Invalid`] for version 0, a `doc_count` outside `1..=2^31`, no deleted docid, or one at or
    /// above `doc_count`.
    pub fn new(
        segment_id: SegmentId,
        version: u64,
        doc_count: u32,
        mut deleted: RoaringBitmap,
    ) -> Result<Self, SidecarError> {
        let invalid = |what| SidecarError::Invalid { what };
        if version == 0 {
            return Err(invalid("version 0"));
        }
        if doc_count == 0 || doc_count > MAX_SEGMENT_DOCS {
            return Err(invalid("doc_count outside 1..=2^31"));
        }
        match deleted.max() {
            None => Err(invalid("no deleted document")),
            Some(max) if max >= doc_count => Err(invalid("a deleted docid at or above doc_count")),
            Some(_) => {
                // Runs where they are smaller: a bulk delete stays compact (ADR-0030).
                deleted.optimize();
                Ok(Self {
                    segment_id,
                    version,
                    doc_count,
                    deleted,
                })
            }
        }
    }

    /// Checks that this sidecar may follow the segment's current one, version `prev_version` deleting
    /// `prev_deleted` (0 and an empty set for a segment without a sidecar): the next version, and every docid the
    /// current one deletes (SPEC §8.3, §8.7.2). The writer runs it before it commits this sidecar; the manifest sees
    /// only the counts.
    ///
    /// # Errors
    /// [`SidecarError::Invalid`] for the first rule broken.
    pub fn check_follows(
        &self,
        prev_version: u64,
        prev_deleted: &RoaringBitmap,
    ) -> Result<(), SidecarError> {
        let invalid = |what| Err(SidecarError::Invalid { what });
        if prev_version.checked_add(1) != Some(self.version) {
            return invalid(
                "a sidecar's version must be one above the current one (1 for a first sidecar)",
            );
        }
        if !prev_deleted.is_subset(&self.deleted) {
            return invalid("a new sidecar version must keep every deletion of the previous one");
        }
        Ok(())
    }

    /// The segment it belongs to.
    pub fn segment_id(&self) -> SegmentId {
        self.segment_id
    }

    /// Its version: 1 for the first, one more per rewrite.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The segment's document count.
    pub fn doc_count(&self) -> u32 {
        self.doc_count
    }

    /// The deleted docids.
    pub fn deleted(&self) -> &RoaringBitmap {
        &self.deleted
    }

    /// The deleted docids, by value.
    pub fn into_deleted(self) -> RoaringBitmap {
        self.deleted
    }

    /// How many documents are deleted: the manifest's `deleted` count (SPEC §8.7).
    pub fn deleted_count(&self) -> u32 {
        // At most `doc_count` (checked on construction), so it fits.
        u32::try_from(self.deleted.len()).unwrap_or(u32::MAX)
    }

    /// The encoding (see the module docs). At most [`MAX_SIDECAR_LEN`] bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.write_bytes(&MAGIC);
        w.write_u32(VERSION);
        w.write_u128(self.segment_id.get());
        w.write_u64(self.version);
        w.write_u32(self.doc_count);
        w.write_u32(self.deleted_count());
        let mut bitmap = Vec::with_capacity(self.deleted.serialized_size());
        // Writing to a `Vec` cannot fail.
        if self.deleted.serialize_into(&mut bitmap).is_ok() {
            w.write_bytes(&bitmap);
        }
        let crc = crc32c(w.as_slice());
        w.write_u32(crc);
        w.into_inner()
    }

    /// Decodes a sidecar: the length, the magic, the CRC32C trailer, the version, then the rest.
    ///
    /// # Errors
    /// [`SidecarError::Corrupt`] for anything SPEC §8.3 does not allow; [`SidecarError::UnsupportedVersion`] for a
    /// newer encoding.
    pub fn decode(bytes: &[u8]) -> Result<Self, SidecarError> {
        let corrupt = |what| SidecarError::Corrupt { what };
        if bytes.len() > MAX_SIDECAR_LEN {
            return Err(corrupt("longer than the largest sidecar"));
        }
        if bytes.get(..4) != Some(&MAGIC[..]) {
            return Err(corrupt("not a tombstone sidecar (bad magic)"));
        }
        // The trailer is fixed for every version, so a damaged version field reads as corruption.
        let body_end = bytes
            .len()
            .checked_sub(TRAILER_LEN)
            .filter(|&end| end >= HEAD_LEN)
            .ok_or(corrupt("shorter than a header and a trailer"))?;
        let (covered, trailer) = bytes.split_at(body_end);
        let stored = Reader::new(trailer)
            .read_u32()
            .map_err(|_| corrupt("shorter than a header and a trailer"))?;
        if stored != crc32c(covered) {
            return Err(corrupt("checksum mismatch"));
        }
        let mut r = Reader::new(covered.get(4..).unwrap_or_default());
        let eof = |_| corrupt("shorter than a header and a trailer");
        match r.read_u32().map_err(eof)? {
            0 => return Err(corrupt("encoding version 0")),
            VERSION => {}
            version => return Err(SidecarError::UnsupportedVersion { version }),
        }
        let segment_id = SegmentId::new(r.read_u128().map_err(eof)?);
        let version = r.read_u64().map_err(eof)?;
        let doc_count = r.read_u32().map_err(eof)?;
        let deleted_count = r.read_u32().map_err(eof)?;
        if version == 0 {
            return Err(corrupt("sidecar version 0"));
        }
        if doc_count == 0 || doc_count > MAX_SEGMENT_DOCS {
            return Err(corrupt("doc_count outside 1..=2^31"));
        }
        if deleted_count == 0 || deleted_count > doc_count {
            return Err(corrupt("deleted count outside 1..=doc_count"));
        }
        let mut rest = r.rest();
        let deleted = RoaringBitmap::deserialize_from(&mut rest)
            .map_err(|_| corrupt("the deleted docids are not a valid roaring bitmap"))?;
        if !rest.is_empty() {
            return Err(corrupt("trailing bytes after the bitmap"));
        }
        if deleted.len() != u64::from(deleted_count) {
            return Err(corrupt("the deleted count is not the bitmap's cardinality"));
        }
        if deleted.max().is_some_and(|max| max >= doc_count) {
            return Err(corrupt("a deleted docid at or above doc_count"));
        }
        Ok(Self {
            segment_id,
            version,
            doc_count,
            deleted,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)] // Tests: inputs are fixed.
mod tests {
    use super::*;

    fn sidecar(deleted: &[u32]) -> TombstoneSidecar {
        TombstoneSidecar::new(SegmentId::new(9), 1, 100, deleted.iter().copied().collect()).unwrap()
    }

    /// Recomputes the trailer after a change to the covered bytes.
    fn reseal(mut bytes: Vec<u8>) -> Vec<u8> {
        let end = bytes.len() - TRAILER_LEN;
        let crc = crc32c(&bytes[..end]);
        bytes[end..].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    #[test]
    fn round_trips() {
        let s = sidecar(&[0, 5, 99]);
        let bytes = s.encode();
        assert_eq!(&bytes[..4], b"PTMB");
        assert_eq!(TombstoneSidecar::decode(&bytes).unwrap(), s);
    }

    #[test]
    fn rejects_what_spec_does_not_allow() {
        let bytes = sidecar(&[1, 2]).encode();
        let corrupt = |b: &[u8]| TombstoneSidecar::decode(b).unwrap_err().is_corruption();
        // Any flipped bit fails the checksum.
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 1;
            assert!(corrupt(&b), "bit flip at {i}");
        }
        // Fields re-sealed with a valid checksum.
        let field = |at: usize, value: &[u8]| {
            let mut b = bytes.clone();
            b[at..at + value.len()].copy_from_slice(value);
            reseal(b)
        };
        assert!(corrupt(&field(4, &0_u32.to_le_bytes())));
        assert_eq!(
            TombstoneSidecar::decode(&field(4, &2_u32.to_le_bytes())),
            Err(SidecarError::UnsupportedVersion { version: 2 })
        );
        assert!(corrupt(&field(24, &0_u64.to_le_bytes())));
        assert!(corrupt(&field(32, &0_u32.to_le_bytes())));
        assert!(corrupt(&field(32, &(MAX_SEGMENT_DOCS + 1).to_le_bytes())));
        assert!(corrupt(&field(32, &2_u32.to_le_bytes()))); // docid 2 ≥ doc_count 2
        assert!(corrupt(&field(36, &0_u32.to_le_bytes())));
        assert!(corrupt(&field(36, &3_u32.to_le_bytes()))); // not the cardinality
        let mut trailing = bytes[..bytes.len() - TRAILER_LEN].to_vec();
        trailing.extend_from_slice(&[0; 5]);
        assert!(corrupt(&reseal(trailing)));
        assert!(corrupt(&bytes[..HEAD_LEN]));
        assert!(corrupt(b""));
    }

    #[test]
    fn successors_keep_every_deletion() {
        let none = RoaringBitmap::new();
        let first = sidecar(&[4]);
        first.check_follows(0, &none).unwrap();
        let second =
            TombstoneSidecar::new(SegmentId::new(9), 2, 100, [4, 7].into_iter().collect()).unwrap();
        second.check_follows(1, first.deleted()).unwrap();
        // A first sidecar is version 1; a rewrite is the next version.
        assert!(second.check_follows(0, &none).is_err());
        assert!(first.check_follows(1, first.deleted()).is_err());
        assert!(second.check_follows(2, first.deleted()).is_err());
        // A dropped deletion.
        let dropped =
            TombstoneSidecar::new(SegmentId::new(9), 2, 100, [7].into_iter().collect()).unwrap();
        assert!(dropped.check_follows(1, first.deleted()).is_err());
        assert!(
            TombstoneSidecar::new(SegmentId::new(9), 1, 100, [100].into_iter().collect()).is_err()
        );
        assert!(TombstoneSidecar::new(SegmentId::new(9), 1, 100, none.clone()).is_err());
        assert!(
            TombstoneSidecar::new(SegmentId::new(9), 0, 100, [1].into_iter().collect()).is_err()
        );
    }
}
