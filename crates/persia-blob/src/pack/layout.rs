//! The blob pack's bytes (SPEC §9.3, ADR-0049): its header, its superblock slots and the payloads of its
//! `BlobChunk` and `BlobIndex` frames. Pure encoding and decoding, no I/O, so recovery, reads, the pack rewrite and
//! `verify` (ROADMAP 9.5) share one set of rules.
//!
//! ```text
//! 0      4096  header         the container header (SPEC §4.2) with magic "PERSIAB\x01"
//! 4096   4096  slot A         PackSuperblock
//! 8192   4096  slot B         PackSuperblock
//! 12288        frames         PFR1 frames (SPEC §4.4), kind 6 BlobChunk and 7 BlobIndex, never compressed
//! ```

use persia_format::checksum::crc32c;
use persia_format::checksummed::{Checksummed, ChecksummedLayout};
use persia_format::frame::{Frame, FrameEncoder, FrameKind};
use persia_format::header::{BLOB_PACK, HEADER_LEN, Header};
use persia_format::superblock::{FIRST_FRAME_OFFSET, SLOT_LEN, SLOTS_LEN, Slot, pick_active};
use persia_format::widen::{u64_from_usize, usize_from_u32};
use persia_format::{Corruption, Reader, Writer};

use crate::chunking::ChunkSize;
use crate::error::{Error, Result};
use crate::manifest::{BlobManifest, UploadId};

/// The name errors give the pack's encodings.
pub const ENCODING: &str = "blob pack";
/// Where the first frame starts: after the header and both slots, as in a container (SPEC §4.2).
pub const FIRST_FRAME: u64 = FIRST_FRAME_OFFSET;
/// The generation of a new pack (SPEC §9.3): a pack rewrite (ROADMAP 9.5.2) writes the next one.
pub const FIRST_GENERATION: u64 = 1;
/// The bytes before a chunk's bytes in a `BlobChunk` payload: `upload_id`, `chunk_idx`, `len`, `crc`, reserved.
pub const CHUNK_HEAD_LEN: usize = 32;
/// The bytes before the blob manifest in a `BlobIndex` payload: `manifest_len`, reserved.
pub const INDEX_HEAD_LEN: usize = 8;
/// The largest chunk a `BlobChunk` frame holds: the largest chunk size (SPEC §9.1).
pub const MAX_CHUNK_LEN: u32 = ChunkSize::MAX;

/// Bytes 72..4092 of a slot.
const SLOT_RESERVED_LEN: usize = SLOT_LEN - 72 - 4;

pub(crate) const fn corrupt(what: &'static str) -> Error {
    Error::Corrupt {
        encoding: ENCODING,
        what,
    }
}

/// A format error of the pack's header, slots or frames as this crate reports it: unsupported stays unsupported,
/// everything else is corruption of the pack.
pub(crate) fn from_format(e: persia_format::Error) -> Error {
    use persia_format::Error as F;
    let what = match &e {
        F::Unsupported { .. } => return Error::PackUnsupported(e),
        F::OutOfMemory { .. } => {
            return Error::PackIo {
                op: "allocate",
                source: std::io::ErrorKind::OutOfMemory.into(),
            };
        }
        F::Corrupt { reason, .. } | F::CorruptBlock { reason, .. } => match reason {
            Corruption::BadMagic => "not a blob pack (bad magic)",
            Corruption::ChecksumMismatch { .. } => "checksum mismatch",
            Corruption::NoValidSuperblock => "no valid superblock",
            Corruption::SuperblockConflict { .. } => "two different superblocks with the same seq",
            Corruption::BadFrameMagic => "bad frame magic",
            Corruption::NonZeroReserved { .. } => "non-zero reserved or padding bytes",
            Corruption::UnexpectedEof { .. } => "a header, slot or frame is cut short",
            Corruption::InvalidPageSize { .. } => "invalid page size",
            _ => "an invalid header, slot or frame",
        },
        _ => "an invalid header, slot or frame",
    };
    corrupt(what)
}

