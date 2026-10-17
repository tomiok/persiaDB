//! Frames: the checksummed records that follow the superblock slots in a container (SPEC §4.4; ADR-0009).
//!
//! [`encode_frame`] appends one frame, optionally with a zstd-compressed payload; [`FrameEncoder`] streams one whose
//! payload is too large to hold in memory. [`Frame::decode`] reads one frame strictly, e.g. the Manifest frame a
//! superblock points at; [`FrameHeader::decode`] reads only its first 24 bytes, so a range reader can size the read
//! of the whole frame. [`FrameScanner`] reads frames until the first invalid or unsupported one and reports why it
//! stopped ([`ScanEnd`]): a clean end, a torn tail, corruption, or a frame from a newer writer. Frames of unknown
//! kinds are yielded as [`FrameKind::Unknown`] and must be skipped.
//!
//! Scanning checks framing only. A compressed payload is decoded on demand by [`Frame::payload`], under a
//! caller-supplied bomb limit.

use std::borrow::Cow;
use std::iter::FusedIterator;

use crate::ALIGNMENT;
use crate::align::padding_for;
use crate::block::Compression;
use crate::checksum::{Crc32c, crc32c, verify_crc32c};
use crate::compress::{Codec, compress_block, decompress_block};
use crate::error::{Corruption, EncodeError, Error, Result, Unsupported, to_u64};
use crate::reader::Reader;
use crate::writer::Writer;

mod stream;

pub use stream::{DEFAULT_SCAN_CHUNK, StreamScanner};

/// The first 4 bytes of every frame.
pub const FRAME_MAGIC: [u8; 4] = *b"PFR1";
/// Bytes before the payload: magic, kind, flags, `payload_len`, `frame_seq`.
pub const FRAME_HEADER_LEN: usize = 24;
/// Offset of `frame_seq` within a frame.
const FRAME_SEQ_OFFSET: u64 = 16;
/// Bytes after the padded payload: CRC and `reserved`.
pub const FRAME_TRAILER_LEN: usize = 8;
/// The smallest frame: an empty payload.
pub const MIN_FRAME_LEN: usize = FRAME_HEADER_LEN + FRAME_TRAILER_LEN;
/// `flags` bit 0: the payload is `uncompressed_len u64` followed by one zstd frame (SPEC §4.4).
pub const FLAG_ZSTD: u32 = 1;
/// `flags` bits this build knows.
pub const KNOWN_FRAME_FLAGS: u32 = FLAG_ZSTD;

// SPEC §4.4 field offsets within a frame.
const KIND_OFFSET: usize = 4;
const PAYLOAD_LEN_OFFSET: usize = 8;

/// What a frame holds (SPEC §4.4). The on-disk byte is [`FrameKind::to_u8`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameKind {
    /// A chunk of write-ahead log (SPEC §8.2).
    WalChunk,
    /// A whole segment (SPEC §5).
    Segment,
    /// The list of live segments a commit publishes (SPEC §4.5).
    Manifest,
    /// No meaning: aligns what follows.
    Padding,
    /// A segment's deleted docids (SPEC §8.3).
    TombstoneSidecar,
    /// A kind this build does not know, written by a newer one. Readers skip it and MUST NOT interpret it.
    Unknown(UnknownKind),
}

/// An unassigned `kind` byte. Only [`FrameKind::from_u8`] builds one, so it never holds an assigned byte and each
/// byte has exactly one [`FrameKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnknownKind(u8);

impl UnknownKind {
    /// The on-disk byte.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl FrameKind {
    /// Parses the on-disk byte; every value is valid (unassigned ones are [`FrameKind::Unknown`]).
    ///
    /// ```
    /// use persia_format::frame::FrameKind;
    ///
    /// assert_eq!(FrameKind::from_u8(3), FrameKind::Manifest);
    /// assert!(matches!(FrameKind::from_u8(200), FrameKind::Unknown(k) if k.get() == 200));
    /// ```
    pub const fn from_u8(byte: u8) -> Self {
        match byte {
            1 => Self::WalChunk,
            2 => Self::Segment,
            3 => Self::Manifest,
            4 => Self::Padding,
            5 => Self::TombstoneSidecar,
            other => Self::Unknown(UnknownKind(other)),
        }
    }

    /// The on-disk byte.
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::WalChunk => 1,
            Self::Segment => 2,
            Self::Manifest => 3,
            Self::Padding => 4,
            Self::TombstoneSidecar => 5,
            Self::Unknown(kind) => kind.0,
        }
    }

    /// Whether this build knows the kind, i.e. may interpret the frame.
    pub const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

/// Total length of a frame whose payload is `payload_len` bytes: `24 + pad8(payload_len) + 8` (SPEC §4.4), or
/// `None` if that overflows `u64`.
pub fn frame_len(payload_len: u64) -> Option<u64> {
    let pad = to_u64(padding_for(payload_len, ALIGNMENT));
    payload_len
        .checked_add(pad)?
        .checked_add(to_u64(MIN_FRAME_LEN))
}

/// Appends one frame holding `payload`.
///
/// With [`Compression::Zstd`] the payload is stored compressed (flags bit 0) when that makes the frame smaller,
/// and as is otherwise. Frames are a multiple of 8 bytes, so a frame appended at an aligned offset leaves the next
/// one aligned too (SPEC §4.1).
///
/// ```
/// use persia_format::block::Compression;
/// use persia_format::frame::{FrameKind, FrameScanner, encode_frame};
/// use persia_format::Writer;
///
/// let mut w = Writer::new();
/// encode_frame(&mut w, FrameKind::Manifest, 1, b"live segments", Compression::None)?;
/// assert_eq!(w.len(), 24 + 16 + 8);
///
/// let mut scanner = FrameScanner::new(w.as_slice(), 0);
/// let frame = scanner.next().expect("one frame");
/// assert_eq!((frame.kind(), frame.frame_seq()), (FrameKind::Manifest, 1));
/// assert_eq!(&*frame.payload(1024)?, b"live segments");
/// assert!(scanner.next().is_none());
/// assert!(scanner.end().is_some_and(|end| end.error().is_none()));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
/// [`EncodeError::InvalidCompressionLevel`] for a zstd level zstd does not accept;
/// [`EncodeError::CompressionFailed`] if zstd runs out of memory. Nothing is appended then.
pub fn encode_frame(
    w: &mut Writer,
    kind: FrameKind,
    frame_seq: u64,
    payload: &[u8],
    compression: Compression,
) -> Result<(), EncodeError> {
    let mut compressed = Vec::new();
    if let Compression::Zstd { level } = compression {
        compressed.extend_from_slice(&to_u64(payload.len()).to_le_bytes());
        compress_block(Codec::Zstd, level, payload, &mut compressed)?;
    }
    let (flags, stored) = if !compressed.is_empty() && compressed.len() < payload.len() {
        (FLAG_ZSTD, compressed.as_slice())
    } else {
        (0, payload)
    };
    write_stored(w, kind, flags, frame_seq, stored);
    Ok(())
}

/// Appends one frame around an already encoded (possibly compressed) payload.
fn write_stored(w: &mut Writer, kind: FrameKind, flags: u32, frame_seq: u64, stored: &[u8]) {
    let start = w.len();
    w.write_bytes(&FRAME_MAGIC);
    w.write_u8(kind.to_u8());
    w.write_bytes(flags.to_le_bytes().get(..3).unwrap_or_default());
    w.write_u64(to_u64(stored.len()));
    w.write_u64(frame_seq);
    w.write_bytes(stored);
    w.write_zeros(padding_for(to_u64(stored.len()), ALIGNMENT));
    let crc = crc32c(
        w.as_slice()
            .get(start.saturating_add(KIND_OFFSET)..)
            .unwrap_or_default(),
    );
    w.write_u32(crc);
    w.write_u32(0);
}

/// The first [`FRAME_HEADER_LEN`] bytes of a frame, read on their own (SPEC §4.4).
///
/// A range reader (object storage, ADR-0006) fetches these 24 bytes first to learn how many to fetch for the whole
/// frame ([`FrameHeader::total_len`]), then validates the frame with [`Frame::decode`]. Only the magic and the
/// length are checked here: the CRC covering the header sits in the frame's trailer, so nothing else in it can be
/// trusted yet. In particular unknown `flags` bits are not reported: whether they make the frame *unsupported* or
/// *invalid* depends on the CRC.
///
/// ```
/// use persia_format::block::Compression;
/// use persia_format::frame::{Frame, FrameHeader, FrameKind, encode_frame};
/// use persia_format::{Reader, Writer};
///
/// let mut w = Writer::with_base_offset(12288);
/// encode_frame(&mut w, FrameKind::Segment, 9, &[7; 100], Compression::None)?;
/// let file = w.as_slice();
///
/// // A ranged read of the header, then one of exactly the frame.
/// let header = FrameHeader::decode(&mut Reader::with_base_offset(&file[..24], 12288))?;
/// assert_eq!((header.kind(), header.payload_len(), header.total_len()), (FrameKind::Segment, 100, 136));
/// let len = usize::try_from(header.total_len())?;
/// let frame = Frame::decode(&mut Reader::with_base_offset(&file[..len], 12288))?;
/// assert_eq!(frame.frame_seq(), header.frame_seq());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    offset: u64,
    kind: FrameKind,
    flags: u32,
    payload_len: u64,
    frame_seq: u64,
    total_len: u64,
}

