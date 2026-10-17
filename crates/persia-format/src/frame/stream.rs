//! Scanning frames through positioned reads with bounded memory (SPEC §4.4; ROADMAP 2.4.12).

use super::{
    FRAME_HEADER_LEN, FRAME_TRAILER_LEN, FrameHeader, KIND_OFFSET, KNOWN_FRAME_FLAGS,
    MIN_FRAME_LEN, PAYLOAD_LEN_OFFSET, ScanEnd, SeqCheck, frame_seq_error,
};
use crate::checksum::Crc32c;
use crate::error::{Corruption, Error, Unsupported, to_u64};
use crate::reader::Reader;

/// Default number of bytes a [`StreamScanner`] reads at once.
pub const DEFAULT_SCAN_CHUNK: usize = 64 * 1024;

/// A [`FrameScanner`](super::FrameScanner) over a range read piece by piece, so memory stays bounded by the chunk
/// length whatever the size of a frame (a large Segment) or of the bytes after a torn tail (SPEC §4.4).
///
/// It yields the same frames and ends with the same [`ScanEnd`], errors included, as a `FrameScanner` over the
/// bytes of `start..end`. Frames are returned as validated [`FrameHeader`]s; their payload is not kept.
///
/// ```
/// use persia_format::block::Compression;
/// use persia_format::frame::{FrameKind, ScanEnd, StreamScanner, encode_frame};
/// use persia_format::Writer;
///
/// let mut w = Writer::with_base_offset(12288);
/// encode_frame(&mut w, FrameKind::WalChunk, 1, b"put", Compression::None).map_err(|_| "encode")?;
/// let mut file = vec![0; 12288];
/// file.extend_from_slice(w.as_slice());
/// file.extend_from_slice(&[0; 40]); // a length change that reached the disk before its data
///
/// let mut read = |offset: u64, buf: &mut [u8]| -> Result<(), &str> {
///     let start = usize::try_from(offset).map_err(|_| "offset")?;
///     buf.copy_from_slice(file.get(start..start + buf.len()).ok_or("past the end")?);
///     Ok(())
/// };
/// let mut scanner = StreamScanner::new(12288, file.len() as u64).expect_frame_seq(1);
/// let frame = scanner.next_frame(&mut read)?.expect("one frame");
/// assert_eq!(frame.kind(), FrameKind::WalChunk);
/// assert!(scanner.next_frame(&mut read)?.is_none());
/// assert!(matches!(scanner.end(), Some(ScanEnd::TornTail { offset: 12328, .. })));
/// # Ok::<(), &str>(())
/// ```
#[derive(Debug, Clone)]
pub struct StreamScanner {
    offset: u64,
    limit: u64,
    seq: SeqCheck,
    end: Option<ScanEnd>,
    chunk: usize,
}

impl StreamScanner {
    /// A scanner over `start..end`: `start` is a frame boundary (e.g. `log_end`) and `end` the end of the
    /// container. An inverted range scans nothing and ends clean at `start`.
    pub fn new(start: u64, end: u64) -> Self {
        Self {
            offset: start,
            limit: end.max(start),
            seq: SeqCheck::Off,
            end: None,
            chunk: DEFAULT_SCAN_CHUNK,
        }
    }

    /// Reads at most `len` bytes at once (at least 1). The scan result does not depend on it.
    #[must_use]
    pub fn with_chunk_len(mut self, len: usize) -> Self {
        self.chunk = len.max(1);
        self
    }

    /// See [`FrameScanner::expect_frame_seq`](super::FrameScanner::expect_frame_seq).
    #[must_use]
    pub fn expect_frame_seq(mut self, first: u64) -> Self {
        self.seq = SeqCheck::Next(first);
        self
    }

    /// See [`FrameScanner::contiguous`](super::FrameScanner::contiguous).
    #[must_use]
    pub fn contiguous(mut self) -> Self {
        if matches!(self.seq, SeqCheck::Off) {
            self.seq = SeqCheck::Contiguous;
        }
        self
    }