/// One superblock slot of a pack (SPEC §9.3), the double-buffered commit record of SPEC §4.3 with the pack's fields:
///
/// ```text
/// 0     8  seq             u64, monotonic commit counter: one per bind
/// 8     8  index_off       u64, the last committed BlobIndex frame (0 when no blob is bound)
/// 16    8  index_len       u64, its total length (0 when no blob is bound)
/// 24    8  log_end         u64, end of the last committed frame
/// 32    8  generation      u64, 1 for a new pack, one more for each pack rewrite (SPEC §18.10)
/// 40    8  blob_count      u64, the BlobIndex frames in 12288..log_end
/// 48    8  next_frame_seq  u64, the frame_seq of the frame at log_end
/// 56   16  pack_id         u128, random, drawn for each new or rewritten pack file
/// 72   ..  reserved        zero
/// 4092  4  crc             CRC32C over bytes 0..4092
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackSuperblock {
    /// Monotonic commit counter: one per bind.
    pub seq: u64,
    /// Offset of the last committed `BlobIndex` frame; 0 when no blob is bound.
    pub index_off: u64,
    /// Total length of that frame; 0 when no blob is bound.
    pub index_len: u64,
    /// End of the last committed frame: nothing after it was ever acknowledged.
    pub log_end: u64,
    /// Which pack this is: [`FIRST_GENERATION`] for a new one, one more per rewrite (SPEC §18.10).
    pub generation: u64,
    /// How many `BlobIndex` frames lie in `12288..log_end`.
    pub blob_count: u64,
    /// The `frame_seq` of the frame at `log_end` (SPEC §4.4).
    pub next_frame_seq: u64,
    /// The file's identity: random, drawn when a pack file is created or rewritten, the same in every commit of it.
    /// With `generation`, what a reader compares to learn the path names another file (SPEC §18.10, ADR-0016 item 5).
    pub pack_id: u128,
}

/// The shortest `BlobIndex` frame: an empty blob's (head 8 + manifest 76, padded to 88, in a 32-byte frame).
pub const MIN_INDEX_FRAME_LEN: u64 = 120;

impl PackSuperblock {
    /// Both slots of a new pack file `pack_id`: nothing committed, generation [`FIRST_GENERATION`].
    pub const fn new_pack(pack_id: u128) -> Self {
        Self {
            seq: 0,
            index_off: 0,
            index_len: 0,
            log_end: FIRST_FRAME,
            generation: FIRST_GENERATION,
            blob_count: 0,
            next_frame_seq: 1,
            pack_id,
        }
    }

    /// The slot's [`SLOT_LEN`] bytes, CRC included. Infallible: every field is fixed-width.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.encode_fields(&mut w);
        let mut bytes = w.into_inner();
        debug_assert_eq!(bytes.len(), SLOT_LEN);
        let crc = crc32c(bytes.get(..SLOT_LEN - 4).unwrap_or_default());
        if let Some(field) = bytes.get_mut(SLOT_LEN - 4..) {
            field.copy_from_slice(&crc.to_le_bytes());
        }
        bytes
    }

    /// Checks the fields against a pack of `file_len` bytes, before any of them is used (SPEC §9.3, ADR-0049 item 2).
    /// Only states a writer produces pass:
    /// - `log_end` a multiple of 8 in `12288..=file_len`, `generation ≥ 1`, `next_frame_seq ≥ 1`;
    /// - `seq = 0`: the new-pack values (`log_end = 12288`, `next_frame_seq = 1`, no index, no blob), with any
    ///   generation and `pack_id` (a rewrite with no live blob writes that);
    /// - `seq ≥ 1`: at least one blob, at most one per [`MIN_INDEX_FRAME_LEN`] bytes of frames, and the last index
    ///   frame ends exactly at `log_end` (a bind appends it last), 8-aligned, at least [`MIN_INDEX_FRAME_LEN`] long,
    ///   with `next_frame_seq ≥ 2`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] naming the first rule that does not hold.
    pub fn check_bounds(&self, file_len: u64) -> Result<()> {
        let aligned = |v: u64| v.is_multiple_of(8);
        if !aligned(self.log_end) || !(FIRST_FRAME..=file_len).contains(&self.log_end) {
            return Err(corrupt("log_end is not a frame boundary inside the file"));
        }
        if self.generation < FIRST_GENERATION || self.next_frame_seq == 0 {
            return Err(corrupt("generation or next_frame_seq is 0"));
        }
        if self.seq == 0 {
            let new = Self {
                generation: self.generation,
                ..Self::new_pack(self.pack_id)
            };
            return if *self == new {
                Ok(())
            } else {
                Err(corrupt(
                    "an uncommitted superblock that is not a new pack's",
                ))
            };
        }
        let frames = self.log_end.saturating_sub(FIRST_FRAME);
        if self.blob_count == 0 || self.blob_count > frames / MIN_INDEX_FRAME_LEN {
            return Err(corrupt(
                "a committed superblock's blob_count does not fit its frames",
            ));
        }
        let index_ok = aligned(self.index_off)
            && self.index_off >= FIRST_FRAME
            && self.index_len >= MIN_INDEX_FRAME_LEN
            && self.index_off.checked_add(self.index_len) == Some(self.log_end);
        if !index_ok || self.next_frame_seq < 2 {
            return Err(corrupt("the index frame is not the last committed frame"));
        }
        Ok(())
    }
}