impl FrameHeader {
    /// The header at `offset` of a frame whose length fits in `u64`, or `None`. `flags` keeps its low 24 bits.
    fn new(
        offset: u64,
        kind: FrameKind,
        flags: u32,
        payload_len: u64,
        frame_seq: u64,
    ) -> Option<Self> {
        Some(Self {
            offset,
            kind,
            flags: flags & 0x00FF_FFFF,
            payload_len,
            frame_seq,
            total_len: frame_len(payload_len)?,
        })
    }

    /// Reads [`FRAME_HEADER_LEN`] bytes and checks the magic and that the frame's total length fits in `u64`.
    /// Whether the frame fits in the container is not checked: call [`FrameHeader::check_fits`] before fetching or
    /// allocating `total_len` bytes (SPEC §4.4).
    ///
    /// # Errors
    /// [`Error::Corrupt`] with [`Corruption::UnexpectedEof`] if fewer than [`FRAME_HEADER_LEN`] bytes remain,
    /// [`Corruption::BadFrameMagic`], or [`Corruption::UnexpectedEof`] at the `payload_len` field (needing
    /// `u64::MAX` bytes) when the total length overflows, as [`Frame::decode`] reports it. On error the position
    /// is unchanged.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let mut ahead = r.clone();
        let offset = ahead.offset();
        let available = to_u64(ahead.remaining());
        if ahead.remaining() < FRAME_HEADER_LEN {
            return Err(Error::Corrupt {
                offset,
                reason: Corruption::UnexpectedEof {
                    needed: to_u64(FRAME_HEADER_LEN),
                    available,
                },
            });
        }
        if ahead.read_array::<4>()? != FRAME_MAGIC {
            return Err(Error::Corrupt {
                offset,
                reason: Corruption::BadFrameMagic,
            });
        }
        let kind = FrameKind::from_u8(ahead.read_u8()?);
        let flags = u32::from_le_bytes(pad_flags(ahead.read_array::<3>()?));
        let len_offset = ahead.offset();
        let payload_len = ahead.read_u64()?;
        let frame_seq = ahead.read_u64()?;
        let header =
            Self::new(offset, kind, flags, payload_len, frame_seq).ok_or(Error::Corrupt {
                offset: len_offset,
                reason: Corruption::UnexpectedEof {
                    needed: u64::MAX,
                    available,
                },
            })?;
        *r = ahead;
        Ok(header)
    }

    /// The header bytes.
    fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut w = Writer::new();
        w.write_bytes(&FRAME_MAGIC);
        w.write_u8(self.kind.to_u8());
        w.write_bytes(self.flags.to_le_bytes().get(..3).unwrap_or_default());
        w.write_u64(self.payload_len);
        w.write_u64(self.frame_seq);
        // Exactly FRAME_HEADER_LEN bytes were written.
        debug_assert_eq!(w.len(), FRAME_HEADER_LEN);
        w.as_slice()
            .first_chunk()
            .copied()
            .unwrap_or([0; FRAME_HEADER_LEN])
    }

    /// Absolute offset of the frame.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Checks that the frame fits before `end`, the absolute end of the container or object: what a range reader
    /// MUST do before fetching or allocating [`FrameHeader::total_len`] bytes (SPEC §4.4, validation rule 2).
    ///
    /// ```
    /// use persia_format::frame::{FrameEncoder, FrameHeader, FrameKind};
    /// use persia_format::Reader;
    ///
    /// let enc = FrameEncoder::new(FrameKind::Segment, 1, 1 << 40)?;
    /// let header = FrameHeader::decode(&mut Reader::with_base_offset(enc.header(), 12288))?;
    /// assert!(header.check_fits(12288 + 4096).is_err()); // a 1 TiB frame in a 16 KiB file
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`Error::Corrupt`] with [`Corruption::UnexpectedEof`] at the `payload_len` field when the frame runs past
    /// `end`: the error [`Frame::decode`] gives for an input ending at `end` with at least [`MIN_FRAME_LEN`] bytes.
    pub fn check_fits(&self, end: u64) -> Result<()> {
        let available = end.saturating_sub(self.offset);
        if self.total_len <= available {
            return Ok(());
        }
        Err(Error::Corrupt {
            offset: self.offset.saturating_add(to_u64(PAYLOAD_LEN_OFFSET)),
            reason: Corruption::UnexpectedEof {
                needed: self.total_len,
                available,
            },
        })
    }

    /// The `kind` field. Unverified until the frame is decoded.
    pub const fn kind(&self) -> FrameKind {
        self.kind
    }

    /// The 24-bit `flags` field, unknown bits included. Unverified until the frame is decoded.
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    /// The `payload_len` field: the stored payload's length, without padding.
    pub const fn payload_len(&self) -> u64 {
        self.payload_len
    }

    /// The `frame_seq` field. Unverified until the frame is decoded.
    pub const fn frame_seq(&self) -> u64 {
        self.frame_seq
    }

    /// Total length of the frame, header to `reserved`: [`frame_len`] of `payload_len`.
    pub const fn total_len(&self) -> u64 {
        self.total_len
    }
}

/// Encodes one frame whose payload arrives in pieces, for payloads too large to hold in memory, e.g. a Segment
/// streamed to a file (SPEC §4.4).
///
/// The payload length is declared up front because it is in the header, which comes first. The caller writes
/// [`FrameEncoder::header`], then each chunk it passes to [`FrameEncoder::update`], then the bytes
/// [`FrameEncoder::finish`] returns. The CRC is computed incrementally over exactly the bytes passed to `update`,
/// so the caller must write those same bytes: a difference is only detected on read, as a checksum mismatch. The
/// payload is stored uncompressed: a compressed length is not known before compressing, so compressed frames use
/// [`encode_frame`] (segments compress their own blocks, SPEC §5.2). The output is byte-for-byte what
/// [`encode_frame`] writes with [`Compression::None`].
///
/// ```
/// use persia_format::block::Compression;
/// use persia_format::frame::{FrameEncoder, FrameKind, encode_frame};
/// use persia_format::Writer;
///
/// let mut out = Vec::new();
/// let mut enc = FrameEncoder::new(FrameKind::Segment, 9, 11)?;
/// out.extend_from_slice(enc.header());
/// for chunk in [&b"hello "[..], b"world"] {
///     enc.update(chunk)?;
///     out.extend_from_slice(chunk);
/// }
/// out.extend_from_slice(&enc.finish()?);
///
/// let mut w = Writer::new();
/// encode_frame(&mut w, FrameKind::Segment, 9, b"hello world", Compression::None)?;
/// assert_eq!(out, w.as_slice());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct FrameEncoder {
    header: FrameHeader,
    bytes: [u8; FRAME_HEADER_LEN],
    crc: Crc32c,
    written: u64,
}

impl FrameEncoder {
    /// Starts a frame of `kind` whose payload is exactly `payload_len` bytes.
    ///
    /// # Errors
    /// [`EncodeError::FrameTooLarge`] if the frame's total length would overflow `u64`.
    pub fn new(kind: FrameKind, frame_seq: u64, payload_len: u64) -> Result<Self, EncodeError> {
        Self::with_flags(kind, 0, frame_seq, payload_len)
            .ok_or(EncodeError::FrameTooLarge { payload_len })
    }

    /// The frame of `source` (kind, flags, stored payload length) renumbered to `frame_seq`: the copy
    /// [`StreamScanner::reframe`] writes. Never `None` for a header that decoded.
    fn reframing(source: &FrameHeader, frame_seq: u64) -> Option<Self> {
        Self::with_flags(source.kind, source.flags, frame_seq, source.payload_len)
    }

    /// `None` if the frame's total length overflows `u64`.
    fn with_flags(kind: FrameKind, flags: u32, frame_seq: u64, payload_len: u64) -> Option<Self> {
        let header = FrameHeader::new(0, kind, flags, payload_len, frame_seq)?;
        let bytes = header.encode();
        let mut crc = Crc32c::new();
        crc.update(bytes.get(KIND_OFFSET..).unwrap_or_default());
        Some(Self {
            header,
            bytes,
            crc,
            written: 0,
        })
    }

    /// The [`FRAME_HEADER_LEN`] header bytes, written before the payload.
    pub const fn header(&self) -> &[u8; FRAME_HEADER_LEN] {
        &self.bytes
    }

    /// Total length of the frame being encoded.
    pub const fn total_len(&self) -> u64 {
        self.header.total_len
    }

    /// Payload bytes still expected.
    pub const fn remaining(&self) -> u64 {
        self.header.payload_len.saturating_sub(self.written)
    }

    /// Adds the next payload bytes to the CRC and the count.
    ///
    /// # Errors
    /// [`EncodeError::PayloadLenMismatch`] if `chunk` would take the payload past its declared length. Nothing is
    /// added then.
    pub fn update(&mut self, chunk: &[u8]) -> Result<(), EncodeError> {
        let declared = self.header.payload_len;
        let total = self.written.checked_add(to_u64(chunk.len()));
        match total {
            Some(total) if total <= declared => {
                self.feed(chunk);
                Ok(())
            }
            _ => Err(EncodeError::PayloadLenMismatch {
                declared,
                actual: total.unwrap_or(u64::MAX),
            }),
        }
    }