    /// See [`FrameScanner::next_frame_seq`](super::FrameScanner::next_frame_seq).
    pub const fn next_frame_seq(&self) -> Option<u64> {
        match self.seq {
            SeqCheck::Next(next) => Some(next),
            SeqCheck::Off | SeqCheck::Contiguous => None,
        }
    }

    /// Absolute offset of the next frame; once stopped, [`ScanEnd::offset`].
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Why the scan stopped; `None` while it has not.
    pub const fn end(&self) -> Option<&ScanEnd> {
        self.end.as_ref()
    }

    /// The next valid frame, or `None` once the scan has stopped ([`StreamScanner::end`] says why). `read` fills a
    /// buffer from the absolute offset it is given; it is only asked for bytes inside `start..end`, at most the
    /// chunk length at a time (but the 24-byte frame header is read whole).
    ///
    /// # Errors
    /// An error of `read`, returned as is. The scanner is then unchanged and may be retried.
    pub fn next_frame<E>(
        &mut self,
        read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
    ) -> Result<Option<FrameHeader>, E> {
        if self.end.is_some() {
            return Ok(None);
        }
        match self.step(read)? {
            Ok((header, seq)) => {
                self.seq = seq;
                self.offset = self.offset.saturating_add(header.total_len());
                Ok(Some(header))
            }
            Err(end) => {
                self.end = Some(end);
                Ok(None)
            }
        }
    }

    /// Validates the frame at [`StreamScanner::offset`] alone, as [`Frame::decode`](super::Frame::decode) would over
    /// the bytes up to the end: no `frame_seq` check, and no look at the bytes after an invalid frame. Reads at most
    /// the frame's extent (bounded by the end). The scanner does not move.
    ///
    /// # Errors
    /// An error of `read`, returned as is. `Ok(Err(..))` is the frame's own [`Error`]: corruption, or
    /// [`Error::Unsupported`] for a valid frame with unknown flags.
    pub fn check_frame<E>(
        &self,
        read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
    ) -> Result<Result<FrameHeader, Error>, E> {
        Ok(match self.check(read)? {
            Checked::Valid(header) => Ok(header),
            Checked::Clean => Err(Error::Corrupt {
                offset: self.offset,
                reason: Corruption::UnexpectedEof {
                    needed: to_u64(MIN_FRAME_LEN),
                    available: 0,
                },
            }),
            Checked::Torn(error) | Checked::Invalid { error, .. } | Checked::Unsupported(error) => {
                Err(error)
            }
        })
    }

    /// Validates the frame at `self.offset`: `Ok(Ok((header, seq)))`, with the check for the next frame, if it is
    /// valid and continues the run; `Ok(Err(end))` if the scan stops there.
    fn step<E>(
        &self,
        read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
    ) -> Result<Result<(FrameHeader, SeqCheck), ScanEnd>, E> {
        let o = self.offset;
        let header = match self.check(read)? {
            Checked::Valid(header) => header,
            Checked::Clean => return Ok(Err(ScanEnd::Clean { offset: o })),
            Checked::Torn(error) => return Ok(Err(ScanEnd::TornTail { offset: o, error })),
            Checked::Unsupported(error) => {
                return Ok(Err(ScanEnd::Unsupported { offset: o, error }));
            }
            Checked::Invalid { error, zeros_from } => {
                return Ok(Err(self.after(read, o, zeros_from, error)?));
            }
        };
        Ok(match self.seq.advance(header.frame_seq()) {
            Ok(seq) => Ok((header, seq)),
            // The frame is whole, so this is never a torn tail; stop before it.
            Err(reason) => Err(ScanEnd::Corrupt {
                offset: o,
                error: frame_seq_error(o, reason),
            }),
        })
    }