impl ChecksummedLayout for PackSuperblock {
    const LEN: usize = SLOT_LEN;
    const CRC_OFFSET: usize = SLOT_LEN - 4;

    fn encode_fields(&self, w: &mut Writer) {
        w.write_u64(self.seq);
        w.write_u64(self.index_off);
        w.write_u64(self.index_len);
        w.write_u64(self.log_end);
        w.write_u64(self.generation);
        w.write_u64(self.blob_count);
        w.write_u64(self.next_frame_seq);
        w.write_u128(self.pack_id);
        w.write_zeros(SLOT_RESERVED_LEN);
        w.write_u32(0); // CRC placeholder
    }

    fn decode_fields(r: &mut Reader<'_>) -> persia_format::Result<Self> {
        let sb = Self {
            seq: r.read_u64()?,
            index_off: r.read_u64()?,
            index_len: r.read_u64()?,
            log_end: r.read_u64()?,
            generation: r.read_u64()?,
            blob_count: r.read_u64()?,
            next_frame_seq: r.read_u64()?,
            pack_id: r.read_u128()?,
        };
        r.expect_zeros(SLOT_RESERVED_LEN)?;
        r.skip(4)?; // CRC
        Ok(sb)
    }
}

/// What the first 12288 bytes of a pack say: its header, and which slot is active with what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    /// The header (SPEC §4.2's fields).
    pub header: Header,
    /// The active slot; the next commit writes the other.
    pub slot: Slot,
    /// Its contents.
    pub superblock: PackSuperblock,
}

/// The header and both slots of a new pack file `pack_id` of the database `db_uuid`: [`FIRST_FRAME`] bytes.
///
/// # Errors
/// [`Error::PackIo`] if the header does not encode, which a new header always does (a valid page size, no
/// flags): reported rather than written short.
pub fn new_pack_prefix(db_uuid: [u8; 16], pack_id: u128) -> Result<Vec<u8>> {
    let mut w = Writer::new();
    Header::new(db_uuid)
        .encode_with(&mut w, &BLOB_PACK)
        .map_err(|e| Error::PackIo {
            op: "encode header",
            source: std::io::Error::other(e),
        })?;
    let slot = PackSuperblock::new_pack(pack_id).encode();
    w.write_bytes(&slot);
    w.write_bytes(&slot);
    let bytes = w.into_inner();
    debug_assert_eq!(u64_from_usize(bytes.len()), FIRST_FRAME);
    Ok(bytes)
}

/// Decodes the header and picks the active slot (SPEC §4.2, §4.3), in that order. Does not check the slot against
/// the file ([`PackSuperblock::check_bounds`]).
///
/// # Errors
/// [`Error::Corrupt`] (a short prefix, a bad magic, a CRC mismatch, no valid slot, conflicting slots), or
/// [`Error::PackUnsupported`] for a newer major or unknown `incompat_flags`.
pub fn decode_prefix(bytes: &[u8]) -> Result<Prefix> {
    let mut r = Reader::new(bytes);
    let header = Header::decode_with(&mut r, &BLOB_PACK).map_err(from_format)?;
    debug_assert_eq!(r.position(), HEADER_LEN);
    let mut slots = r.sub_reader(SLOTS_LEN).map_err(from_format)?;
    let a_offset = Slot::A.offset();
    let b_offset = Slot::B.offset();
    let a = slots
        .sub_reader(SLOT_LEN)
        .and_then(|mut s| Checksummed::decode(&mut s));
    let b = slots
        .sub_reader(SLOT_LEN)
        .and_then(|mut s| Checksummed::decode(&mut s));
    let (slot, Checksummed(superblock)) = pick_active(
        a,
        b,
        |s: &Checksummed<PackSuperblock>| s.0.seq,
        a_offset,
        b_offset,
    )
    .map_err(from_format)?;
    Ok(Prefix {
        header,
        slot,
        superblock,
    })
}

