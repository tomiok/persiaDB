//! The WAL chunk encoding (SPEC §8.2.1, ADR-0028): a header, then length-prefixed records, each with a CRC32C.

use persia_format::checksum::crc32c;

use crate::document::{MAX_DOCUMENT_LEN, MAX_ID_LEN};
use crate::schema::{NameProblem, collection_name_problem};

/// `"PWAL"`: the first bytes of every chunk.
pub const MAGIC: [u8; 4] = *b"PWAL";
/// The encoding version this build writes and reads.
pub const VERSION: u32 = 1;
/// Longest chunk, in bytes: the bomb limit readers pass for a `WalChunk` frame (SPEC §4.4, §8.2.1).
pub const MAX_CHUNK_LEN: usize = 64 << 20;
/// Magic, version and record count.
pub(super) const HEADER_LEN: usize = 12;
/// `body_len` and the CRC around a record's body.
const RECORD_OVERHEAD: usize = 8;
/// `op`, `name_len` and at least one name byte.
const MIN_BODY_LEN: usize = 3;

const OP_PUT: u8 = 1;
const OP_DELETE: u8 = 2;

/// What a record does (SPEC §8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalOp {
    /// An upsert; the payload is the document's encoding with every field (SPEC §6.4.2).
    Put,
    /// A delete; the payload is the document id.
    Delete,
}

/// One operation of the log: borrowed from the caller when written, from the chunk when read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalRecord<'a> {
    op: WalOp,
    collection: &'a str,
    payload: &'a [u8],
}

/// Why a record cannot be written: the caller's input, not stored bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// The collection name breaks the naming rule (SPEC §6.2.1).
    #[error("invalid collection name {name:?}: {problem}")]
    CollectionName {
        /// The name.
        name: String,
        /// What is wrong with it.
        problem: NameProblem,
    },
    /// A put's document encoding is empty or longer than [`MAX_DOCUMENT_LEN`].
    #[error("document encoding of {len} bytes, must be 1..={MAX_DOCUMENT_LEN}")]
    DocumentLen {
        /// Its length.
        len: usize,
    },
    /// A delete's id is empty or longer than [`MAX_ID_LEN`] bytes.
    #[error("document id of {len} bytes, must be 1..={MAX_ID_LEN}")]
    IdLen {
        /// Its length.
        len: usize,
    },
}

impl<'a> WalRecord<'a> {
    /// A put of `document`, the [`crate::document::Document::encode`] of a document with [`FieldSet::All`] for
    /// `collection`'s schema. The decoder checks only its length; replay decodes and indexes it, so log only a
    /// document the collection's memtable accepted (SPEC §8.2.1): one it rejects fails every later open.
    ///
    /// [`FieldSet::All`]: crate::document::FieldSet::All
    ///
    /// # Errors
    /// [`RecordError::CollectionName`], [`RecordError::DocumentLen`].
    pub fn put(collection: &'a str, document: &'a [u8]) -> Result<Self, RecordError> {
        check_name(collection).map_err(|problem| RecordError::CollectionName {
            name: collection.to_owned(),
            problem,
        })?;
        if !(1..=MAX_DOCUMENT_LEN).contains(&document.len()) {
            return Err(RecordError::DocumentLen {
                len: document.len(),
            });
        }
        Ok(Self {
            op: WalOp::Put,
            collection,
            payload: document,
        })
    }

    /// A delete of the document with id `id` in `collection`.
    ///
    /// # Errors
    /// [`RecordError::CollectionName`], [`RecordError::IdLen`].
    pub fn delete(collection: &'a str, id: &'a str) -> Result<Self, RecordError> {
        check_name(collection).map_err(|problem| RecordError::CollectionName {
            name: collection.to_owned(),
            problem,
        })?;
        if !(1..=MAX_ID_LEN).contains(&id.len()) {
            return Err(RecordError::IdLen { len: id.len() });
        }
        Ok(Self {
            op: WalOp::Delete,
            collection,
            payload: id.as_bytes(),
        })
    }

    /// What the record does.
    pub fn op(&self) -> WalOp {
        self.op
    }