    /// The bytes after the payload: zero padding to 8 bytes, the CRC and `reserved`.
    ///
    /// # Errors
    /// [`EncodeError::PayloadLenMismatch`] if fewer payload bytes were given than declared.
    pub fn finish(self) -> Result<Vec<u8>, EncodeError> {
        let declared = self.header.payload_len;
        if self.written != declared {
            return Err(EncodeError::PayloadLenMismatch {
                declared,
                actual: self.written,
            });
        }
        Ok(self.trailer())
    }

    /// [`FrameEncoder::update`] for a caller that bounds the payload itself.
    fn feed(&mut self, chunk: &[u8]) {
        self.crc.update(chunk);
        self.written = self.written.saturating_add(to_u64(chunk.len()));
    }

    /// [`FrameEncoder::finish`] for a caller that wrote exactly the declared payload.
    fn trailer(mut self) -> Vec<u8> {
        debug_assert_eq!(self.written, self.header.payload_len);
        let mut w = Writer::new();
        w.write_zeros(padding_for(self.header.payload_len, ALIGNMENT));
        self.crc.update(w.as_slice());
        w.write_u32(self.crc.finish());
        w.write_u32(0);
        w.into_inner()
    }
}

/// One valid frame, borrowing its payload from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    offset: u64,
    kind: FrameKind,
    compressed: bool,
    seq: u64,
    stored: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Reads and validates the next frame, in the order of SPEC §4.4: length and magic, total length, CRC,
    /// padding and `reserved`, then `flags`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for an invalid frame: with [`Corruption::UnexpectedEof`] if fewer than
    /// [`MIN_FRAME_LEN`] bytes remain or the frame runs past the input (at its `payload_len` field),
    /// [`Corruption::BadFrameMagic`], [`Corruption::ChecksumMismatch`] or [`Corruption::NonZeroReserved`].
    /// [`Error::Unsupported`] with [`Unsupported::FrameFlags`] for a valid frame with unknown `flags` bits. On error
    /// the position is unchanged.
    pub fn decode(r: &mut Reader<'a>) -> Result<Self> {
        let mut ahead = r.clone();
        let offset = ahead.offset();
        let available = to_u64(ahead.remaining());
        if ahead.remaining() < MIN_FRAME_LEN {
            return Err(Error::Corrupt {
                offset,
                reason: Corruption::UnexpectedEof {
                    needed: to_u64(MIN_FRAME_LEN),
                    available,
                },
            });
        }
        let len = frame_extent(ahead.rest()).map_err(|reason| Error::Corrupt {
            offset: match reason {
                Corruption::BadFrameMagic => offset,
                _ => offset.saturating_add(to_u64(PAYLOAD_LEN_OFFSET)),
            },
            reason,
        })?;
        let mut f = ahead.sub_reader(len)?;
        // `len >= MIN_FRAME_LEN`, so the trailer exists and the covered range `kind..padding` is not reversed.
        let (body, trailer) = f
            .rest()
            .split_at_checked(len.saturating_sub(FRAME_TRAILER_LEN))
            .unwrap_or_default();
        let mut trailer =
            Reader::with_base_offset(trailer, offset.saturating_add(to_u64(body.len())));
        let crc_offset = trailer.offset();
        // Checked before the fields: the CRC covers kind..padding, so a torn frame is a checksum mismatch.
        verify_crc32c(
            body.get(KIND_OFFSET..).unwrap_or_default(),
            trailer.read_u32()?,
            crc_offset,
        )?;
        f.skip(FRAME_MAGIC.len())?;
        let kind = FrameKind::from_u8(f.read_u8()?);
        let flags_offset = f.offset();
        let flags = u32::from_le_bytes(pad_flags(f.read_array::<3>()?));
        let payload_len = f.read_u64()?;
        let frame_seq = f.read_u64()?;
        // `frame_extent` checked that the padded payload fits in `len`, hence in `usize`.
        let stored = f.read_bytes(usize::try_from(payload_len).unwrap_or(usize::MAX))?;
        f.expect_zeros(padding_for(payload_len, ALIGNMENT))?;
        trailer.expect_zeros(4)?; // reserved
        let unknown = flags & !KNOWN_FRAME_FLAGS;
        if unknown != 0 {
            return Err(Error::Unsupported {
                offset: flags_offset,
                reason: Unsupported::FrameFlags { flags: unknown },
            });
        }
        *r = ahead;
        Ok(Self {
            offset,
            kind,
            compressed: flags & FLAG_ZSTD != 0,
            seq: frame_seq,
            stored,
        })
    }

    /// Absolute offset of the frame.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Total length of the frame, header to `reserved`.
    pub fn total_len(&self) -> u64 {
        // A decoded frame's length fits in the input, so it cannot overflow.
        frame_len(to_u64(self.stored.len())).unwrap_or(u64::MAX)
    }

    /// Absolute offset just past the frame: where the next one starts.
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.total_len())
    }

    /// What the frame holds. A reader MUST NOT interpret a [`FrameKind::Unknown`] frame.
    pub const fn kind(&self) -> FrameKind {
        self.kind
    }

    /// The `frame_seq` field, stored but not interpreted (ROADMAP 2.4.9).
    pub const fn frame_seq(&self) -> u64 {
        self.seq
    }

    /// Whether the payload is stored zstd-compressed (flags bit 0).
    pub const fn is_compressed(&self) -> bool {
        self.compressed
    }

    /// Appends this frame again with a new `frame_seq`: same kind, flags and stored payload (a compressed payload is
    /// copied as is, not recompressed). A container rewrite renumbers the frames it copies this way (SPEC §4.4,
    /// §4.6). Only the `frame_seq` field and the CRC differ from the original bytes.
    ///
    /// ```
    /// use persia_format::block::Compression;
    /// use persia_format::frame::{Frame, FrameKind, encode_frame};
    /// use persia_format::{Reader, Writer};
    ///
    /// let mut w = Writer::new();
    /// encode_frame(&mut w, FrameKind::Segment, 5, &[7; 500], Compression::None)?;
    /// let bytes = w.into_inner();
    /// let frame = Frame::decode(&mut Reader::new(&bytes))?;
    ///
    /// let mut w = Writer::new();
    /// frame.reframe(&mut w, 40);
    /// let copy = Frame::decode(&mut Reader::new(w.as_slice()))?;
    /// assert_eq!((copy.frame_seq(), copy.stored_payload()), (40, frame.stored_payload()));
    /// assert_eq!(copy.kind(), FrameKind::Segment);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn reframe(&self, w: &mut Writer, frame_seq: u64) {
        let flags = if self.compressed { FLAG_ZSTD } else { 0 };
        write_stored(w, self.kind, flags, frame_seq, self.stored);
    }

    /// The payload as stored, without padding: compressed if [`Frame::is_compressed`].
    pub const fn stored_payload(&self) -> &'a [u8] {
        self.stored
    }

    /// The payload, decompressed if needed. `max_len` is the largest payload the caller accepts for this kind
    /// (the decompression-bomb guard, SPEC §4.4); it applies to uncompressed payloads too, so a limit means the
    /// same whichever way the writer stored the frame.
    ///
    /// # Errors
    /// [`Error::Corrupt`] with [`Corruption::UncompressedTooLarge`] for a payload longer than `max_len`, and for a
    /// compressed payload that is shorter than its `uncompressed_len` prefix or malformed (see
    /// [`decompress_block`]); [`Error::OutOfMemory`] if the output cannot be allocated.
    pub fn payload(&self, max_len: usize) -> Result<Cow<'a, [u8]>> {
        let payload_offset = self.offset.saturating_add(to_u64(FRAME_HEADER_LEN));
        if !self.compressed {
            let len = to_u64(self.stored.len());
            if self.stored.len() > max_len {
                return Err(Error::Corrupt {
                    offset: payload_offset,
                    reason: Corruption::UncompressedTooLarge {
                        len,
                        max: to_u64(max_len),
                    },
                });
            }
            return Ok(Cow::Borrowed(self.stored));
        }
        let mut r = Reader::with_base_offset(self.stored, payload_offset);
        let uncompressed_len = r.read_u64()?;
        let mut out = Vec::new();
        decompress_block(
            Codec::Zstd,
            r.rest(),
            uncompressed_len,
            max_len,
            payload_offset,
            &mut out,
        )?;
        Ok(Cow::Owned(out))
    }
}

/// The length of the frame at the start of `rest` (at least [`MIN_FRAME_LEN`] bytes), from its magic and
/// `payload_len`.
fn frame_extent(rest: &[u8]) -> Result<usize, Corruption> {
    if rest.get(..FRAME_MAGIC.len()) != Some(&FRAME_MAGIC[..]) {
        return Err(Corruption::BadFrameMagic);
    }
    let payload_len = rest
        .get(PAYLOAD_LEN_OFFSET..)
        .and_then(|b| b.first_chunk::<8>())
        .map_or(0, |b| u64::from_le_bytes(*b));
    let available = to_u64(rest.len());
    let needed = frame_len(payload_len).unwrap_or(u64::MAX);
    usize::try_from(needed)
        .ok()
        .filter(|&len| len <= rest.len())
        .ok_or(Corruption::UnexpectedEof { needed, available })
}