/// The head of a `BlobChunk` payload (SPEC §9.3), followed by `len` bytes of the chunk:
///
/// ```text
/// 0   16  upload_id  u128
/// 16  4   chunk_idx  u32
/// 20  4   len        u32, 1..=64 MiB
/// 24  4   crc        u32, CRC32C of the chunk's bytes
/// 28  4   reserved   zero
/// 32  ..  bytes      len bytes
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHead {
    /// The upload the chunk was staged under.
    pub upload_id: UploadId,
    /// Its index in the blob.
    pub index: u32,
    /// Its length.
    pub len: u32,
    /// The CRC32C of its bytes.
    pub crc: u32,
}

/// One whole `BlobChunk` frame (header, payload, trailer) for `bytes`, chunk `index` of `upload_id`, numbered
/// `frame_seq`. The chunk is copied once.
///
/// # Errors
/// [`Error::InvalidManifest`] for an empty chunk or one over [`MAX_CHUNK_LEN`] bytes (chunks are cut by the
/// chunking rules, SPEC §9.2, so neither exists).
pub fn encode_chunk_frame(
    frame_seq: u64,
    upload_id: UploadId,
    index: u32,
    bytes: &[u8],
) -> Result<Vec<u8>> {
    encode_chunk(frame_seq, upload_id, index, bytes).map(|(frame, _)| frame)
}

/// [`encode_chunk_frame`], and the head it wrote (the chunk's length and CRC32C, which a writer records).
///
/// # Errors
/// As [`encode_chunk_frame`].
pub fn encode_chunk(
    frame_seq: u64,
    upload_id: UploadId,
    index: u32,
    bytes: &[u8],
) -> Result<(Vec<u8>, ChunkHead)> {
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|len| (1..=MAX_CHUNK_LEN).contains(len))
        .ok_or(Error::InvalidManifest {
            what: "a chunk that is empty or over 64 MiB",
        })?;
    let chunk = ChunkHead {
        upload_id,
        index,
        len,
        crc: crc32c(bytes),
    };
    let mut head = Writer::new();
    head.write_u128(upload_id.get());
    head.write_u32(index);
    head.write_u32(len);
    head.write_u32(chunk.crc);
    head.write_u32(0);
    let payload_len = u64_from_usize(CHUNK_HEAD_LEN).saturating_add(u64::from(len));
    let frame = frame_bytes(
        FrameKind::BlobChunk,
        frame_seq,
        &[head.as_slice(), bytes],
        payload_len,
    )?;
    Ok((frame, chunk))
}

/// Decodes the [`CHUNK_HEAD_LEN`]-byte head of a `BlobChunk` payload, without the chunk's bytes: what a reader that
/// only checks a chunk frame against a manifest reads (ADR-0049 item 9).
///
/// # Errors
/// [`Error::Corrupt`] for fewer than [`CHUNK_HEAD_LEN`] bytes, a `len` of 0 or over 64 MiB, or non-zero reserved bytes.
pub fn decode_chunk_head(head: &[u8]) -> Result<ChunkHead> {
    let short = || corrupt("a chunk frame shorter than its head");
    let mut r = Reader::new(head);
    let upload_id = UploadId::new(r.read_u128().map_err(|_| short())?);
    let index = r.read_u32().map_err(|_| short())?;
    let len = r.read_u32().map_err(|_| short())?;
    let crc = r.read_u32().map_err(|_| short())?;
    if r.read_u32().map_err(|_| short())? != 0 {
        return Err(corrupt("non-zero reserved bytes in a chunk frame"));
    }
    if !(1..=MAX_CHUNK_LEN).contains(&len) {
        return Err(corrupt("a chunk frame's len is 0 or over 64 MiB"));
    }
    Ok(ChunkHead {
        upload_id,
        index,
        len,
        crc,
    })
}