    /// The collection it applies to.
    pub fn collection(&self) -> &'a str {
        self.collection
    }

    /// The payload: a put's document encoding, a delete's id as bytes.
    pub fn payload(&self) -> &'a [u8] {
        self.payload
    }

    /// A delete's id; `None` for a put.
    pub fn deleted_id(&self) -> Option<&'a str> {
        match self.op {
            // Valid UTF-8: checked by `delete` and by the decoder.
            WalOp::Delete => std::str::from_utf8(self.payload).ok(),
            WalOp::Put => None,
        }
    }

    /// Bytes the record takes in a chunk.
    pub(super) fn encoded_len(&self) -> usize {
        // Bounded by the constructors: a 64-byte name and a 16 MiB payload.
        RECORD_OVERHEAD + 2 + self.collection.len() + self.payload.len()
    }

    /// Appends the record to `out`.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        // Fits: `encoded_len` is far below `u32::MAX`, and a name below 256 bytes (constructors).
        let body_len = (2 + self.collection.len() + self.payload.len()) as u32;
        out.extend_from_slice(&body_len.to_le_bytes());
        out.push(match self.op {
            WalOp::Put => OP_PUT,
            WalOp::Delete => OP_DELETE,
        });
        out.push(self.collection.len() as u8);
        out.extend_from_slice(self.collection.as_bytes());
        out.extend_from_slice(self.payload);
        let crc = crc32c(out.get(start..).unwrap_or_default());
        out.extend_from_slice(&crc.to_le_bytes());
    }
}

fn check_name(name: &str) -> Result<(), NameProblem> {
    collection_name_problem(name).map_or(Ok(()), Err)
}

