//! Frames: the checksummed records that follow the superblock slots in a container (SPEC §4.4; ADR-0009).
//!
//! [`encode_frame`] appends one frame, optionally with a zstd-compressed payload. [`Frame::decode`] reads one frame
//! strictly, e.g. the Manifest frame a superblock points at. [`FrameScanner`] reads frames until the first invalid
//! or unsupported one and reports why it stopped ([`ScanEnd`]): a clean end, a torn tail, corruption, or a frame
//! from a newer writer. Frames of unknown kinds are yielded as [`FrameKind::Unknown`] and must be skipped.
//!
//! Scanning checks framing only. A compressed payload is decoded on demand by [`Frame::payload`], under a
//! caller-supplied bomb limit.

use std::borrow::Cow;
use std::iter::FusedIterator;

use crate::ALIGNMENT;
use crate::align::padding_for;
use crate::block::Compression;
use crate::checksum::{crc32c, verify_crc32c};
use crate::compress::{Codec, compress_block, decompress_block};
use crate::error::{Corruption, EncodeError, Error, Result, Unsupported, to_u64};
use crate::reader::Reader;
use crate::writer::Writer;

/// The first 4 bytes of every frame.
pub const FRAME_MAGIC: [u8; 4] = *b"PFR1";
/// Bytes before the payload: magic, kind, flags, `payload_len`, `frame_seq`.
pub const FRAME_HEADER_LEN: usize = 24;
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
    Ok(())
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
    /// pages reached the disk out of order (SPEC §4.4). The bytes after `offset` must not be destroyed silently
    /// (ROADMAP 2.4.10).
    Corrupt {
        /// Start of the invalid frame.
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
}

impl<'a> FrameScanner<'a> {
    /// A scanner over `bytes`, which start at absolute offset `base` (a frame boundary, e.g. `log_end`) and run to
    /// the end of the container. `base + bytes.len()` must fit in `u64` (true of any real file); otherwise offsets
    /// saturate and are wrong, though nothing panics.
    pub fn new(bytes: &'a [u8], base: u64) -> Self {
        Self {
            r: Reader::with_base_offset(bytes, base),
            end: None,
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
        match Frame::decode(&mut self.r) {
            Ok(frame) => Some(frame),
            Err(error) => {
                self.end = Some(ScanEnd::classify(self.r.rest(), offset, error));
                None
            }
        }
    }
}

impl FusedIterator for FrameScanner<'_> {}

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
}