/// Decodes a `BlobChunk` payload: its head and the chunk's bytes. Does not recompute the CRC32C (the frame's CRC
/// covers the bytes, and a reader compares `crc` and the bytes with the blob manifest's, SPEC §9.3).
///
/// # Errors
/// [`Error::Corrupt`] for a payload shorter than its head, a `len` of 0 or over 64 MiB or that is not the rest of
/// the payload, or non-zero reserved bytes.
pub fn decode_chunk_payload(payload: &[u8]) -> Result<(ChunkHead, &[u8])> {
    let head = decode_chunk_head(payload)?;
    let bytes = payload.get(CHUNK_HEAD_LEN..).unwrap_or_default();
    if u64_from_usize(bytes.len()) != u64::from(head.len) {
        return Err(corrupt(
            "a chunk frame's len is not the length of its bytes",
        ));
    }
    Ok((head, bytes))
}

/// One whole `BlobIndex` frame numbered `frame_seq`: the blob manifest (SPEC §9.1.1) and, per chunk, the offset of
/// the `BlobChunk` frame holding it (SPEC §9.3):
///
/// ```text
/// 0   4    manifest_len  u32, the blob manifest's length (76 + 4·n in version 1)
/// 4   4    reserved      zero
/// 8   ..   manifest      the blob manifest, then zeros to a multiple of 8
/// ..  8·n  chunk_off     u64 per chunk, in chunk order: the offset of its BlobChunk frame
/// ```
///
/// # Errors
/// [`Error::InvalidManifest`] when `offsets` does not hold one offset per chunk, or the manifest is over
/// `u32::MAX` bytes.
pub fn encode_index_frame(
    frame_seq: u64,
    manifest: &BlobManifest,
    offsets: &[u64],
) -> Result<Vec<u8>> {
    if u64_from_usize(offsets.len()) != u64::from(manifest.chunk_count()) {
        return Err(Error::InvalidManifest {
            what: "not one chunk frame offset per chunk",
        });
    }
    let encoded = manifest.encode();
    let manifest_len = u32::try_from(encoded.len()).map_err(|_| Error::InvalidManifest {
        what: "a blob manifest over u32::MAX bytes",
    })?;
    let mut w = Writer::new();
    w.write_u32(manifest_len);
    w.write_u32(0);
    w.write_bytes(&encoded);
    w.write_zeros(pad8(encoded.len()));
    for &offset in offsets {
        w.write_u64(offset);
    }
    let payload = w.into_inner();
    frame_bytes(
        FrameKind::BlobIndex,
        frame_seq,
        &[&payload],
        u64_from_usize(payload.len()),
    )
}

/// Decodes a `BlobIndex` payload found in a frame at `frame_offset`: the blob manifest and its chunk frames'
/// offsets. Allocates at most the payload's length.
///
/// # Errors
/// - the blob manifest's errors ([`BlobManifest::decode`]): [`Error::Corrupt`], [`Error::UnsupportedVersion`],
///   [`Error::UnsupportedFlags`];
/// - [`Error::Corrupt`] for a short payload, non-zero reserved or padding bytes, a `manifest_len` past the payload,
///   not exactly one offset per chunk after it, or an offset that is not 8-aligned in `12288..frame_offset` (a chunk
///   is staged before the index that names it).
pub fn decode_index_payload(payload: &[u8], frame_offset: u64) -> Result<(BlobManifest, Vec<u64>)> {
    let short = || corrupt("an index frame shorter than its head");
    let mut r = Reader::new(payload);
    let manifest_len = usize_from_u32(r.read_u32().map_err(|_| short())?);
    if r.read_u32().map_err(|_| short())? != 0 {
        return Err(corrupt("non-zero reserved bytes in an index frame"));
    }
    let manifest_bytes = r
        .read_bytes(manifest_len)
        .map_err(|_| corrupt("an index frame's manifest_len runs past its payload"))?;
    let padding = r
        .read_bytes(pad8(manifest_len))
        .map_err(|_| corrupt("an index frame's manifest padding runs past its payload"))?;
    if padding.iter().any(|&b| b != 0) {
        return Err(corrupt("non-zero padding in an index frame"));
    }
    let manifest = BlobManifest::decode(manifest_bytes)?;
    let rest = r.rest();
    if u64_from_usize(rest.len()) != u64::from(manifest.chunk_count()).saturating_mul(8) {
        return Err(corrupt("an index frame does not hold one offset per chunk"));
    }
    let offsets: Vec<u64> = rest
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| u64::from_le_bytes(*b))
        .collect();
    if offsets
        .iter()
        .any(|&o| !o.is_multiple_of(8) || o < FIRST_FRAME || o >= frame_offset)
    {
        return Err(corrupt(
            "a chunk frame offset is not a frame before its index",
        ));
    }
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    if sorted.windows(2).any(|w| w.first() == w.get(1)) {
        return Err(corrupt("two chunks of one index in one frame"));
    }
    Ok((manifest, offsets))
}