/// Encodes `records` as one chunk.
///
/// ```
/// use persia_engine::wal::{WalRecord, decode_chunk, encode_chunk};
///
/// let chunk = encode_chunk(&[WalRecord::delete("notes", "n1")?])?;
/// let records: Vec<_> = decode_chunk(&chunk)?.collect::<Result<_, _>>()?;
/// assert_eq!(records[0].deleted_id(), Some("n1"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
/// [`ChunkFull`] if there are no records, more than `u32::MAX`, or more bytes than [`MAX_CHUNK_LEN`].
pub fn encode_chunk(records: &[WalRecord<'_>]) -> Result<Vec<u8>, ChunkFull> {
    let mut chunk = ChunkBuilder::new();
    for record in records {
        chunk.push(record)?;
    }
    chunk.finish().map(<[u8]>::to_vec).ok_or(ChunkFull)
}

/// A chunk cannot take a record: it would pass [`MAX_CHUNK_LEN`] or `u32::MAX` records (or, for
/// [`encode_chunk`], there were no records).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the WAL chunk is full (at most {MAX_CHUNK_LEN} bytes, at least one record)")]
pub struct ChunkFull;

/// A chunk being filled with records.
#[derive(Debug, Clone)]
pub(super) struct ChunkBuilder {
    buf: Vec<u8>,
    count: u32,
}

impl ChunkBuilder {
    pub(super) fn new() -> Self {
        let mut buf = Vec::new();
        Self::start(&mut buf);
        Self { buf, count: 0 }
    }

    fn start(buf: &mut Vec<u8>) {
        buf.extend_from_slice(&MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
    }

    /// Bytes of the chunk so far, header included.
    pub(super) fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether `record` fits.
    pub(super) fn fits(&self, record: &WalRecord<'_>) -> bool {
        self.count < u32::MAX
            && self
                .buf
                .len()
                .checked_add(record.encoded_len())
                .is_some_and(|len| len <= MAX_CHUNK_LEN)
    }

    /// Whether `record` fits an empty chunk.
    pub(super) fn fits_empty(record: &WalRecord<'_>) -> bool {
        HEADER_LEN.saturating_add(record.encoded_len()) <= MAX_CHUNK_LEN
    }

    /// Appends `record`, or changes nothing.
    pub(super) fn push(&mut self, record: &WalRecord<'_>) -> Result<(), ChunkFull> {
        if !self.fits(record) {
            return Err(ChunkFull);
        }
        record.encode_into(&mut self.buf);
        self.count += 1;
        Ok(())
    }

    /// The finished chunk; `None` if it has no records.
    pub(super) fn finish(&mut self) -> Option<&[u8]> {
        if self.count == 0 {
            return None;
        }
        self.buf
            .get_mut(8..HEADER_LEN)?
            .copy_from_slice(&self.count.to_le_bytes());
        Some(&self.buf)
    }

    /// Empties the chunk, keeping its memory.
    pub(super) fn clear(&mut self) {
        self.buf.clear();
        Self::start(&mut self.buf);
        self.count = 0;
    }
}

/// Why a chunk could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WalError {
    /// The bytes are not a valid chunk.
    #[error("corrupt WAL chunk at offset {offset}: {reason}")]
    Corrupt {
        /// Offset from the start of the chunk.
        offset: u64,
        /// What was wrong.
        reason: WalCorruption,
    },
    /// An encoding version above the one this build reads.
    #[error("WAL chunk encoding version {version} (this build reads {VERSION})")]
    UnsupportedVersion {
        /// The version read.
        version: u32,
    },
}

impl WalError {
    /// Whether the input was corrupt, as opposed to valid input from a newer format.
    pub fn is_corruption(&self) -> bool {
        matches!(self, Self::Corrupt { .. })
    }
}

/// What is wrong with a corrupt chunk (SPEC §8.2.1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WalCorruption {
    /// The input ended inside a value, or a length runs past it.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// The input is longer than [`MAX_CHUNK_LEN`].
    #[error("{len} bytes, at most {MAX_CHUNK_LEN}")]
    TooLarge {
        /// Its length.
        len: usize,
    },
    /// The input does not start with the chunk magic.
    #[error("not a WAL chunk (bad magic)")]
    Magic,
    /// Encoding version 0.
    #[error("encoding version 0")]
    ZeroVersion,
    /// A record count of 0.
    #[error("no records")]
    NoRecords,
    /// Bytes after the last record.
    #[error("trailing bytes after the last record")]
    TrailingBytes,
    /// A record body too short for its op and name.
    #[error("record body of {len} bytes, at least {MIN_BODY_LEN}")]
    BodyLen {
        /// The stored `body_len`.
        len: u32,
    },
    /// A record's CRC32C does not match.
    #[error("record checksum mismatch: stored {stored:#010x}, computed {computed:#010x}")]
    ChecksumMismatch {
        /// The stored checksum.
        stored: u32,
        /// The checksum of the bytes.
        computed: u32,
    },
    /// An unknown op.
    #[error("unknown op {op}")]
    Op {
        /// The op byte.
        op: u8,
    },
    /// The collection name runs past the body, is not UTF-8, or breaks the naming rule.
    #[error("invalid collection name")]
    CollectionName,
    /// A put's document encoding is empty or too long.
    #[error("document encoding of {len} bytes, must be 1..={MAX_DOCUMENT_LEN}")]
    DocumentLen {
        /// Its length.
        len: usize,
    },
    /// A delete's id is empty, too long, or not UTF-8.
    #[error("invalid document id")]
    Id,
}

fn corrupt(offset: usize, reason: WalCorruption) -> WalError {
    WalError::Corrupt {
        offset: offset as u64,
        reason,
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let b: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(b))
}

/// Checks a chunk's header and returns its records, in order. The iterator checks each record as it reaches it
/// (CRC first), yields the first problem as an error, and then ends; after the last record it checks that no bytes
/// remain.
///
/// # Errors
/// [`WalError::Corrupt`] for a bad header, [`WalError::UnsupportedVersion`] for a newer encoding.
pub fn decode_chunk(bytes: &[u8]) -> Result<ChunkRecords<'_>, WalError> {
    if bytes.len() > MAX_CHUNK_LEN {
        return Err(corrupt(0, WalCorruption::TooLarge { len: bytes.len() }));
    }
    if bytes.get(..4).is_some_and(|m| m != MAGIC) {
        return Err(corrupt(0, WalCorruption::Magic));
    }
    let eof = |at| corrupt(at, WalCorruption::UnexpectedEof);
    if bytes.len() < 4 {
        return Err(eof(bytes.len()));
    }
    let version = u32_at(bytes, 4).ok_or_else(|| eof(bytes.len()))?;
    match version {
        0 => return Err(corrupt(4, WalCorruption::ZeroVersion)),
        VERSION => {}
        version => return Err(WalError::UnsupportedVersion { version }),
    }
    let count = u32_at(bytes, 8).ok_or_else(|| eof(bytes.len()))?;
    if count == 0 {
        return Err(corrupt(8, WalCorruption::NoRecords));
    }
    Ok(ChunkRecords {
        bytes,
        pos: HEADER_LEN,
        remaining: count,
        done: false,
    })
}