/// The 24-bit `flags` field widened to 4 little-endian bytes.
const fn pad_flags([a, b, c]: [u8; 3]) -> [u8; 4] {
    [a, b, c, 0]
}

/// Why a [`FrameScanner`] stopped, and where (SPEC §4.4; ADR-0009). `offset` is the end of the valid prefix: where
/// recovery truncates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanEnd {
    /// The input ended exactly at a frame boundary.
    Clean {
        /// End of the input.
        offset: u64,
    },
    /// The frame at `offset` is invalid and nothing but zeros follows it: the usual result of a crash during an
    /// append. Recovery truncates here.
    TornTail {
        /// Start of the invalid frame.
        offset: u64,
        /// Why it is invalid.
        error: Error,
    },
    /// The frame at `offset` is invalid and non-zero bytes follow it: lost data, or an interrupted append whose
    /// pages reached the disk out of order (SPEC §4.4). Also a valid frame that breaks a checked `frame_seq` run
    /// ([`FrameScanner::expect_frame_seq`]). The bytes after `offset` must not be destroyed silently
    /// (ROADMAP 2.4.10).
    Corrupt {
        /// Start of the frame where the scan stopped: invalid, or valid but breaking the `frame_seq` run.
        offset: u64,
        /// Why it is invalid.
        error: Error,
    },
    /// The frame at `offset` is valid but uses `flags` this build does not know. Nothing may be truncated.
    Unsupported {
        /// Start of the frame.
        offset: u64,
        /// The [`Error::Unsupported`] error.
        error: Error,
    },
}

impl ScanEnd {
    /// Where the scan stopped: the end of the valid prefix.
    pub const fn offset(&self) -> u64 {
        match *self {
            Self::Clean { offset }
            | Self::TornTail { offset, .. }
            | Self::Corrupt { offset, .. }
            | Self::Unsupported { offset, .. } => offset,
        }
    }

    /// Why the frame at [`ScanEnd::offset`] was rejected; `None` for a clean end.
    pub const fn error(&self) -> Option<&Error> {
        match self {
            Self::Clean { .. } => None,
            Self::TornTail { error, .. }
            | Self::Corrupt { error, .. }
            | Self::Unsupported { error, .. } => Some(error),
        }
    }

    /// Whether the valid prefix ends before the input does: a torn tail or corruption, where recovery logically
    /// truncates (SPEC §4.4). Only a torn tail may also be truncated physically.
    pub const fn truncates(&self) -> bool {
        matches!(self, Self::TornTail { .. } | Self::Corrupt { .. })
    }

    /// Classifies a decode failure of the frame at the start of `rest` (ADR-0009).
    fn classify(rest: &[u8], offset: u64, error: Error) -> Self {
        if matches!(error, Error::Unsupported { .. }) {
            return Self::Unsupported { offset, error };
        }
        let after = if rest.len() < MIN_FRAME_LEN {
            Some(&[][..])
        } else {
            match frame_extent(rest) {
                // No known end: everything from the frame's start must be zero.
                Err(Corruption::BadFrameMagic) => Some(rest),
                Err(_) => Some(&[][..]),
                Ok(len) => rest.get(len..),
            }
        };
        if after.unwrap_or_default().iter().all(|&b| b == 0) {
            Self::TornTail { offset, error }
        } else {
            Self::Corrupt { offset, error }
        }
    }
}

/// Iterates the frames of `bytes`, stopping at the first invalid or unsupported one (SPEC §4.4).
///
/// Frames of every kind are yielded, [`FrameKind::Unknown`] and [`FrameKind::Padding`] included: consumers skip
/// what they do not interpret. Once the iterator returns `None`, [`FrameScanner::end`] says why.
///
/// ```
/// use persia_format::block::Compression;
/// use persia_format::frame::{FrameKind, FrameScanner, ScanEnd, encode_frame};
/// use persia_format::Writer;
///
/// let mut w = Writer::new();
/// encode_frame(&mut w, FrameKind::WalChunk, 1, b"put", Compression::None)?;
/// encode_frame(&mut w, FrameKind::WalChunk, 2, b"delete", Compression::None)?;
/// let mut bytes = w.into_inner();
/// bytes.truncate(bytes.len() - 3); // a crash during the second append
///
/// let mut scanner = FrameScanner::new(&bytes, 12288);
/// assert_eq!(scanner.by_ref().count(), 1);
/// assert!(matches!(scanner.end(), Some(ScanEnd::TornTail { offset: 12328, .. })));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct FrameScanner<'a> {
    r: Reader<'a>,
    end: Option<ScanEnd>,
    seq: SeqCheck,
}

/// Which `frame_seq` the next frame must have (SPEC §4.4; ADR-0013).
#[derive(Debug, Clone, Copy)]
enum SeqCheck {
    /// Not checked.
    Off,
    /// Any value in `1..u64::MAX` for the first frame, then one more each.
    Contiguous,
    /// Exactly this value, then one more each.
    Next(u64),
}

impl SeqCheck {
    /// The check for the frame after one with `found`, or why `found` breaks the run.
    fn advance(self, found: u64) -> Result<Self, Corruption> {
        match self {
            Self::Off => Ok(Self::Off),
            Self::Next(expected) if expected != found => {
                Err(Corruption::FrameSeqMismatch { expected, found })
            }
            Self::Contiguous | Self::Next(_) => match found.checked_add(1) {
                Some(next) if found != 0 => Ok(Self::Next(next)),
                _ => Err(Corruption::FrameSeqOutOfRange { found }),
            },
        }
    }
}

impl<'a> FrameScanner<'a> {
    /// A scanner over `bytes`, which start at absolute offset `base` (a frame boundary, e.g. `log_end`) and run to
    /// the end of the container. `base + bytes.len()` must fit in `u64` (true of any real file); otherwise offsets
    /// saturate and are wrong, though nothing panics.
    pub fn new(bytes: &'a [u8], base: u64) -> Self {
        Self {
            r: Reader::with_base_offset(bytes, base),
            end: None,
            seq: SeqCheck::Off,
        }
    }

    /// Also requires the first frame to have `frame_seq = first` and each later one one more (SPEC §4.4;
    /// ADR-0013), as recovery does from `log_end` with the superblock's `next_frame_seq`. A valid frame that
    /// breaks the run, or has `frame_seq` 0 or `u64::MAX` (never written), ends the scan as [`ScanEnd::Corrupt`] at that frame.
    #[must_use]
    pub fn expect_frame_seq(mut self, first: u64) -> Self {
        self.seq = SeqCheck::Next(first);
        self
    }

    /// Like [`FrameScanner::expect_frame_seq`], but the first frame may have any `frame_seq` in `1..u64::MAX`:
    /// for a range that starts at a frame whose number is not known, such as `wal_start`.
    #[must_use]
    pub fn contiguous(mut self) -> Self {
        if matches!(self.seq, SeqCheck::Off) {
            self.seq = SeqCheck::Contiguous;
        }
        self
    }

    /// The `frame_seq` the next frame must have, once a checked scan (see [`FrameScanner::expect_frame_seq`])
    /// has seen a frame or was given its first value; `None` otherwise.
    pub const fn next_frame_seq(&self) -> Option<u64> {
        match self.seq {
            SeqCheck::Next(next) => Some(next),
            SeqCheck::Off | SeqCheck::Contiguous => None,
        }
    }

    /// Absolute offset of the next frame; once stopped, [`ScanEnd::offset`].
    pub fn offset(&self) -> u64 {
        self.r.offset()
    }

    /// Why the scan stopped; `None` while it has not.
    pub const fn end(&self) -> Option<&ScanEnd> {
        self.end.as_ref()
    }
}

impl<'a> Iterator for FrameScanner<'a> {
    type Item = Frame<'a>;

    fn next(&mut self) -> Option<Frame<'a>> {
        if self.end.is_some() {
            return None;
        }
        let offset = self.r.offset();
        if self.r.is_empty() {
            self.end = Some(ScanEnd::Clean { offset });
            return None;
        }
        let before = self.r.clone();
        match Frame::decode(&mut self.r) {
            Ok(frame) => match self.seq.advance(frame.frame_seq()) {
                Ok(seq) => {
                    self.seq = seq;
                    Some(frame)
                }
                Err(reason) => {
                    // The frame is whole, so this is never a torn tail; stop before it.
                    self.r = before;
                    self.end = Some(ScanEnd::Corrupt {
                        offset,
                        error: frame_seq_error(offset, reason),
                    });
                    None
                }
            },
            Err(error) => {
                self.end = Some(ScanEnd::classify(self.r.rest(), offset, error));
                None
            }
        }
    }
}

impl FusedIterator for FrameScanner<'_> {}