/// Validates the frame that `bytes` holds exactly, of kind `kind` and stored uncompressed, and returns its
/// `frame_seq` and payload. `bytes` start at absolute offset `offset`.
///
/// # Errors
/// [`Error::Corrupt`] for an invalid frame (SPEC §4.4), one that is not exactly `bytes` long, of another kind, or
/// compressed (a pack never compresses, SPEC §9.2); [`Error::PackUnsupported`] for unknown frame flags.
pub fn decode_frame(bytes: &[u8], offset: u64, kind: FrameKind) -> Result<(u64, &[u8])> {
    let mut r = Reader::with_base_offset(bytes, offset);
    let frame = Frame::decode(&mut r).map_err(from_format)?;
    if !r.is_empty() {
        return Err(corrupt(
            "a frame is shorter than the bytes that should hold it",
        ));
    }
    if frame.kind() != kind {
        return Err(corrupt("a frame of another kind than expected"));
    }
    if frame.is_compressed() {
        return Err(corrupt(
            "a compressed frame (a pack stores blobs as they are)",
        ));
    }
    Ok((frame.frame_seq(), frame.stored_payload()))
}

/// One frame of `kind` whose payload is `parts` in order, `payload_len` bytes in all.
fn frame_bytes(
    kind: FrameKind,
    frame_seq: u64,
    parts: &[&[u8]],
    payload_len: u64,
) -> Result<Vec<u8>> {
    let encode = |_| Error::InvalidManifest {
        what: "a frame too large to encode",
    };
    let mut enc = FrameEncoder::new(kind, frame_seq, payload_len).map_err(encode)?;
    let total = usize::try_from(enc.total_len()).map_err(|_| Error::InvalidManifest {
        what: "a frame larger than this platform can address",
    })?;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(enc.header());
    for part in parts {
        enc.update(part).map_err(encode)?;
        out.extend_from_slice(part);
    }
    out.extend_from_slice(&enc.finish().map_err(encode)?);
    Ok(out)
}