    /// The validation part of [`StreamScanner::step`], without the zero check that classifies an invalid frame.
    fn check<E>(
        &self,
        read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
    ) -> Result<Checked, E> {
        let o = self.offset;
        let available = self.limit.saturating_sub(o);
        if available == 0 {
            return Ok(Checked::Clean);
        }
        if available < to_u64(MIN_FRAME_LEN) {
            // Fewer bytes than the smallest frame: always a torn tail, whatever they hold.
            return Ok(Checked::Torn(Error::Corrupt {
                offset: o,
                reason: Corruption::UnexpectedEof {
                    needed: to_u64(MIN_FRAME_LEN),
                    available,
                },
            }));
        }
        let mut head = [0; FRAME_HEADER_LEN];
        read(o, &mut head)?;
        let header = match FrameHeader::decode(&mut Reader::with_base_offset(&head, o)) {
            Ok(header) => header,
            Err(Error::Corrupt {
                reason: Corruption::BadFrameMagic,
                ..
            }) => {
                // No known end: everything from the frame's start must be zero.
                return Ok(Checked::Invalid {
                    error: Error::Corrupt {
                        offset: o,
                        reason: Corruption::BadFrameMagic,
                    },
                    zeros_from: o,
                });
            }
            Err(_) => {
                // The total length overflows `u64`, so the frame runs past any end: a torn tail.
                return Ok(Checked::Torn(Error::Corrupt {
                    offset: o.saturating_add(to_u64(PAYLOAD_LEN_OFFSET)),
                    reason: Corruption::UnexpectedEof {
                        needed: u64::MAX,
                        available,
                    },
                }));
            }
        };
        if let Err(error) = header.check_fits(self.limit) {
            return Ok(Checked::Torn(error));
        }
        Ok(match self.check_body(read, &head, &header)? {
            Ok(()) => Checked::Valid(header),
            Err(error @ Error::Unsupported { .. }) => Checked::Unsupported(error),
            Err(error) => Checked::Invalid {
                error,
                zeros_from: o.saturating_add(header.total_len()),
            },
        })
    }

    /// Checks the bytes after the header of a frame that fits: CRC, padding, `reserved`, then `flags`, in the order
    /// and with the errors of [`Frame::decode`](super::Frame::decode).
    fn check_body<E>(
        &self,
        read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
        head: &[u8; FRAME_HEADER_LEN],
        header: &FrameHeader,
    ) -> Result<Result<(), Error>, E> {
        let o = header.offset();
        let total = header.total_len();
        // Relative to the frame start: the CRC covers `KIND_OFFSET..crc_at`, padding is `pad_at..crc_at`.
        let crc_at = total.saturating_sub(to_u64(FRAME_TRAILER_LEN));
        let pad_at = to_u64(FRAME_HEADER_LEN).saturating_add(header.payload_len());
        let mut crc = Crc32c::new();
        crc.update(head.get(KIND_OFFSET..).unwrap_or_default());
        let mut trailer = [0; FRAME_TRAILER_LEN];
        let mut first_nonzero_pad: Option<(u64, u8)> = None;
        let mut buf = vec![0; self.chunk_len(total)];
        let mut rel = to_u64(FRAME_HEADER_LEN);
        while rel < total {
            let len = self.chunk_len(total.saturating_sub(rel));
            let chunk = buf.get_mut(..len).unwrap_or_default();
            read(o.saturating_add(rel), chunk)?;
            let chunk_end = rel.saturating_add(to_u64(len));
            if let Some(covered) = slice_of(chunk, rel, rel..crc_at.min(chunk_end)) {
                crc.update(covered);
            }
            let pad_start = pad_at.max(rel);
            if first_nonzero_pad.is_none()
                && let Some(pad) = slice_of(chunk, rel, pad_start..crc_at.min(chunk_end))
            {
                first_nonzero_pad = pad
                    .iter()
                    .position(|&b| b != 0)
                    .and_then(|i| Some((pad_start.saturating_add(to_u64(i)), *pad.get(i)?)));
            }
            if let Some(tail) = slice_of(chunk, rel, crc_at.max(rel)..chunk_end) {
                let at = usize::try_from(crc_at.max(rel).saturating_sub(crc_at)).unwrap_or(0);
                if let Some(dst) = trailer.get_mut(at..at.saturating_add(tail.len())) {
                    dst.copy_from_slice(tail);
                }
            }
            rel = chunk_end;
        }
        let [c0, c1, c2, c3, r0, r1, r2, r3] = trailer;
        if let Err(err) = crc.verify(
            u32::from_le_bytes([c0, c1, c2, c3]),
            o.saturating_add(crc_at),
        ) {
            return Ok(Err(err));
        }
        if let Some((at, value)) = first_nonzero_pad {
            return Ok(Err(Error::Corrupt {
                offset: o.saturating_add(at),
                reason: Corruption::NonZeroReserved { value },
            }));
        }
        let reserved_at = o.saturating_add(crc_at).saturating_add(4);
        if let Some((i, &value)) = [r0, r1, r2, r3].iter().enumerate().find(|&(_, &b)| b != 0) {
            return Ok(Err(Error::Corrupt {
                offset: reserved_at.saturating_add(to_u64(i)),
                reason: Corruption::NonZeroReserved { value },
            }));
        }
        let unknown = header.flags() & !KNOWN_FRAME_FLAGS;
        if unknown != 0 {
            return Ok(Err(Error::Unsupported {
                offset: o.saturating_add(to_u64(KIND_OFFSET)).saturating_add(1),
                reason: Unsupported::FrameFlags { flags: unknown },
            }));
        }
        Ok(Ok(()))
    }