/// The error for a valid frame at `offset` whose `frame_seq` breaks the run.
fn frame_seq_error(offset: u64, reason: Corruption) -> Error {
    Error::Corrupt {
        offset: offset.saturating_add(FRAME_SEQ_OFFSET),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::DEFAULT_ZSTD_LEVEL;

    const BASE: u64 = 12288;
    const ZSTD: Compression = Compression::Zstd {
        level: DEFAULT_ZSTD_LEVEL,
    };

    fn frame(kind: FrameKind, seq: u64, payload: &[u8], compression: Compression) -> Vec<u8> {
        let mut w = Writer::new();
        encode_frame(&mut w, kind, seq, payload, compression).unwrap();
        w.into_inner()
    }

    fn raw(kind: u8, seq: u64, payload: &[u8]) -> Vec<u8> {
        frame(FrameKind::from_u8(kind), seq, payload, Compression::None)
    }

    /// Rewrites the CRC of the frame at the start of `bytes` after a field was changed.
    fn fix_crc(bytes: &mut [u8]) {
        let len = frame_extent(bytes).unwrap();
        let crc = crc32c(&bytes[4..len - 8]);
        bytes[len - 8..len - 4].copy_from_slice(&crc.to_le_bytes());
    }

    fn scan(bytes: &[u8]) -> (Vec<Frame<'_>>, ScanEnd) {
        let mut scanner = FrameScanner::new(bytes, BASE);
        let frames: Vec<_> = scanner.by_ref().collect();
        assert!(scanner.next().is_none(), "fused");
        assert_eq!(scanner.offset(), scanner.end().unwrap().offset());
        (frames, scanner.end().unwrap().clone())
    }

    fn decode_err(bytes: &[u8]) -> Error {
        let mut r = Reader::with_base_offset(bytes, BASE);
        let err = Frame::decode(&mut r).unwrap_err();
        assert_eq!(r.position(), 0);
        err
    }

    #[test]
    fn golden_frame() {
        let bytes = raw(3, 0x0102_0304_0506_0708, b"hello");
        let mut expected = b"PFR1\x03\0\0\0".to_vec();
        expected.extend_from_slice(&5_u64.to_le_bytes());
        expected.extend_from_slice(&0x0102_0304_0506_0708_u64.to_le_bytes());
        expected.extend_from_slice(b"hello\0\0\0");
        let crc = crc32c(&expected[4..]);
        expected.extend_from_slice(&crc.to_le_bytes());
        expected.extend_from_slice(&[0; 4]);
        assert_eq!(bytes, expected);
        // Pins the CRC32C itself, independently of the computation above.
        assert_eq!(crc, 0xfa9b_c56d);
    }

    #[test]
    fn frame_lengths() {
        for (payload_len, len) in [(0, 32), (1, 40), (8, 40), (9, 48), (16, 48)] {
            assert_eq!(frame_len(payload_len), Some(len));
            assert_eq!(
                raw(1, 0, &vec![7; usize::try_from(payload_len).unwrap()]).len() as u64,
                len
            );
        }
        assert_eq!(frame_len(u64::MAX - 39), Some(u64::MAX - 7));
        assert_eq!(frame_len(u64::MAX - 38), None);

        assert_eq!(frame_len(u64::MAX), None);
    }

    #[test]
    fn kinds_round_trip() {
        for byte in 0..=u8::MAX {
            let kind = FrameKind::from_u8(byte);
            assert_eq!(kind.to_u8(), byte);
            assert_eq!(kind.is_known(), (1..=5).contains(&byte), "{byte}");
        }
    }

    #[test]
    fn round_trip() {
        let text = b"persia ".repeat(100);
        for (payload, compression, compressed) in [
            (&b""[..], Compression::None, false),
            (&b""[..], ZSTD, false),
            (&b"tiny"[..], ZSTD, false), // zstd does not shrink it: stored as is
            (&text[..], Compression::None, false),
            (&text[..], ZSTD, true),
        ] {
            // zstd is C behind FFI, which Miri cannot run. The uncompressed cases stay.
            if cfg!(miri) && matches!(compression, Compression::Zstd { .. }) {
                continue;
            }
            let bytes = frame(FrameKind::Segment, 9, payload, compression);
            let mut r = Reader::with_base_offset(&bytes, BASE);
            let f = Frame::decode(&mut r).unwrap();
            assert!(r.is_empty());
            assert_eq!(f.is_compressed(), compressed);
            assert_eq!((f.offset(), f.end()), (BASE, BASE + bytes.len() as u64));
            assert_eq!(f.total_len(), bytes.len() as u64);
            assert_eq!((f.kind(), f.frame_seq()), (FrameKind::Segment, 9));
            assert_eq!(&*f.payload(text.len()).unwrap(), payload);
            assert_eq!(bytes[5] & 1 == 1, compressed);
            if compressed {
                assert_eq!(f.stored_payload()[..8], (text.len() as u64).to_le_bytes());
                assert!(bytes.len() < text.len());
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI, which Miri cannot run")]
    fn bad_compression_level_appends_nothing() {
        let mut w = Writer::new();
        let err = encode_frame(
            &mut w,
            FrameKind::WalChunk,
            0,
            b"x",
            Compression::Zstd { level: 1000 },
        );
        assert!(matches!(
            err,
            Err(EncodeError::InvalidCompressionLevel { .. })
        ));
        assert!(w.is_empty());
    }

    #[test]
    fn bomb_limit() {
        let text = b"persia ".repeat(100);
        for compression in [Compression::None, ZSTD] {
            // zstd is C behind FFI, which Miri cannot run. The uncompressed case stays.
            if cfg!(miri) && matches!(compression, Compression::Zstd { .. }) {
                continue;
            }
            let bytes = frame(FrameKind::Manifest, 1, &text, compression);
            let f = Frame::decode(&mut Reader::new(&bytes)).unwrap();
            assert_eq!(f.payload(text.len()).unwrap().len(), text.len());
            let err = f.payload(text.len() - 1).unwrap_err();
            assert_eq!(
                err,
                Error::Corrupt {
                    offset: 24,
                    reason: Corruption::UncompressedTooLarge {
                        len: text.len() as u64,
                        max: text.len() as u64 - 1,
                    },
                },
                "{compression:?}"
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI, which Miri cannot run")]
    fn malformed_compressed_payload_fails_on_decode_not_scan() {
        let text = b"persia ".repeat(100);
        let mut bytes = frame(FrameKind::Manifest, 1, &text, ZSTD);
        // Claim one more uncompressed byte than the zstd frame declares.
        bytes[24] = bytes[24].wrapping_add(1);
        fix_crc(&mut bytes);
        let (frames, end) = scan(&bytes);
        assert!(matches!(end, ScanEnd::Clean { .. }));
        let err = frames[0].payload(usize::MAX).unwrap_err();
        assert!(matches!(
            err.corruption(),
            Some((o, Corruption::UncompressedLenMismatch { .. })) if o == BASE + 24
        ));
        // A compressed payload too short for its `uncompressed_len`.
        let mut short = raw(2, 0, b"abc");
        short[5] = 1;
        fix_crc(&mut short);
        let f = Frame::decode(&mut Reader::new(&short)).unwrap();
        assert!(f.payload(16).unwrap_err().is_corruption());
    }

    #[test]
    fn validation_errors_and_offsets() {
        let good = raw(1, 4, b"payload!x");
        // Too short for any frame.
        assert_eq!(
            decode_err(&good[..31]),
            Error::Corrupt {
                offset: BASE,
                reason: Corruption::UnexpectedEof {
                    needed: 32,
                    available: 31
                },
            }
        );
        // Bad magic.
        let mut b = good.clone();
        b[3] = b'2';
        assert_eq!(
            decode_err(&b),
            Error::Corrupt {
                offset: BASE,
                reason: Corruption::BadFrameMagic
            }
        );
        // Runs past the input, and a length that overflows u64.
        assert_eq!(
            decode_err(&good[..good.len() - 1]),
            Error::Corrupt {
                offset: BASE + 8,
                reason: Corruption::UnexpectedEof {
                    needed: good.len() as u64,
                    available: good.len() as u64 - 1,
                },
            }
        );
        let mut b = good.clone();
        b[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            decode_err(&b),
            Error::Corrupt { offset, reason: Corruption::UnexpectedEof { needed: u64::MAX, .. } }
                if offset == BASE + 8
        ));
        // Every bit of kind..padding is covered by the CRC; magic and reserved are checked on their own.
        let crc_at = good.len() - 8;
        for byte in 4..crc_at + 4 {
            for bit in 0..8 {
                let mut b = good.clone();
                b[byte] ^= 1 << bit;
                let err = decode_err(&b);
                if (8..16).contains(&byte) {
                    // A flipped length moves the CRC, or makes the frame run past the input.
                    assert!(
                        matches!(
                            err.corruption(),
                            Some((
                                _,
                                Corruption::UnexpectedEof { .. }
                                    | Corruption::ChecksumMismatch { .. }
                            ))
                        ),
                        "{byte}.{bit}"
                    );
                } else {
                    assert!(
                        matches!(err.corruption(), Some((o, Corruption::ChecksumMismatch { .. })) if o == BASE + crc_at as u64),
                        "byte {byte} bit {bit}: {err:?}"
                    );
                }
            }
        }
        // Non-zero padding with a matching CRC, and non-zero reserved.
        let mut b = good.clone();
        b[24 + 9] = 1;
        fix_crc(&mut b);
        assert_eq!(
            decode_err(&b),
            Error::Corrupt {
                offset: BASE + 33,
                reason: Corruption::NonZeroReserved { value: 1 }
            }
        );
        let mut b = good.clone();
        b[good.len() - 2] = 0x80;
        assert_eq!(
            decode_err(&b),
            Error::Corrupt {
                offset: BASE + good.len() as u64 - 2,
                reason: Corruption::NonZeroReserved { value: 0x80 },
            }
        );
    }

    #[test]
    fn unknown_flags_are_unsupported() {
        for (flags, unknown) in [
            (2_u32, 2_u32),
            (3, 2),
            (0x80_0000, 0x80_0000),
            (0xff_ffff, 0xff_fffe),
        ] {
            let mut b = raw(1, 0, b"x");
            b[5..8].copy_from_slice(&flags.to_le_bytes()[..3]);
            fix_crc(&mut b);
            assert_eq!(
                decode_err(&b),
                Error::Unsupported {
                    offset: BASE + 5,
                    reason: Unsupported::FrameFlags { flags: unknown }
                }
            );
            let mut log = raw(1, 0, b"a");
            log.extend_from_slice(&b);
            let (frames, end) = scan(&log);
            assert_eq!(frames.len(), 1);
            assert!(matches!(end, ScanEnd::Unsupported { offset, .. } if offset == BASE + 40));
            assert!(!end.truncates());
        }
    }

    #[test]
    fn unknown_kinds_are_skipped_not_rejected() {
        let mut log = raw(1, 1, b"a");
        log.extend(raw(0, 2, b"zero kind"));
        log.extend(raw(200, 3, b"from the future"));
        log.extend(raw(4, 4, &[0; 16]));
        log.extend(raw(3, 5, b"manifest"));
        let (frames, end) = scan(&log);
        let kinds: Vec<_> = frames.iter().map(Frame::kind).collect();
        assert_eq!(
            kinds,
            [
                FrameKind::WalChunk,
                FrameKind::from_u8(0),
                FrameKind::from_u8(200),
                FrameKind::Padding,
                FrameKind::Manifest
            ]
        );
        assert_eq!(
            end,
            ScanEnd::Clean {
                offset: BASE + log.len() as u64
            }
        );
        let known: Vec<_> = frames
            .iter()
            .filter(|f| f.kind().is_known())
            .map(Frame::frame_seq)
            .collect();
        assert_eq!(known, [1, 4, 5]);
    }

    #[test]
    fn scan_end_classes() {
        let f1 = raw(1, 1, b"first");
        let f2 = raw(1, 2, b"second frame");
        let log = [f1.clone(), f2.clone()].concat();
        let second = BASE + f1.len() as u64;
        let class = |bytes: &[u8]| {
            let (frames, end) = scan(bytes);
            (frames.len(), end)
        };

        assert_eq!(class(&[]), (0, ScanEnd::Clean { offset: BASE }));
        assert_eq!(
            class(&log),
            (
                2,
                ScanEnd::Clean {
                    offset: BASE + log.len() as u64
                }
            )
        );

        // Short file: the second frame is cut anywhere.
        for cut in f1.len() + 1..log.len() {
            let (count, end) = class(&log[..cut]);
            assert!(
                matches!(end, ScanEnd::TornTail { offset, .. } if offset == second),
                "cut {cut}: {end:?}"
            );
            assert_eq!(count, 1);
            assert!(end.truncates());
        }

        // The file grew but the data did not land: zeros, from inside the second frame or from its start.
        // (Not from inside the 4-byte magic: an aligned 4-byte field never straddles a sector. Not only `reserved`:
        // it is zero already.)
        for zero_from in
            (f1.len()..log.len() - 4).filter(|z| !(f1.len() + 1..f1.len() + 4).contains(z))
        {
            let mut zeroed = log.clone();
            zeroed[zero_from..].fill(0);
            zeroed.extend_from_slice(&[0; 64]);
            assert!(
                matches!(class(&zeroed).1, ScanEnd::TornTail { offset, .. } if offset == second),
                "{zero_from}"
            );
        }

        // Zeros after a clean end are a torn tail too (an empty magic), not a clean end.
        let zeroed = [log.clone(), vec![0; 40]].concat();
        assert!(
            matches!(class(&zeroed).1, ScanEnd::TornTail { offset, .. } if offset == BASE + log.len() as u64)
        );

        // A damaged first frame with a valid one behind it: corruption.
        let mut bad = log.clone();
        bad[30] ^= 1;
        let (count, end) = class(&bad);
        assert_eq!(count, 0);
        assert!(
            matches!(&end, ScanEnd::Corrupt { offset: BASE, error } if matches!(error.corruption(), Some((_, Corruption::ChecksumMismatch { .. }))))
        );
        assert!(end.truncates());

        // Bad magic with any non-zero byte after it: corruption, wherever that byte is.
        let mut bad = log.clone();
        bad[f1.len()] = b'X';
        bad[f1.len() + 1..].fill(0);
        assert!(matches!(class(&bad).1, ScanEnd::Corrupt { offset, .. } if offset == second));
        let mut bad = [f1.clone(), vec![0; 64]].concat();
        bad[f1.len() + 63] = 1;
        assert!(matches!(class(&bad).1, ScanEnd::Corrupt { offset, .. } if offset == second));

        // An invalid last frame followed by garbage, and one followed only by zeros.
        let mut bad = [log.clone(), vec![0; 16]].concat();
        bad[f1.len() + 30] ^= 1;
        assert!(matches!(class(&bad).1, ScanEnd::TornTail { offset, .. } if offset == second));
        bad[log.len() + 15] = 9;
        assert!(matches!(class(&bad).1, ScanEnd::Corrupt { offset, .. } if offset == second));

        // Fewer than 32 bytes left, even non-zero ones, cannot hold a frame: torn tail.
        let bad = [f1.clone(), vec![0xaa; 31]].concat();
        assert!(matches!(class(&bad).1, ScanEnd::TornTail { offset, .. } if offset == second));
    }

    /// A compressed frame written once and pinned: zstd output may change across versions, so the encoder cannot
    /// be pinned, but these bytes must decode forever (SPEC §4.4 compressed payload).
    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI, which Miri cannot run")]
    fn golden_compressed_frame_decodes() {
        const HEX: &str = "50465231030100001f0000000000000007000000000000008c00000000000000\
                           28b52ffd208c750000387065727369612001000251c508000159f1b500000000";
        let bytes: Vec<u8> = HEX
            .as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        let f = Frame::decode(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(
            (f.kind(), f.frame_seq(), f.is_compressed()),
            (FrameKind::Manifest, 7, true)
        );
        assert_eq!(f.total_len(), 64);
        assert_eq!(&*f.payload(140).unwrap(), &b"persia ".repeat(20)[..]);
    }

    /// Writeback out of order inside one large unsynced frame (a hole at its header, payload bytes behind it) is
    /// reported as corruption: the bytes cannot tell it from lost data (SPEC §4.4), so they are kept, not erased.
    #[test]
    fn hole_inside_last_frame_is_reported_as_corruption() {
        let first = raw(1, 1, b"first");
        let log = [first.clone(), raw(2, 2, &[0xab; 4096])].concat();
        let mut holed = log.clone();
        holed[first.len()..first.len() + 512].fill(0);
        let (frames, end) = scan(&holed);
        assert_eq!(frames.len(), 1);
        assert!(
            matches!(end, ScanEnd::Corrupt { offset, .. } if offset == BASE + first.len() as u64)
        );
    }

    #[test]
    fn scan_end_accessors() {
        let error = Error::Corrupt {
            offset: 1,
            reason: Corruption::BadFrameMagic,
        };
        let ends = [
            (ScanEnd::Clean { offset: 1 }, false),
            (
                ScanEnd::TornTail {
                    offset: 1,
                    error: error.clone(),
                },
                true,
            ),
            (
                ScanEnd::Corrupt {
                    offset: 1,
                    error: error.clone(),
                },
                true,
            ),
            (ScanEnd::Unsupported { offset: 1, error }, false),
        ];
        for (end, truncates) in ends {
            assert_eq!(end.offset(), 1);
            assert_eq!(end.truncates(), truncates);
            assert_eq!(end.error().is_some(), !matches!(end, ScanEnd::Clean { .. }));
        }
    }

    /// Runs a checked scan; `first` selects `expect_frame_seq(first)`, `None` selects `contiguous()`.
    fn scan_seq(bytes: &[u8], first: Option<u64>) -> (Vec<u64>, ScanEnd, Option<u64>) {
        let scanner = FrameScanner::new(bytes, BASE);
        let mut scanner = match first {
            Some(first) => scanner.expect_frame_seq(first),
            None => scanner.contiguous(),
        };
        let seqs = scanner.by_ref().map(|f| f.frame_seq()).collect();
        assert_eq!(scanner.offset(), scanner.end().unwrap().offset());
        (
            seqs,
            scanner.end().unwrap().clone(),
            scanner.next_frame_seq(),
        )
    }

    fn run(seqs: &[u64]) -> Vec<u8> {
        seqs.iter().flat_map(|&s| raw(1, s, b"wal")).collect()
    }

    #[test]
    fn frame_seq_run_is_checked_only_when_asked() {
        let bytes = run(&[5, 9, 2]);
        let (frames, end) = scan(&bytes);
        assert_eq!((frames.len(), end.offset()), (3, BASE + bytes.len() as u64));
        assert_eq!(FrameScanner::new(&bytes, BASE).next_frame_seq(), None);
        assert_eq!(
            FrameScanner::new(&[], BASE)
                .expect_frame_seq(4)
                .next_frame_seq(),
            Some(4)
        );
        assert_eq!(
            FrameScanner::new(&[], BASE).contiguous().next_frame_seq(),
            None
        );
        // `contiguous` does not override a known first value.
        let scanner = FrameScanner::new(&[], BASE)
            .expect_frame_seq(4)
            .contiguous();
        assert_eq!(scanner.next_frame_seq(), Some(4));
    }

    #[test]
    fn frame_seq_run_accepts_contiguous_frames() {
        let bytes = run(&[7, 8, 9]);
        let end = ScanEnd::Clean {
            offset: BASE + bytes.len() as u64,
        };
        assert_eq!(
            scan_seq(&bytes, Some(7)),
            (vec![7, 8, 9], end.clone(), Some(10))
        );
        assert_eq!(scan_seq(&bytes, None), (vec![7, 8, 9], end, Some(10)));
        let empty = ScanEnd::Clean { offset: BASE };
        assert_eq!(scan_seq(&[], Some(1)), (vec![], empty, Some(1)));
    }

    /// `(frame_seqs, first, frames accepted, expected, found)`.
    type SeqCase = (&'static [u64], Option<u64>, usize, u64, u64);

    #[test]
    fn frame_seq_break_is_corruption_at_the_frame() {
        let len = raw(1, 1, b"wal").len() as u64;
        let mismatch = |at: u64, expected, found| ScanEnd::Corrupt {
            offset: at,
            error: Error::Corrupt {
                offset: at + 16,
                reason: Corruption::FrameSeqMismatch { expected, found },
            },
        };
        // Wrong first value, a gap, a repeat, a step back.
        let cases: [SeqCase; 5] = [
            (&[2, 3], Some(1), 0, 1, 2),
            (&[1, 3], Some(1), 1, 2, 3),
            (&[1, 3], None, 1, 2, 3),
            (&[4, 4], None, 1, 5, 4),
            (&[4, 5, 3], Some(4), 2, 6, 3),
        ];
        for (seqs, first, valid, expected, found) in cases {
            let (got, end, _) = scan_seq(&run(seqs), first);
            assert_eq!(got, seqs[..valid], "{seqs:?}");
            let at = BASE + valid as u64 * len;
            assert_eq!(end, mismatch(at, expected, found), "{seqs:?}");
            assert!(end.truncates());
        }
    }

    #[test]
    fn frame_seq_zero_and_max_are_never_valid() {
        let out_of_range = |at: u64, found| ScanEnd::Corrupt {
            offset: at,
            error: Error::Corrupt {
                offset: at + 16,
                reason: Corruption::FrameSeqOutOfRange { found },
            },
        };
        let len = raw(1, 1, b"wal").len() as u64;
        for found in [0, u64::MAX] {
            assert_eq!(scan_seq(&run(&[found]), None).1, out_of_range(BASE, found));
            assert_eq!(
                scan_seq(&run(&[found]), Some(found)).1,
                out_of_range(BASE, found)
            );
        }
        let (seqs, end, next) = scan_seq(&run(&[u64::MAX - 1, u64::MAX]), None);
        assert_eq!(
            (seqs, end, next),
            (
                vec![u64::MAX - 1],
                out_of_range(BASE + len, u64::MAX),
                Some(u64::MAX)
            )
        );
    }

    #[test]
    fn frame_seq_run_counts_every_kind() {
        let bytes = [
            raw(1, 3, b"wal"),
            raw(4, 4, &[0; 16]),
            raw(9, 5, b"unknown kind"),
            raw(0, 6, b"kind zero"),
            raw(3, 7, b"manifest"),
        ]
        .concat();
        let (seqs, end, next) = scan_seq(&bytes, Some(3));
        assert_eq!((seqs, next), (vec![3, 4, 5, 6, 7], Some(8)));
        assert!(matches!(end, ScanEnd::Clean { .. }));
    }

    // Decoding comes first: a newer writer's frame is unsupported even if its `frame_seq` is also wrong.
    #[test]
    fn unknown_flags_win_over_a_frame_seq_break() {
        let mut b = raw(1, 9, b"x");
        b[5] = 2;
        fix_crc(&mut b);
        let log = [raw(1, 1, b"a"), b].concat();
        let (seqs, end, _) = scan_seq(&log, Some(1));
        assert_eq!(seqs, [1]);
        assert!(matches!(end, ScanEnd::Unsupported { offset, .. } if offset == BASE + 40));
    }

    #[test]
    fn invalid_frame_still_classified_in_a_checked_scan() {
        let mut bytes = run(&[1, 2]);
        let len = bytes.len();
        bytes.truncate(len - 3);
        let (seqs, end, _) = scan_seq(&bytes, Some(1));
        assert_eq!(seqs, [1]);
        assert!(matches!(end, ScanEnd::TornTail { .. }), "{end:?}");
    }

    // -----------------------------------------------------------------------------------------------------
    // FrameHeader and FrameEncoder (ROADMAP 1.7.5; SPEC §4.4 "Streaming and ranged access")
    // -----------------------------------------------------------------------------------------------------

    /// Decodes a header from `bytes` at `BASE`; on error asserts the reader did not move.
    fn header(bytes: &[u8]) -> Result<FrameHeader> {
        let mut r = Reader::with_base_offset(bytes, BASE);
        let result = FrameHeader::decode(&mut r);
        let expected_pos = if result.is_ok() { FRAME_HEADER_LEN } else { 0 };
        assert_eq!(r.position(), expected_pos, "{result:?}");
        result
    }

    /// Streams `payload` in `chunks`-sized pieces through a `FrameEncoder` and returns the frame bytes.
    fn stream(kind: FrameKind, seq: u64, payload: &[u8], chunk: usize) -> Vec<u8> {
        let mut enc = FrameEncoder::new(kind, seq, payload.len() as u64).unwrap();
        let mut out = enc.header().to_vec();
        for piece in payload.chunks(chunk.max(1)) {
            enc.update(piece).unwrap();
            out.extend_from_slice(piece);
        }
        assert_eq!(enc.remaining(), 0);
        let total = enc.total_len();
        out.extend_from_slice(&enc.finish().unwrap());
        assert_eq!(out.len() as u64, total);
        out
    }

    #[test]
    fn golden_header_decodes() {
        let bytes = raw(3, 0x0102_0304_0506_0708, b"hello");
        // The first 24 bytes alone are enough; the rest of the frame is not read.
        let h = header(&bytes[..FRAME_HEADER_LEN]).unwrap();
        assert_eq!(h.kind(), FrameKind::Manifest);
        assert_eq!(h.flags(), 0);
        assert_eq!(h.payload_len(), 5);
        assert_eq!(h.frame_seq(), 0x0102_0304_0506_0708);
        assert_eq!(h.total_len(), 40);
        assert_eq!(h.total_len(), bytes.len() as u64);
        // The same header with the whole frame behind it.
        assert_eq!(header(&bytes).unwrap(), h);

        // Hand-written bytes, independent of the encoder: unknown kind, every flags bit set, odd length.
        let mut b = b"PFR1\xc8\xff\xff\xff".to_vec();
        b.extend_from_slice(&13_u64.to_le_bytes());
        b.extend_from_slice(&u64::MAX.to_le_bytes());
        let h = header(&b).unwrap();
        assert_eq!(h.kind(), FrameKind::from_u8(200));
        assert_eq!(
            h.flags(),
            0x00ff_ffff,
            "unknown flags bits are kept, not rejected"
        );
        assert_eq!(
            (h.payload_len(), h.frame_seq(), h.total_len()),
            (13, u64::MAX, 48)
        );
    }

    #[test]
    fn header_offset_follows_the_reader_position() {
        let frame_bytes = raw(1, 2, b"abc");
        let buf = [vec![0xee; 16], frame_bytes.clone()].concat();
        let mut r = Reader::with_base_offset(&buf, BASE);
        r.skip(16).unwrap();
        let h = FrameHeader::decode(&mut r).unwrap();
        assert_eq!(r.offset(), BASE + 16 + 24);
        assert_eq!(h.total_len(), frame_bytes.len() as u64);

        // Errors are reported at the reader's absolute offset too.
        let mut bad = buf.clone();
        bad[16] = b'X';
        let mut r = Reader::with_base_offset(&bad, BASE);
        r.skip(16).unwrap();
        assert_eq!(
            FrameHeader::decode(&mut r),
            Err(Error::Corrupt {
                offset: BASE + 16,
                reason: Corruption::BadFrameMagic
            })
        );
        assert_eq!(r.position(), 16);
    }

    #[test]
    fn header_too_short_is_eof_at_its_start() {
        let bytes = raw(1, 4, b"payload!x");
        for n in 0..FRAME_HEADER_LEN {
            let eof = |needed: u64| Error::Corrupt {
                offset: BASE,
                reason: Corruption::UnexpectedEof {
                    needed,
                    available: n as u64,
                },
            };
            assert_eq!(header(&bytes[..n]), Err(eof(24)), "{n}");
            // Frame::decode reports the same place and availability, needing a whole minimal frame.
            assert_eq!(decode_err(&bytes[..n]), eof(32), "{n}");
        }
        // Exactly 24 bytes are enough for the header, not for the frame.
        assert!(header(&bytes[..24]).is_ok());
        assert_eq!(
            decode_err(&bytes[..24]).corruption(),
            Some((
                BASE,
                Corruption::UnexpectedEof {
                    needed: 32,
                    available: 24
                }
            ))
        );
    }

    #[test]
    fn header_bad_magic_matches_frame_decode() {
        let good = raw(1, 4, b"payload!x");
        for byte in 0..FRAME_MAGIC.len() {
            for bit in 0..8 {
                let mut b = good.clone();
                b[byte] ^= 1 << bit;
                let expected = Error::Corrupt {
                    offset: BASE,
                    reason: Corruption::BadFrameMagic,
                };
                assert_eq!(header(&b), Err(expected.clone()), "{byte}.{bit}");
                assert_eq!(decode_err(&b), expected, "{byte}.{bit}");
                // With only the header's 24 bytes, the magic is still checked.
                assert_eq!(header(&b[..24]), Err(expected), "{byte}.{bit}");
            }
        }
    }

    /// `check_fits(end)` agrees with `Frame::decode` on an input ending at `end`, for ends around the frame length.
    #[test]
    fn check_fits_matches_frame_decode() {
        let mut log = raw(1, 4, b"a payload of 23 bytes!!");
        let total = log.len();
        log.extend_from_slice(&raw(2, 5, b"next"));
        let h = header(&log).unwrap();
        assert_eq!((h.offset(), h.total_len()), (BASE, total as u64));
        for len in total - 8..=total + 8 {
            let end = BASE + len as u64;
            let mut r = Reader::with_base_offset(&log[..len], BASE);
            match Frame::decode(&mut r) {
                Ok(f) => {
                    assert_eq!(h.check_fits(end), Ok(()), "{len}");
                    assert_eq!(f.total_len(), h.total_len());
                }
                Err(e) => assert_eq!(h.check_fits(end), Err(e), "{len}"),
            }
        }
        // An end before the frame start fits nothing and does not underflow.
        assert!(h.check_fits(0).is_err());
    }

    #[test]
    fn header_length_overflow_is_eof_at_payload_len() {
        let good = raw(1, 4, b"payload!x");
        let with_len = |payload_len: u64| {
            let mut b = good.clone();
            b[8..16].copy_from_slice(&payload_len.to_le_bytes());
            b
        };
        // u64::MAX - 39 is the largest payload_len whose frame length fits: 24 + (u64::MAX - 39) + 8.
        for payload_len in u64::MAX - 38..=u64::MAX {
            let b = with_len(payload_len);
            let expected = Error::Corrupt {
                offset: BASE + 8,
                reason: Corruption::UnexpectedEof {
                    needed: u64::MAX,
                    available: b.len() as u64,
                },
            };
            assert_eq!(header(&b), Err(expected.clone()), "{payload_len}");
            assert_eq!(decode_err(&b), expected, "{payload_len}");
        }
        let b = with_len(u64::MAX - 39);
        let h = header(&b).unwrap();
        assert_eq!(
            (h.payload_len(), h.total_len()),
            (u64::MAX - 39, u64::MAX - 7)
        );
        // The header fits; the frame does not fit in the input.
        assert_eq!(
            decode_err(&b),
            Error::Corrupt {
                offset: BASE + 8,
                reason: Corruption::UnexpectedEof {
                    needed: u64::MAX - 7,
                    available: b.len() as u64,
                },
            }
        );
    }

    #[test]
    fn header_does_not_check_the_crc_or_the_tail() {
        // Whatever follows the header, and a wrong CRC, are Frame::decode's business.
        let mut b = raw(2, 7, b"abcdefgh");
        let crc_at = b.len() - 8;
        b[crc_at] ^= 1;
        b[30] = 0xff;
        let h = header(&b).unwrap();
        assert_eq!(
            (h.kind(), h.frame_seq(), h.payload_len()),
            (FrameKind::Segment, 7, 8)
        );
        assert!(matches!(
            decode_err(&b).corruption(),
            Some((_, Corruption::ChecksumMismatch { .. }))
        ));
    }

    #[test]
    fn encoder_output_is_the_golden_frame() {
        let golden = raw(3, 0x0102_0304_0506_0708, b"hello");
        for chunk in 1..=6 {
            assert_eq!(
                stream(FrameKind::Manifest, 0x0102_0304_0506_0708, b"hello", chunk),
                golden,
                "{chunk}"
            );
        }
        // Pins the CRC32C of the streamed frame (same value as `golden_frame`).
        assert_eq!(golden[32..36], 0xfa9b_c56d_u32.to_le_bytes());
    }

    #[test]
    fn encoder_matches_encode_frame_for_every_padding_width() {
        for len in 0..=17_u8 {
            let payload: Vec<u8> = (0..len).map(|i| i ^ 0xa5).collect();
            let expected = raw(4, u64::from(len), &payload);
            for chunk in [1, 3, 8, 64] {
                assert_eq!(
                    stream(FrameKind::Padding, u64::from(len), &payload, chunk),
                    expected,
                    "len {len} chunk {chunk}"
                );
            }
        }
    }

    #[test]
    fn encoder_header_is_what_frame_header_decodes() {
        let enc = FrameEncoder::new(FrameKind::from_u8(0), 42, 9).unwrap();
        let h = header(enc.header()).unwrap();
        assert_eq!(h.kind(), FrameKind::from_u8(0));
        assert_eq!((h.flags(), h.payload_len(), h.frame_seq()), (0, 9, 42));
        assert_eq!(h.total_len(), enc.total_len());
        assert_eq!(enc.total_len(), 48);
        assert_eq!(enc.remaining(), 9);
    }

    #[test]
    fn encoder_rejects_a_frame_too_large_for_u64() {
        for payload_len in u64::MAX - 38..=u64::MAX {
            assert_eq!(
                FrameEncoder::new(FrameKind::Segment, 0, payload_len).unwrap_err(),
                EncodeError::FrameTooLarge { payload_len },
                "{payload_len}"
            );
        }
        // The boundary: the largest frame length that fits in u64.
        let enc = FrameEncoder::new(FrameKind::Segment, 0, u64::MAX - 39).unwrap();
        assert_eq!(enc.total_len(), u64::MAX - 7);
        assert_eq!(enc.remaining(), u64::MAX - 39);
        assert_eq!(enc.header()[8..16], (u64::MAX - 39).to_le_bytes());
        assert_eq!(
            enc.finish(),
            Err(EncodeError::PayloadLenMismatch {
                declared: u64::MAX - 39,
                actual: 0
            })
        );
    }

    #[test]
    fn encoder_overrun_is_rejected_and_changes_nothing() {
        let payload = b"0123456789";
        let mut enc = FrameEncoder::new(FrameKind::Segment, 3, 10).unwrap();
        enc.update(&payload[..4]).unwrap();
        assert_eq!(enc.remaining(), 6);
        assert_eq!(
            enc.update(b"too many b"),
            Err(EncodeError::PayloadLenMismatch {
                declared: 10,
                actual: 14
            })
        );
        assert_eq!(enc.remaining(), 6);
        // One byte over is still over.
        assert_eq!(
            enc.update(b"abcdefg"),
            Err(EncodeError::PayloadLenMismatch {
                declared: 10,
                actual: 11
            })
        );
        // The rejected bytes are not in the CRC: the correct rest still gives the exact frame.
        enc.update(&payload[4..]).unwrap();
        assert_eq!(enc.remaining(), 0);
        // Nothing more fits, not even one byte; an empty chunk does.
        assert_eq!(
            enc.update(b"x"),
            Err(EncodeError::PayloadLenMismatch {
                declared: 10,
                actual: 11
            })
        );
        enc.update(b"").unwrap();
        let mut out = enc.header().to_vec();
        out.extend_from_slice(payload);
        out.extend_from_slice(&enc.finish().unwrap());
        assert_eq!(out, raw(2, 3, payload));
    }

    #[test]
    fn encoder_underrun_is_rejected_at_finish() {
        let mut enc = FrameEncoder::new(FrameKind::Segment, 3, 10).unwrap();
        assert_eq!(
            enc.clone().finish(),
            Err(EncodeError::PayloadLenMismatch {
                declared: 10,
                actual: 0
            })
        );
        enc.update(b"012345678").unwrap();
        assert_eq!(
            enc.finish(),
            Err(EncodeError::PayloadLenMismatch {
                declared: 10,
                actual: 9
            })
        );
    }

    #[test]
    fn empty_streamed_frame() {
        let enc = FrameEncoder::new(FrameKind::Manifest, 1, 0).unwrap();
        assert_eq!((enc.remaining(), enc.total_len()), (0, 32));
        let mut out = enc.header().to_vec();
        let tail = enc.finish().unwrap();
        assert_eq!(tail.len(), FRAME_TRAILER_LEN);
        out.extend_from_slice(&tail);
        assert_eq!(out, raw(3, 1, b""));
    }
}