/// Zeros after `len` bytes up to a multiple of 8.
const fn pad8(len: usize) -> usize {
    len.wrapping_neg() & 7
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_id_of;

    fn manifest(data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
        let crcs = data.chunks(chunk as usize).map(crc32c).collect();
        BlobManifest::new(
            blob_id_of(data),
            data.len() as u64,
            ChunkSize::from_encoding(chunk).unwrap(),
            UploadId::new(upload),
            crcs,
        )
        .unwrap()
    }

    #[test]
    fn a_new_prefix_decodes_to_the_new_pack_state() {
        let bytes = new_pack_prefix([3; 16], 7).unwrap();
        assert_eq!(bytes.len() as u64, FIRST_FRAME);
        let p = decode_prefix(&bytes).unwrap();
        assert_eq!(
            (p.slot, p.superblock),
            (Slot::A, PackSuperblock::new_pack(7))
        );
        assert_eq!(p.header.db_uuid, [3; 16]);
        p.superblock.check_bounds(FIRST_FRAME).unwrap();
    }

    #[test]
    fn a_container_header_is_not_a_pack_header() {
        let mut w = Writer::new();
        Header::new([0; 16]).encode(&mut w).unwrap();
        let mut bytes = w.into_inner();
        bytes.extend(PackSuperblock::new_pack(7).encode());
        bytes.extend(PackSuperblock::new_pack(7).encode());
        assert!(decode_prefix(&bytes).unwrap_err().is_corruption());
        let pack = new_pack_prefix([0; 16], 7).unwrap();
        assert!(Header::decode(&mut Reader::new(&pack)).is_err());
    }

    #[test]
    fn the_higher_valid_slot_wins_and_a_damaged_one_is_skipped() {
        let mut bytes = new_pack_prefix([0; 16], 7).unwrap();
        let newer = PackSuperblock {
            seq: 1,
            ..PackSuperblock::new_pack(7)
        };
        bytes[8192..12288].copy_from_slice(&newer.encode());
        let p = decode_prefix(&bytes).unwrap();
        assert_eq!((p.slot, p.superblock.seq), (Slot::B, 1));
        bytes[8200] ^= 1;
        assert_eq!(decode_prefix(&bytes).unwrap().slot, Slot::A);
        bytes[4100] ^= 1;
        assert!(decode_prefix(&bytes).unwrap_err().is_corruption());
    }

    type Edit = (&'static str, fn(&mut PackSuperblock));

    #[test]
    fn check_bounds_rejects_each_rule() {
        let committed = PackSuperblock {
            seq: 1,
            index_off: FIRST_FRAME + 120,
            index_len: 128,
            log_end: FIRST_FRAME + 248,
            generation: 1,
            blob_count: 2,
            next_frame_seq: 3,
            pack_id: 7,
        };
        let len = committed.log_end;
        committed.check_bounds(len).unwrap();
        let edits: [Edit; 15] = [
            ("log_end unaligned", |s| s.log_end += 1),
            ("log_end below the frames", |s| s.log_end = FIRST_FRAME - 8),
            ("generation 0", |s| s.generation = 0),
            ("next_frame_seq 0", |s| s.next_frame_seq = 0),
            ("next_frame_seq 1", |s| s.next_frame_seq = 1),
            ("index not last", |s| s.index_off -= 8),
            ("index unaligned", |s| {
                s.index_off += 4;
                s.index_len -= 4;
            }),
            ("index too short", |s| {
                s.index_off += 16;
                s.index_len -= 16;
            }),
            ("index below the frames", |s| {
                s.index_off = 8;
                s.index_len = s.log_end - 8;
            }),
            ("no blob", |s| s.blob_count = 0),
            ("more blobs than frames", |s| s.blob_count = 3),
            ("huge blob_count", |s| s.blob_count = u64::MAX),
            ("seq 0 with frames", |s| s.seq = 0),
            ("index_off overflow", |s| s.index_off = u64::MAX - 7),
            ("index_len overflow", |s| s.index_len = u64::MAX - 7),
        ];
        for (what, edit) in edits {
            let mut sb = committed;
            edit(&mut sb);
            assert!(sb.check_bounds(len).is_err(), "{what}: {sb:?}");
        }
        assert!(committed.check_bounds(len - 8).is_err());
        // An uncommitted slot of a rewritten pack keeps its generation and id.
        PackSuperblock {
            generation: 5,
            ..PackSuperblock::new_pack(9)
        }
        .check_bounds(FIRST_FRAME)
        .unwrap();
        // The shortest index frame is an empty blob's.
        let empty = manifest(b"", 4, 1);
        assert_eq!(
            encode_index_frame(1, &empty, &[]).unwrap().len() as u64,
            MIN_INDEX_FRAME_LEN
        );
    }

    #[test]
    fn a_slot_round_trips_every_field() {
        let sb = PackSuperblock {
            seq: 3,
            index_off: 0x1_0000,
            index_len: 0x200,
            log_end: 0x1_0200,
            generation: 4,
            blob_count: 5,
            next_frame_seq: 6,
            pack_id: 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210,
        };
        let bytes = sb.encode();
        assert_eq!(bytes.len(), SLOT_LEN);
        assert_eq!(&bytes[32..40], &4_u64.to_le_bytes());
        assert_eq!(&bytes[56..72], &sb.pack_id.to_le_bytes());
        assert!(bytes[72..4092].iter().all(|&b| b == 0));
        assert_eq!(&bytes[4092..], &crc32c(&bytes[..4092]).to_le_bytes());
        let Checksummed(back) =
            Checksummed::<PackSuperblock>::decode(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(back, sb);
    }

    #[test]
    fn chunk_frames_round_trip() {
        let frame = encode_chunk_frame(9, UploadId::new(7), 3, b"chunk bytes").unwrap();
        assert_eq!(
            frame.len() as u64,
            persia_format::frame::frame_len(32 + 11).unwrap()
        );
        let (seq, payload) = decode_frame(&frame, FIRST_FRAME, FrameKind::BlobChunk).unwrap();
        assert_eq!(seq, 9);
        let (head, bytes) = decode_chunk_payload(payload).unwrap();
        assert_eq!(
            head,
            ChunkHead {
                upload_id: UploadId::new(7),
                index: 3,
                len: 11,
                crc: crc32c(b"chunk bytes")
            }
        );
        assert_eq!(bytes, b"chunk bytes");
        assert!(decode_frame(&frame, FIRST_FRAME, FrameKind::BlobIndex).is_err());
        assert!(encode_chunk_frame(1, UploadId::new(1), 0, b"").is_err());
    }

    #[test]
    fn chunk_payloads_reject_bad_heads() {
        let frame = encode_chunk_frame(1, UploadId::new(7), 0, b"abc").unwrap();
        let payload = &frame[24..24 + 35];
        decode_chunk_payload(payload).unwrap();
        for cut in [0, 31, 32, 34] {
            assert!(decode_chunk_payload(&payload[..cut]).is_err(), "{cut}");
        }
        let mut longer = payload.to_vec();
        longer.push(0);
        assert!(decode_chunk_payload(&longer).is_err());
        let mut reserved = payload.to_vec();
        reserved[28] = 1;
        assert!(decode_chunk_payload(&reserved).is_err());
        let mut zero_len = payload[..32].to_vec();
        zero_len[20..24].fill(0);
        assert!(decode_chunk_payload(&zero_len).is_err());
    }

    #[test]
    fn index_frames_round_trip() {
        let m = manifest(b"0123456789", 4, 5);
        let offsets = [FIRST_FRAME, FIRST_FRAME + 64, FIRST_FRAME + 128];
        let frame = encode_index_frame(4, &m, &offsets).unwrap();
        let at = FIRST_FRAME + 192;
        let (seq, payload) = decode_frame(&frame, at, FrameKind::BlobIndex).unwrap();
        assert_eq!(seq, 4);
        assert_eq!(
            decode_index_payload(payload, at).unwrap(),
            (m.clone(), offsets.to_vec())
        );
        // An offset at or after the index frame cannot name a chunk staged before it.
        assert!(decode_index_payload(payload, FIRST_FRAME + 128).is_err());
        assert!(encode_index_frame(4, &m, &offsets[..2]).is_err());
        let twice =
            encode_index_frame(4, &m, &[FIRST_FRAME, FIRST_FRAME, FIRST_FRAME + 64]).unwrap();
        let (_, payload) = decode_frame(&twice, at, FrameKind::BlobIndex).unwrap();
        assert!(
            decode_index_payload(payload, at).is_err(),
            "two chunks in one frame"
        );
        let empty = manifest(b"", 4, 6);
        let frame = encode_index_frame(5, &empty, &[]).unwrap();
        let (_, payload) = decode_frame(&frame, at, FrameKind::BlobIndex).unwrap();
        assert_eq!(
            decode_index_payload(payload, at).unwrap().1,
            Vec::<u64>::new()
        );
    }

    #[allow(clippy::cast_possible_truncation)]
    #[test]
    fn index_payloads_reject_bad_layouts() {
        let m = manifest(b"0123456789", 4, 5);
        let offsets = [FIRST_FRAME, FIRST_FRAME + 64, FIRST_FRAME + 128];
        let frame = encode_index_frame(4, &m, &offsets).unwrap();
        let len = u64::from_le_bytes(frame[8..16].try_into().unwrap()) as usize;
        let payload = frame[24..24 + len].to_vec();
        let at = u64::MAX;
        decode_index_payload(&payload, at).unwrap();
        let edits: [(usize, u8); 5] = [(0, 1), (4, 1), (8 + 88, 1), (8 + 50, 1), (8 + 87, 1)];
        for (i, v) in edits {
            let mut p = payload.clone();
            p[i] ^= v;
            assert!(decode_index_payload(&p, at).is_err(), "byte {i}");
        }
        for cut in [0, 7, 8, 96, payload.len() - 1] {
            assert!(decode_index_payload(&payload[..cut], at).is_err(), "{cut}");
        }
        let mut longer = payload.clone();
        longer.extend([0; 8]);
        assert!(decode_index_payload(&longer, at).is_err());
    }
}