    /// How a scan that rejected the frame at `o` with `error` ends: a torn tail if only zeros follow `from`,
    /// corruption otherwise (SPEC §4.4).
    fn after<E>(
        &self,
        read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
        o: u64,
        from: u64,
        error: Error,
    ) -> Result<ScanEnd, E> {
        if self.all_zero(read, from)? {
            Ok(ScanEnd::TornTail { offset: o, error })
        } else {
            Ok(ScanEnd::Corrupt { offset: o, error })
        }
    }

    /// Whether every byte of `from..end` is zero.
    fn all_zero<E>(
        &self,
        read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
        from: u64,
    ) -> Result<bool, E> {
        let mut at = from;
        let mut buf = vec![0; self.chunk_len(self.limit.saturating_sub(at))];
        while at < self.limit {
            let len = self.chunk_len(self.limit.saturating_sub(at));
            let chunk = buf.get_mut(..len).unwrap_or_default();
            read(at, chunk)?;
            if chunk.iter().any(|&b| b != 0) {
                return Ok(false);
            }
            at = at.saturating_add(to_u64(len));
        }
        Ok(true)
    }

    /// The next read's length when `remaining` bytes are left: at least 1 if any are.
    fn chunk_len(&self, remaining: u64) -> usize {
        usize::try_from(remaining).map_or(self.chunk, |r| r.min(self.chunk))
    }
}

/// The part of `chunk`, which starts at relative position `chunk_start`, that lies in `range`; `None` if empty.
fn slice_of(chunk: &[u8], chunk_start: u64, range: std::ops::Range<u64>) -> Option<&[u8]> {
    if range.start >= range.end {
        return None;
    }
    let from = usize::try_from(range.start.checked_sub(chunk_start)?).ok()?;
    let to = usize::try_from(range.end.checked_sub(chunk_start)?).ok()?;
    chunk.get(from..to)
}

/// What [`StreamScanner::check`] found at the scan offset.
enum Checked {
    /// No bytes left.
    Clean,
    Valid(FrameHeader),
    /// Invalid, and a torn tail whatever follows (short, or running past the end).
    Torn(Error),
    /// Invalid: a torn tail if only zeros follow `zeros_from`, corruption otherwise.
    Invalid {
        error: Error,
        zeros_from: u64,
    },
    /// Valid, with unknown flags.
    Unsupported(Error),
}