/// The records of a chunk, from [`decode_chunk`].
#[derive(Debug, Clone)]
pub struct ChunkRecords<'a> {
    bytes: &'a [u8],
    pos: usize,
    remaining: u32,
    done: bool,
}

impl<'a> ChunkRecords<'a> {
    /// Offset of the next record from the start of the chunk (after the last, the end of the records).
    pub fn offset(&self) -> u64 {
        self.pos as u64
    }

    fn record(&mut self) -> Result<WalRecord<'a>, WalError> {
        let start = self.pos;
        let eof = |at| corrupt(at, WalCorruption::UnexpectedEof);
        let body_len = u32_at(self.bytes, start).ok_or_else(|| eof(self.bytes.len()))?;
        let len = body_len as usize;
        if len < MIN_BODY_LEN {
            return Err(corrupt(start, WalCorruption::BodyLen { len: body_len }));
        }
        // `start` lies inside the chunk (at most 64 MiB), but `len` is stored: up to `u32::MAX`.
        let body_start = start + 4;
        let crc_at = body_start
            .checked_add(len)
            .ok_or_else(|| eof(self.bytes.len()))?;
        let stored = u32_at(self.bytes, crc_at).ok_or_else(|| eof(self.bytes.len()))?;
        let covered = self
            .bytes
            .get(start..crc_at)
            .ok_or_else(|| eof(self.bytes.len()))?;
        let computed = crc32c(covered);
        if stored != computed {
            return Err(corrupt(
                crc_at,
                WalCorruption::ChecksumMismatch { stored, computed },
            ));
        }
        let body = covered.get(4..).unwrap_or_default();
        let (op, name_len, rest) = match body {
            [op, name_len, rest @ ..] => (*op, usize::from(*name_len), rest),
            _ => return Err(eof(body_start)),
        };
        let op = match op {
            OP_PUT => WalOp::Put,
            OP_DELETE => WalOp::Delete,
            op => return Err(corrupt(body_start, WalCorruption::Op { op })),
        };
        let name_at = body_start + 1;
        let collection = rest
            .get(..name_len)
            .and_then(|n| std::str::from_utf8(n).ok())
            .filter(|n| check_name(n).is_ok())
            .ok_or_else(|| corrupt(name_at, WalCorruption::CollectionName))?;
        let payload = rest.get(name_len..).unwrap_or_default();
        let payload_at = name_at + 1 + name_len;
        match op {
            WalOp::Put if !(1..=MAX_DOCUMENT_LEN).contains(&payload.len()) => {
                return Err(corrupt(
                    payload_at,
                    WalCorruption::DocumentLen { len: payload.len() },
                ));
            }
            WalOp::Delete
                if !(1..=MAX_ID_LEN).contains(&payload.len())
                    || std::str::from_utf8(payload).is_err() =>
            {
                return Err(corrupt(payload_at, WalCorruption::Id));
            }
            _ => {}
        }
        self.pos = crc_at + 4;
        Ok(WalRecord {
            op,
            collection,
            payload,
        })
    }
}

impl<'a> Iterator for ChunkRecords<'a> {
    type Item = Result<WalRecord<'a>, WalError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.remaining == 0 {
            self.done = true;
            return (self.pos != self.bytes.len())
                .then(|| Err(corrupt(self.pos, WalCorruption::TrailingBytes)));
        }
        let record = self.record();
        match record {
            Ok(_) => self.remaining -= 1,
            Err(_) => self.done = true,
        }
        Some(record)
    }
}
