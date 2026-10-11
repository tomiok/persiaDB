//! Block framing of segment sections (SPEC §5.2; ADR-0006).
//!
//! A block-framed section stores a logical byte stream cut into fixed-size blocks, each with a 16-byte
//! [`BlockHeader`], followed by a block table that locates every block. [`BlockWriter`] produces a section;
//! [`BlockReader`] validates its table once and then decodes any block, or any stream range, on its own, so
//! a block needs only its own bytes once the table is known. [`BlockTable`] opens from the section's tail alone
//! and decodes blocks fetched separately (object storage); [`BlockReader`] wraps it for a section held in
//! memory or mapped.
//!
//! Everything read is hostile (SPEC §17): the table's `block_size` (at most [`MAX_BLOCK_SIZE`]) bounds every
//! decode, every block's CRC is verified before its header is interpreted, and errors in block `i` are
//! [`Error::CorruptBlock`] with `index: i`.

use std::ops::Range;

use crate::checksum::{Crc32c, check, crc32c};
use crate::compress::{
    Codec, DEFAULT_ZSTD_LEVEL, check_zstd_level, compress_block, decompress_block,
};
use crate::error::{Corruption, EncodeError, Error, Result, to_u64};
use crate::reader::Reader;
use crate::writer::Writer;

/// Size of a [`BlockHeader`] on disk.
pub const BLOCK_HEADER_LEN: usize = 16;
/// Size of the block table's fixed trailer (`total_len`, `block_size`, `block_count`, reserved, CRC).
pub const TABLE_TRAILER_LEN: usize = 24;
/// Block size used when the caller has no preference (SPEC §5.2).
pub const DEFAULT_BLOCK_SIZE: u32 = 16 * 1024;
/// The largest valid `block_size`: the most a reader allocates for one block, whatever a table claims.
pub const MAX_BLOCK_SIZE: u32 = 1 << 20;

/// Width of a `block_end` table entry.
const END_LEN: usize = 8;

/// The 16-byte header in front of every block's payload (SPEC §5.2).
///
/// [`BlockHeader::decode`] checks the fields but not the CRC, which covers the payload too: verify the raw
/// bytes first ([`BlockReader`] does).
///
/// ```
/// use persia_format::block::BlockHeader;
/// use persia_format::compress::Codec;
/// use persia_format::{Reader, Writer};
///
/// let header = BlockHeader::new(Codec::None, b"abc")?;
/// let mut w = Writer::new();
/// header.encode(&mut w);
/// assert_eq!(w.len(), 16);
/// assert_eq!(BlockHeader::decode(&mut Reader::new(w.as_slice()))?, header);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    /// CRC32C over the rest of the header and the payload.
    pub crc32c: u32,
    /// How the payload is stored.
    pub codec: Codec,
    /// Length of the payload once decoded.
    pub uncompressed_len: u32,
    /// Length of the payload as stored.
    pub stored_len: u32,
}

impl BlockHeader {
    /// The header for `payload` stored with `codec`, which decodes to `uncompressed_len` bytes.
    ///
    /// # Errors
    /// [`EncodeError::InvalidLen`] if the payload is longer than `u32::MAX`.
    pub fn with_uncompressed_len(
        codec: Codec,
        uncompressed_len: u32,
        payload: &[u8],
    ) -> Result<Self, EncodeError> {
        let stored_len = u32::try_from(payload.len()).map_err(|_| EncodeError::InvalidLen {
            len: payload.len(),
            min: 0,
            max: u32::MAX as usize,
        })?;
        let mut header = Self {
            crc32c: 0,
            codec,
            uncompressed_len,
            stored_len,
        };
        let mut crc = Crc32c::new();
        crc.update(&header.checked_fields());
        crc.update(payload);
        header.crc32c = crc.finish();
        Ok(header)
    }

    /// The header for an uncompressed payload (`uncompressed_len = stored_len`), mostly for tests.
    ///
    /// # Errors
    /// As [`BlockHeader::with_uncompressed_len`].
    pub fn new(codec: Codec, payload: &[u8]) -> Result<Self, EncodeError> {
        let len = u32::try_from(payload.len()).map_err(|_| EncodeError::InvalidLen {
            len: payload.len(),
            min: 0,
            max: u32::MAX as usize,
        })?;
        Self::with_uncompressed_len(codec, len, payload)
    }

    /// Bytes 4..16, the part of the header the CRC covers.
    fn checked_fields(&self) -> [u8; 12] {
        let [u0, u1, u2, u3] = self.uncompressed_len.to_le_bytes();
        let [s0, s1, s2, s3] = self.stored_len.to_le_bytes();
        [self.codec.to_u8(), 0, 0, 0, u0, u1, u2, u3, s0, s1, s2, s3]
    }

    /// Appends the 16 header bytes.
    pub fn encode(&self, w: &mut Writer) {
        w.write_u32(self.crc32c);
        w.write_bytes(&self.checked_fields());
    }

    /// Reads 16 header bytes. Does not verify the CRC (see the type docs).
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a short input, an unknown codec or non-zero reserved bytes. On error the
    /// position is unchanged.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let mut ahead = r.clone();
        let crc32c = ahead.read_u32()?;
        let codec_offset = ahead.offset();
        let codec = Codec::from_u8(ahead.read_u8()?, codec_offset)?;
        ahead.expect_zeros(3)?;
        let uncompressed_len = ahead.read_u32()?;
        let stored_len = ahead.read_u32()?;
        *r = ahead;
        Ok(Self {
            crc32c,
            codec,
            uncompressed_len,
            stored_len,
        })
    }
}

/// How a [`BlockWriter`] stores its blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// Every block as [`Codec::None`].
    None,
    /// zstd at `level`; a block that zstd does not shrink is stored as [`Codec::None`].
    Zstd {
        /// The zstd level (not recorded on disk).
        level: i32,
    },
}

impl Default for Compression {
    fn default() -> Self {
        Self::Zstd {
            level: DEFAULT_ZSTD_LEVEL,
        }
    }
}

/// Builds a block-framed section from a byte stream written in pieces of any size (SPEC §5.2).
///
/// It buffers at most one block of input, plus the encoded section until the caller takes it: call
/// [`BlockWriter::take_output`] to drain the bytes encoded so far (e.g. to stream them to storage), and
/// [`BlockWriter::finish`] for the rest, block table included. The concatenation is the section.
///
/// ```
/// use persia_format::block::{BlockReader, BlockWriter, Compression};
///
/// let mut w = BlockWriter::new(4, Compression::None)?;
/// w.write(b"hello ")?;
/// w.write(b"world")?;
/// let section = w.finish()?;
///
/// let r = BlockReader::open(&section, 0)?;
/// assert_eq!((r.block_count(), r.len()), (3, 11));
/// let mut out = Vec::new();
/// r.read_block(1, &mut out)?;
/// assert_eq!(out, b"o wo");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct BlockWriter {
    block_size: u32,
    compression: Compression,
    out: Writer,
    /// Input not yet in a block; shorter than `block_size` between calls.
    pending: Vec<u8>,
    ends: Vec<u64>,
    /// Stream bytes already in blocks.
    flushed_len: u64,
    scratch: Vec<u8>,
    /// Set by a failed `write` or `finish`: the stream lost bytes, so every later call fails.
    failed: Option<EncodeError>,
}

impl BlockWriter {
    /// A writer that cuts blocks of `block_size` uncompressed bytes.
    ///
    /// # Errors
    /// [`EncodeError::InvalidBlockSize`] outside `1..=`[`MAX_BLOCK_SIZE`];
    /// [`EncodeError::InvalidCompressionLevel`] for a zstd level zstd does not accept.
    pub fn new(block_size: u32, compression: Compression) -> Result<Self, EncodeError> {
        if !(1..=MAX_BLOCK_SIZE).contains(&block_size) {
            return Err(EncodeError::InvalidBlockSize {
                size: block_size,
                max: MAX_BLOCK_SIZE,
            });
        }
        if let Compression::Zstd { level } = compression {
            check_zstd_level(level)?;
        }
        Ok(Self {
            block_size,
            compression,
            out: Writer::new(),
            pending: Vec::new(),
            ends: Vec::new(),
            flushed_len: 0,
            scratch: Vec::new(),
            failed: None,
        })
    }

    /// Stream bytes written so far: the stream position of the next byte (e.g. a `DocStore` row's offset).
    pub fn position(&self) -> u64 {
        self.flushed_len.saturating_add(to_u64(self.pending.len()))
    }

    /// Appends `bytes` to the stream, emitting every block that fills up.
    ///
    /// # Errors
    /// [`EncodeError::TooManyBlocks`] past `u32::MAX` blocks; [`EncodeError::CompressionFailed`] if zstd runs
    /// out of memory. An error poisons the writer: an unknown prefix of `bytes` was taken, so every later
    /// `write` or `finish` returns the same error rather than a section with bytes missing.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), EncodeError> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let result = self.write_blocks(bytes);
        if let Err(e) = result {
            self.failed = Some(e);
        }
        result
    }

    fn write_blocks(&mut self, mut bytes: &[u8]) -> Result<(), EncodeError> {
        let block_size = self.block_len();
        while !bytes.is_empty() {
            // Whole blocks straight from the input, without copying them into `pending`.
            if self.pending.is_empty() && bytes.len() >= block_size {
                let (block, rest) = bytes.split_at(block_size);
                self.emit(block)?;
                bytes = rest;
                continue;
            }
            let room = block_size.saturating_sub(self.pending.len());
            let (head, rest) = bytes.split_at(room.min(bytes.len()));
            self.pending.extend_from_slice(head);
            bytes = rest;
            if self.pending.len() == block_size {
                self.flush_pending()?;
            }
        }
        Ok(())
    }

    /// Removes and returns the section bytes encoded so far (whole blocks only). Later output continues
    /// where this ends, so the caller can stream a large section instead of holding it in memory.
    pub fn take_output(&mut self) -> Vec<u8> {
        let next = Writer::with_base_offset(self.out.offset());
        std::mem::replace(&mut self.out, next).into_inner()
    }

    /// Emits the last, partial block and the block table, and returns the section bytes not yet taken by
    /// [`BlockWriter::take_output`].
    ///
    /// # Errors
    /// As [`BlockWriter::write`], including an earlier `write` error.
    pub fn finish(mut self) -> Result<Vec<u8>, EncodeError> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if !self.pending.is_empty() {
            self.flush_pending()?;
        }
        let block_count = u32::try_from(self.ends.len()).map_err(|_| EncodeError::TooManyBlocks)?;
        let mut table = Writer::new();
        for &end in &self.ends {
            table.write_u64(end);
        }
        table.write_u64(self.flushed_len);
        table.write_u32(self.block_size);
        table.write_u32(block_count);
        table.write_u32(0);
        let crc = crc32c(table.as_slice());
        table.write_u32(crc);
        self.out.write_bytes(table.as_slice());
        Ok(self.out.into_inner())
    }

    fn block_len(&self) -> usize {
        usize::try_from(self.block_size).unwrap_or(usize::MAX)
    }

    fn flush_pending(&mut self) -> Result<(), EncodeError> {
        let pending = std::mem::take(&mut self.pending);
        let result = self.emit(&pending);
        self.pending = pending;
        self.pending.clear();
        result
    }

    /// Appends one block holding `src` (at most `block_size` bytes).
    fn emit(&mut self, src: &[u8]) -> Result<(), EncodeError> {
        if self.ends.len() >= u32::MAX as usize {
            return Err(EncodeError::TooManyBlocks);
        }
        let uncompressed_len = u32::try_from(src.len()).map_err(|_| EncodeError::InvalidLen {
            len: src.len(),
            min: 1,
            max: self.block_len(),
        })?;
        let (codec, payload) = match self.compression {
            Compression::None => (Codec::None, src),
            Compression::Zstd { level } => {
                self.scratch.clear();
                compress_block(Codec::Zstd, level, src, &mut self.scratch)?;
                if self.scratch.len() < src.len() {
                    (Codec::Zstd, self.scratch.as_slice())
                } else {
                    (Codec::None, src)
                }
            }
        };
        let header = BlockHeader::with_uncompressed_len(codec, uncompressed_len, payload)?;
        header.encode(&mut self.out);
        self.out.write_bytes(payload);
        self.ends.push(self.out.offset());
        self.flushed_len = self.flushed_len.saturating_add(u64::from(uncompressed_len));
        Ok(())
    }
}

/// A validated block table: everything needed to locate and decode any block of a block-framed section
/// without holding the section (SPEC §5.2; ADR-0006).
///
/// Over object storage: fetch the section's last [`TABLE_TRAILER_LEN`] bytes (or more, speculatively), ask
/// [`BlockTable::required_len`] how many tail bytes the table needs, fetch them if needed, then
/// [`BlockTable::open`]. Each block is then one range request for [`BlockTable::block_range`], decoded with
/// [`BlockTable::decode_block`].
///
/// ```
/// use persia_format::block::{BlockTable, BlockWriter, Compression, TABLE_TRAILER_LEN};
///
/// let mut w = BlockWriter::new(4, Compression::None)?;
/// w.write(b"hello world")?;
/// let section = w.finish()?;
/// let section_len = section.len() as u64;
///
/// // Only the tail and one block are "fetched".
/// let trailer = section[section.len() - TABLE_TRAILER_LEN..].try_into()?;
/// let needed = BlockTable::required_len(trailer) as usize;
/// let table = BlockTable::open(&section[section.len() - needed..], section_len, 0)?;
/// let range = table.block_range(2)?;
/// let mut out = Vec::new();
/// table.decode_block(2, &section[range.start as usize..range.end as usize], &mut out)?;
/// assert_eq!(out, b"rld");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy)]
pub struct BlockTable<'a> {
    /// The table's `block_end` entries, already validated.
    ends: &'a [u8],
    /// Absolute offset of the section start.
    base: u64,
    total_len: u64,
    block_size: u32,
    block_count: u32,
}

impl<'a> BlockTable<'a> {
    /// How many bytes at the end of the section the block table occupies (`8·block_count + 24`), read from
    /// the trailer (the section's last [`TABLE_TRAILER_LEN`] bytes). Not verified: [`BlockTable::open`] checks
    /// it against the table CRC.
    pub fn required_len(trailer: &[u8; TABLE_TRAILER_LEN]) -> u64 {
        let [.., c0, c1, c2, c3, _, _, _, _, _, _, _, _] = *trailer;
        let block_count = u32::from_le_bytes([c0, c1, c2, c3]);
        u64::from(block_count)
            .saturating_mul(to_u64(END_LEN))
            .saturating_add(to_u64(TABLE_TRAILER_LEN))
    }

    /// Opens the block table from `tail`, the last bytes of a block-framed section of `section_len` bytes
    /// that starts at absolute offset `base` (for error reports). `tail` may be the whole section.
    ///
    /// # Errors
    /// - [`Error::Corrupt`] if the section is too short for its table, the table fails its CRC, or it breaks a
    ///   rule of SPEC §5.2.
    /// - [`Error::OutOfBounds`] if `tail` is longer than `section_len`, or shorter than the table: then
    ///   `index` is the number of tail bytes needed; fetch them and retry.
    pub fn open(tail: &'a [u8], section_len: u64, base: u64) -> Result<Self> {
        let tail_len = to_u64(tail.len());
        if tail_len > section_len {
            return Err(Error::OutOfBounds {
                index: tail_len,
                len: section_len,
            });
        }
        let eof = |needed: u64| Error::Corrupt {
            offset: base,
            reason: Corruption::UnexpectedEof {
                needed,
                available: section_len,
            },
        };
        let too_short = |needed: u64| Error::OutOfBounds {
            index: needed,
            len: tail_len,
        };
        let trailer_len = to_u64(TABLE_TRAILER_LEN);
        if section_len < trailer_len {
            return Err(eof(trailer_len));
        }
        let trailer: &[u8; TABLE_TRAILER_LEN] =
            tail.last_chunk().ok_or_else(|| too_short(trailer_len))?;
        // Only `block_count` is used before the CRC check, to find where the table starts.
        let table_len = Self::required_len(trailer);
        let table_start = section_len
            .checked_sub(table_len)
            .ok_or_else(|| eof(table_len))?;
        let table = usize::try_from(table_len)
            .ok()
            .and_then(|len| tail.len().checked_sub(len))
            .and_then(|start| tail.get(start..))
            .ok_or_else(|| too_short(table_len))?;
        // Absolute offset of a position within the table.
        let at = |pos: usize| base.saturating_add(table_start).saturating_add(to_u64(pos));

        let mut r = Reader::with_base_offset(table, at(0));
        let ends_len = table.len().saturating_sub(TABLE_TRAILER_LEN);
        let ends = r.read_bytes(ends_len)?;
        let total_len = r.read_u64()?;
        let size_offset = r.offset();
        let block_size = r.read_u32()?;
        let count_offset = r.offset();
        let block_count = r.read_u32()?;
        let reserved = r.clone();
        r.skip(4)?;
        let crc_offset = r.offset();
        let stored_crc = r.read_u32()?;
        let covered = table
            .get(..table.len().saturating_sub(4))
            .unwrap_or_default();
        check(crc32c(covered), stored_crc, crc_offset)?;

        reserved.clone().expect_zeros(4)?;
        if !(1..=MAX_BLOCK_SIZE).contains(&block_size) {
            return Err(Error::Corrupt {
                offset: size_offset,
                reason: Corruption::InvalidBlockSize {
                    size: block_size,
                    max: MAX_BLOCK_SIZE,
                },
            });
        }
        let expected = total_len.div_ceil(u64::from(block_size));
        if expected != u64::from(block_count) {
            return Err(Error::Corrupt {
                offset: count_offset,
                reason: Corruption::BlockCountMismatch {
                    count: block_count,
                    expected,
                },
            });
        }
        validate_ends(ends, block_count, block_size, base, table_start)?;

        Ok(Self {
            ends,
            base,
            total_len,
            block_size,
            block_count,
        })
    }

    /// Number of blocks.
    pub const fn block_count(&self) -> u32 {
        self.block_count
    }

    /// Uncompressed size of every block but the last.
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Length of the stream (all blocks, uncompressed).
    pub const fn len(&self) -> u64 {
        self.total_len
    }

    /// Whether the stream is empty (then there are no blocks).
    pub const fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    /// Where block `index` is stored, relative to the section start (e.g. for a range request). At most
    /// `16 + block_size` bytes long.
    ///
    /// # Errors
    /// [`Error::OutOfBounds`] if `index >= block_count`.
    pub fn block_range(&self, index: u32) -> Result<Range<u64>> {
        let out_of_bounds = Error::OutOfBounds {
            index: u64::from(index),
            len: u64::from(self.block_count),
        };
        let i = usize::try_from(index).map_err(|_| out_of_bounds.clone())?;
        let end = self.end(i).ok_or(out_of_bounds)?;
        let start = match i.checked_sub(1) {
            Some(prev) => self.end(prev).unwrap_or(0),
            None => 0,
        };
        Ok(start..end)
    }

    /// Stream position where block `index` starts; the block holds the next [`BlockTable::block_size`] bytes
    /// (fewer for the last). Pure arithmetic: unlike [`BlockTable::block_range`], it does not check `index`.
    pub fn block_start(&self, index: u32) -> u64 {
        u64::from(index).saturating_mul(u64::from(self.block_size))
    }

    /// Decodes block `index` from `block`, its stored bytes (exactly [`BlockTable::block_range`]), and appends
    /// the decoded bytes to `out`.
    ///
    /// # Errors
    /// [`Error::OutOfBounds`] if `index >= block_count`, or if `block` is not exactly the block's length (then
    /// `index` is `block.len()` and `len` the expected length); [`Error::CorruptBlock`] with this `index` if
    /// the block fails its CRC, its header disagrees with the table, or its payload does not decode;
    /// [`Error::OutOfMemory`] as for [`decompress_block`]. On error `out` is left as it was.
    pub fn decode_block(&self, index: u32, block: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let range = self.block_range(index)?;
        let expected_len = range.end.saturating_sub(range.start);
        if to_u64(block.len()) != expected_len {
            return Err(Error::OutOfBounds {
                index: to_u64(block.len()),
                len: expected_len,
            });
        }
        let block_err = |offset: u64, reason: Corruption| Error::CorruptBlock {
            index,
            offset,
            reason,
        };
        let base = self.base.saturating_add(range.start);

        // The table guarantees at least a header; a shorter block is still an error, never a vacuous CRC match.
        let (stored_crc, checked) = block.split_first_chunk::<4>().ok_or_else(|| {
            block_err(
                base,
                Corruption::UnexpectedEof {
                    needed: to_u64(BLOCK_HEADER_LEN),
                    available: to_u64(block.len()),
                },
            )
        })?;
        check(crc32c(checked), u32::from_le_bytes(*stored_crc), base)
            .map_err(|e| in_block(index, e))?;
        let mut r = Reader::with_base_offset(block, base);
        let header = BlockHeader::decode(&mut r).map_err(|e| in_block(index, e))?;
        let payload = r.rest();
        if u64::from(header.stored_len) != to_u64(payload.len()) {
            return Err(block_err(
                base.saturating_add(12),
                Corruption::BlockLenMismatch {
                    header: u64::from(header.stored_len),
                    table: to_u64(payload.len()),
                },
            ));
        }
        let expected = self.uncompressed_len_of(index);
        if u64::from(header.uncompressed_len) != expected {
            return Err(block_err(
                base.saturating_add(8),
                Corruption::BlockLenMismatch {
                    header: u64::from(header.uncompressed_len),
                    table: expected,
                },
            ));
        }
        let max_len = usize::try_from(self.block_size).unwrap_or(usize::MAX);
        decompress_block(
            header.codec,
            payload,
            u64::from(header.uncompressed_len),
            max_len,
            base.saturating_add(to_u64(BLOCK_HEADER_LEN)),
            out,
        )
        .map_err(|e| in_block(index, e))
    }

    /// End of block `i` relative to the section start.
    fn end(&self, i: usize) -> Option<u64> {
        let at = i.checked_mul(END_LEN)?;
        let entry = self.ends.get(at..at.checked_add(END_LEN)?)?;
        Some(u64::from_le_bytes(entry.try_into().ok()?))
    }

    /// Uncompressed length of block `index` per SPEC §5.2: full blocks, then the remainder.
    fn uncompressed_len_of(&self, index: u32) -> u64 {
        let block_size = u64::from(self.block_size);
        self.total_len
            .saturating_sub(self.block_start(index))
            .min(block_size)
    }
}

/// Random access to the blocks of a block-framed section held in memory or mapped (SPEC §5.2): a
/// [`BlockTable`] plus the section's bytes.
///
/// [`BlockReader::open`] validates the whole block table; each block is checked when it is decoded.
#[derive(Debug, Clone, Copy)]
pub struct BlockReader<'a> {
    table: BlockTable<'a>,
    /// The whole section.
    section: &'a [u8],
}

impl<'a> BlockReader<'a> {
    /// Opens the block-framed `section`, which starts at absolute offset `base` (for error reports).
    ///
    /// # Errors
    /// [`Error::Corrupt`] if the block table is short, fails its CRC, or breaks a rule of SPEC §5.2.
    pub fn open(section: &'a [u8], base: u64) -> Result<Self> {
        let table = BlockTable::open(section, to_u64(section.len()), base)?;
        Ok(Self { table, section })
    }

    /// The validated block table.
    pub const fn table(&self) -> &BlockTable<'a> {
        &self.table
    }

    /// Number of blocks.
    pub const fn block_count(&self) -> u32 {
        self.table.block_count
    }

    /// Uncompressed size of every block but the last.
    pub const fn block_size(&self) -> u32 {
        self.table.block_size
    }

    /// Length of the stream (all blocks, uncompressed).
    pub const fn len(&self) -> u64 {
        self.table.total_len
    }

    /// Whether the stream is empty (then there are no blocks).
    pub const fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// As [`BlockTable::block_range`].
    ///
    /// # Errors
    /// [`Error::OutOfBounds`] if `index >= block_count`.
    pub fn block_range(&self, index: u32) -> Result<Range<u64>> {
        self.table.block_range(index)
    }

    /// As [`BlockTable::block_start`].
    pub fn block_start(&self, index: u32) -> u64 {
        self.table.block_start(index)
    }

    /// Decodes block `index` and appends its bytes to `out`.
    ///
    /// # Errors
    /// [`Error::OutOfBounds`] if `index >= block_count`; otherwise as [`BlockTable::decode_block`]. On error
    /// `out` is left as it was.
    pub fn read_block(&self, index: u32, out: &mut Vec<u8>) -> Result<()> {
        let range = self.table.block_range(index)?;
        // In-section ranges were validated by `open`.
        let start = usize::try_from(range.start).unwrap_or(usize::MAX);
        let end = usize::try_from(range.end).unwrap_or(usize::MAX);
        let block = self.section.get(start..end).unwrap_or_default();
        self.table.decode_block(index, block, out)
    }

    /// Appends stream bytes `start..start + len` to `out`, decoding every block they overlap.
    ///
    /// # Errors
    /// [`Error::OutOfBounds`] if the range ends past the stream; otherwise as [`BlockReader::read_block`]. On
    /// error `out` is left as it was.
    pub fn read_range(&self, start: u64, len: u64, out: &mut Vec<u8>) -> Result<()> {
        let total_len = self.table.total_len;
        let end = start
            .checked_add(len)
            .filter(|&end| end <= total_len)
            .ok_or(Error::OutOfBounds {
                index: start.saturating_add(len),
                len: total_len,
            })?;
        if len == 0 {
            return Ok(());
        }
        let block_size = u64::from(self.table.block_size);
        let first = start.checked_div(block_size).unwrap_or(0);
        let last = end.saturating_sub(1).checked_div(block_size).unwrap_or(0);
        let rollback = out.len();
        let mut scratch = Vec::new();
        for index in first..=last {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            scratch.clear();
            if let Err(e) = self.read_block(index, &mut scratch) {
                out.truncate(rollback);
                return Err(e);
            }
            let block_start = self.block_start(index);
            let lo = usize::try_from(start.saturating_sub(block_start)).unwrap_or(usize::MAX);
            let hi = usize::try_from(end.saturating_sub(block_start)).unwrap_or(usize::MAX);
            out.extend_from_slice(scratch.get(lo..hi.min(scratch.len())).unwrap_or_default());
        }
        Ok(())
    }
}

/// Tags a block-level decode error with the block's index.
fn in_block(index: u32, e: Error) -> Error {
    match e {
        Error::Corrupt { offset, reason } => Error::CorruptBlock {
            index,
            offset,
            reason,
        },
        other => other,
    }
}

/// Checks the table's `block_end` entries (SPEC §5.2): each block is at least a header long and at most a
/// header plus `block_size`, and the last one (or position 0 when there are none) ends where the table starts.
fn validate_ends(
    ends: &[u8],
    block_count: u32,
    block_size: u32,
    base: u64,
    table_start: u64,
) -> Result<()> {
    let table_start_u64 = table_start;
    let corrupt = |pos: u64, end: u64, min: u64, max: u64| Error::Corrupt {
        offset: base.saturating_add(pos),
        reason: Corruption::InvalidBlockEnd { end, min, max },
    };
    if block_count == 0 && table_start != 0 {
        return Err(corrupt(0, 0, table_start_u64, table_start_u64));
    }
    let header = to_u64(BLOCK_HEADER_LEN);
    let largest = header.saturating_add(u64::from(block_size));
    let last = usize::try_from(block_count)
        .unwrap_or(usize::MAX)
        .saturating_sub(1);
    let mut prev = 0_u64;
    for (i, entry) in ends.as_chunks::<END_LEN>().0.iter().enumerate() {
        let end = u64::from_le_bytes(*entry);
        let shortest = prev.saturating_add(header);
        let longest = prev.saturating_add(largest).min(table_start_u64);
        let (min, max) = if i == last {
            (shortest.max(table_start_u64), longest)
        } else {
            (shortest, longest)
        };
        if end < min || end > max {
            let pos = table_start.saturating_add(to_u64(i.saturating_mul(END_LEN)));
            return Err(corrupt(pos, end, min, max));
        }
        prev = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 1000;

    fn section(block_size: u32, compression: Compression, data: &[u8]) -> Vec<u8> {
        let mut w = BlockWriter::new(block_size, compression).unwrap();
        w.write(data).unwrap();
        w.finish().unwrap()
    }

    fn read_all(r: &BlockReader<'_>) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for i in 0..r.block_count() {
            r.read_block(i, &mut out)?;
        }
        Ok(out)
    }

    /// Hand-built `None` block: header fields per SPEC §5.2, CRC over bytes 4.. from the CRC primitive.
    fn none_block(payload: &[u8]) -> Vec<u8> {
        let len = u32::try_from(payload.len()).unwrap().to_le_bytes();
        let mut b = vec![0, 0, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(&len);
        b.extend_from_slice(&len);
        b.extend_from_slice(payload);
        let crc = crc32c(&b[4..]).to_le_bytes();
        b[..4].copy_from_slice(&crc);
        b
    }

    /// Hand-built block table (SPEC §5.2).
    fn table(ends: &[u64], total_len: u64, block_size: u32) -> Vec<u8> {
        let mut t = Vec::new();
        for end in ends {
            t.extend_from_slice(&end.to_le_bytes());
        }
        t.extend_from_slice(&total_len.to_le_bytes());
        t.extend_from_slice(&block_size.to_le_bytes());
        t.extend_from_slice(&u32::try_from(ends.len()).unwrap().to_le_bytes());
        t.extend_from_slice(&[0; 4]);
        let crc = crc32c(&t).to_le_bytes();
        t.extend_from_slice(&crc);
        t
    }

    fn corrupt(offset: u64, reason: Corruption) -> Error {
        Error::Corrupt { offset, reason }
    }

    #[test]
    fn golden_section() {
        let mut expected = none_block(b"hell");
        expected.extend(none_block(b"o"));
        expected.extend(table(&[20, 37], 5, 4));
        assert_eq!(section(4, Compression::None, b"hello"), expected);
        // Pins the CRC32C of the first header too, independently of the helper above.
        assert_eq!(&expected[..4], &0x4e54_f6b4_u32.to_le_bytes());

        let r = BlockReader::open(&expected, BASE).unwrap();
        assert_eq!((r.block_count(), r.block_size(), r.len()), (2, 4, 5));
        assert_eq!(r.block_range(1), Ok(20..37));
        assert_eq!(read_all(&r).unwrap(), b"hello");
    }

    /// Reader side of a zstd block, built by hand around the zstd frame pinned in `compress.rs` (zstd 1.5.7,
    /// level 3, `b"persia "` × 8): writer output depends on the zstd version, decoding must not.
    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI")]
    fn golden_zstd_block() {
        const FRAME: &[u8] = &[
            0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x38, 0x6d, 0x00, 0x00, 0x38, 0x70, 0x65, 0x72, 0x73,
            0x69, 0x61, 0x20, 0x01, 0x00, 0x2a, 0x2a, 0x46,
        ];
        let mut block = vec![0, 0, 0, 0, 1, 0, 0, 0, 56, 0, 0, 0, 22, 0, 0, 0];
        block.extend_from_slice(FRAME);
        let crc = crc32c(&block[4..]).to_le_bytes();
        block[..4].copy_from_slice(&crc);
        block.extend(table(&[38], 56, 64));

        let r = BlockReader::open(&block, BASE).unwrap();
        let mut out = Vec::new();
        r.read_block(0, &mut out).unwrap();
        assert_eq!(out, b"persia ".repeat(8));
    }

    #[test]
    fn golden_empty_section() {
        let expected = table(&[], 0, DEFAULT_BLOCK_SIZE);
        assert_eq!(
            section(DEFAULT_BLOCK_SIZE, Compression::None, b""),
            expected
        );
        let r = BlockReader::open(&expected, BASE).unwrap();
        assert!(r.is_empty());
        assert_eq!(r.block_count(), 0);
        assert_eq!(
            r.read_block(0, &mut Vec::new()),
            Err(Error::OutOfBounds { index: 0, len: 0 })
        );
        assert_eq!(r.read_range(0, 0, &mut Vec::new()), Ok(()));
    }

    #[test]
    fn header_round_trip_and_field_errors() {
        let header = BlockHeader::with_uncompressed_len(Codec::Zstd, 99, b"xyz").unwrap();
        let mut w = Writer::new();
        header.encode(&mut w);
        let mut bytes = w.into_inner();
        assert_eq!(bytes[4..], [1, 0, 0, 0, 99, 0, 0, 0, 3, 0, 0, 0]);
        assert_eq!(BlockHeader::decode(&mut Reader::new(&bytes)), Ok(header));

        bytes[4] = 2;
        let mut r = Reader::with_base_offset(&bytes, BASE);
        assert_eq!(
            BlockHeader::decode(&mut r),
            Err(corrupt(BASE + 4, Corruption::UnknownCodec { codec: 2 }))
        );
        assert_eq!(r.position(), 0, "position unchanged on error");
        bytes[4] = 0;
        bytes[6] = 7;
        assert_eq!(
            BlockHeader::decode(&mut Reader::with_base_offset(&bytes, BASE)),
            Err(corrupt(BASE + 6, Corruption::NonZeroReserved { value: 7 }))
        );
    }

    #[test]
    fn writer_rejects_bad_parameters() {
        for size in [0, MAX_BLOCK_SIZE + 1] {
            assert_eq!(
                BlockWriter::new(size, Compression::None).unwrap_err(),
                EncodeError::InvalidBlockSize {
                    size,
                    max: MAX_BLOCK_SIZE
                }
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI")]
    fn writer_rejects_bad_zstd_level() {
        assert!(matches!(
            BlockWriter::new(16, Compression::Zstd { level: i32::MAX }),
            Err(EncodeError::InvalidCompressionLevel { .. })
        ));
    }

    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI")]
    fn compressible_blocks_use_zstd_and_others_none() {
        let mut data = vec![b'a'; 64];
        data.extend((0..64_u8).map(|i| i.wrapping_mul(97)));
        let bytes = section(64, Compression::default(), &data);
        let r = BlockReader::open(&bytes, 0).unwrap();
        let codec_of = |i| {
            let start = usize::try_from(r.block_range(i).unwrap().start).unwrap();
            bytes[start + 4]
        };
        assert_eq!((codec_of(0), codec_of(1)), (1, 0));
        assert_eq!(read_all(&r).unwrap(), data);
    }

    #[test]
    fn write_in_pieces_equals_write_at_once() {
        let data: Vec<u8> = (0..=255).collect();
        let whole = section(7, Compression::None, &data);
        let mut w = BlockWriter::new(7, Compression::None).unwrap();
        for piece in data.chunks(5) {
            w.write(piece).unwrap();
        }
        assert_eq!(w.position(), 256);
        assert_eq!(w.finish().unwrap(), whole);

        // Draining output as it is produced yields the same section.
        let mut w = BlockWriter::new(7, Compression::None).unwrap();
        let mut streamed = w.take_output();
        assert_eq!(streamed, [0_u8; 0]);
        for piece in data.chunks(30) {
            w.write(piece).unwrap();
            streamed.extend(w.take_output());
        }
        streamed.extend(w.finish().unwrap());
        assert_eq!(streamed, whole);
    }

    #[test]
    fn read_range_spans_blocks() {
        let data: Vec<u8> = (0..100).collect();
        let bytes = section(8, Compression::None, &data);
        let r = BlockReader::open(&bytes, 0).unwrap();
        for (start, len) in [(0, 100), (3, 0), (5, 20), (95, 5), (8, 8)] {
            let mut out = vec![9];
            r.read_range(start, len, &mut out).unwrap();
            let (s, e) = (
                usize::try_from(start).unwrap(),
                usize::try_from(start + len).unwrap(),
            );
            assert_eq!(out[1..], data[s..e]);
        }
        // A range that ends exactly at the end of a full last block.
        let bytes = section(8, Compression::None, &data[..96]);
        let mut out = Vec::new();
        BlockReader::open(&bytes, 0)
            .unwrap()
            .read_range(88, 8, &mut out)
            .unwrap();
        assert_eq!(out, data[88..96]);
        assert_eq!(
            r.read_range(90, 11, &mut Vec::new()),
            Err(Error::OutOfBounds {
                index: 101,
                len: 100
            })
        );
        assert_eq!(
            r.read_range(u64::MAX, 2, &mut Vec::new()),
            Err(Error::OutOfBounds {
                index: u64::MAX,
                len: 100
            })
        );
    }

    #[test]
    fn corrupt_block_names_its_index_and_leaves_out_untouched() {
        let mut bytes = section(4, Compression::None, b"hello world");
        bytes[20 + 16] ^= 1; // payload of block 1
        let r = BlockReader::open(&bytes, BASE).unwrap();
        let mut out = vec![7];
        let err = r.read_block(1, &mut out).unwrap_err();
        assert!(
            matches!(err, Error::CorruptBlock { index: 1, offset, reason: Corruption::ChecksumMismatch { .. } } if offset == BASE + 20),
            "{err:?}"
        );
        assert_eq!(out, [7]);
        r.read_block(0, &mut out).unwrap();
        assert_eq!(r.read_range(0, 11, &mut out), Err(err));
        assert_eq!(out, b"\x07hell");
    }

    #[test]
    fn block_header_must_agree_with_table() {
        // Block 0 claims 3 uncompressed bytes where the table implies 4 (a valid CRC, so the header is read).
        let mut block = Vec::new();
        let header = BlockHeader::with_uncompressed_len(Codec::None, 3, b"hell").unwrap();
        let mut w = Writer::new();
        header.encode(&mut w);
        block.extend(w.as_slice());
        block.extend(b"hell");
        let mut bytes = block.clone();
        bytes.extend(table(&[20], 4, 4));
        let r = BlockReader::open(&bytes, BASE).unwrap();
        assert_eq!(
            r.read_block(0, &mut Vec::new()),
            Err(Error::CorruptBlock {
                index: 0,
                offset: BASE + 8,
                reason: Corruption::BlockLenMismatch {
                    header: 3,
                    table: 4
                }
            })
        );

        // A `stored_len` that disagrees with the block's extent in the table.
        let header = BlockHeader {
            stored_len: 5,
            ..BlockHeader::new(Codec::None, b"hell").unwrap()
        };
        let mut w = Writer::new();
        header.encode(&mut w);
        w.write_bytes(b"hell");
        let mut block = w.into_inner();
        let crc = crc32c(&block[4..]).to_le_bytes();
        block[..4].copy_from_slice(&crc);
        block.extend(table(&[20], 4, 4));
        let r = BlockReader::open(&block, BASE).unwrap();
        assert_eq!(
            r.read_block(0, &mut Vec::new()),
            Err(Error::CorruptBlock {
                index: 0,
                offset: BASE + 12,
                reason: Corruption::BlockLenMismatch {
                    header: 5,
                    table: 4
                }
            })
        );
    }

    /// "hell" and "o" as two `None` blocks of size 4, followed by table `t`.
    fn with_table(t: Vec<u8>) -> Vec<u8> {
        [none_block(b"hell"), none_block(b"o"), t].concat()
    }

    fn open(bytes: &[u8]) -> Result<()> {
        BlockReader::open(bytes, BASE).map(|_| ())
    }

    /// Absolute offset of the table trailer.
    fn trailer(bytes: &[u8]) -> u64 {
        BASE + u64::try_from(bytes.len()).unwrap() - 24
    }

    #[test]
    fn table_rules() {
        let short = [0_u8; 23];
        assert_eq!(
            open(&short),
            Err(corrupt(
                BASE,
                Corruption::UnexpectedEof {
                    needed: 24,
                    available: 23
                }
            ))
        );

        // block_count larger than the section.
        let mut bytes = with_table(table(&[20, 37], 5, 4));
        let count_at = bytes.len() - 12;
        bytes[count_at] = 200;
        assert!(matches!(
            open(&bytes),
            Err(Error::Corrupt {
                offset: BASE,
                reason: Corruption::UnexpectedEof { .. }
            })
        ));

        // Any flipped table bit fails the table CRC.
        let mut bytes = with_table(table(&[20, 37], 5, 4));
        bytes[37] ^= 0x10;
        let crc_at = BASE + u64::try_from(bytes.len()).unwrap() - 4;
        assert!(
            matches!(open(&bytes), Err(Error::Corrupt { offset, reason: Corruption::ChecksumMismatch { .. } }) if offset == crc_at)
        );

        let bytes = with_table(table(&[20, 37], 5, 0));
        assert_eq!(
            open(&bytes),
            Err(corrupt(
                trailer(&bytes) + 8,
                Corruption::InvalidBlockSize {
                    size: 0,
                    max: MAX_BLOCK_SIZE
                }
            ))
        );
        let bytes = with_table(table(&[20, 37], 9, 4));
        assert_eq!(
            open(&bytes),
            Err(corrupt(
                trailer(&bytes) + 12,
                Corruption::BlockCountMismatch {
                    count: 2,
                    expected: 3
                }
            ))
        );
    }

    #[test]
    fn table_block_ends_and_reserved() {
        // A block longer than a header plus `block_size` (here 16 + 4): a CRC-valid table cannot make one block
        // fetch unbounded bytes. 20 bytes of block 0 plus 17 of block 1 become one 37-byte block of 5 bytes.
        let bytes = [none_block(b"hell"), none_block(b"o"), table(&[37], 4, 5)].concat();
        assert_eq!(
            open(&bytes),
            Err(corrupt(
                BASE + 37,
                Corruption::InvalidBlockEnd {
                    end: 37,
                    min: 37,
                    max: 21
                }
            ))
        );
        // Block 0 shorter than a header; last block not ending at the table.
        let bytes = with_table(table(&[15, 37], 5, 4));
        assert_eq!(
            open(&bytes),
            Err(corrupt(
                BASE + 37,
                Corruption::InvalidBlockEnd {
                    end: 15,
                    min: 16,
                    max: 20
                }
            ))
        );
        let bytes = with_table(table(&[20, 36], 5, 4));
        assert_eq!(
            open(&bytes),
            Err(corrupt(
                BASE + 45,
                Corruption::InvalidBlockEnd {
                    end: 36,
                    min: 37,
                    max: 37
                }
            ))
        );
        // No blocks, but bytes before the table.
        let bytes = with_table(table(&[], 0, 4));
        assert_eq!(
            open(&bytes),
            Err(corrupt(
                BASE,
                Corruption::InvalidBlockEnd {
                    end: 0,
                    min: 37,
                    max: 37
                }
            ))
        );
        // Reserved bytes are covered by the CRC, then checked.
        let mut t = table(&[20, 37], 5, 4);
        let at = t.len() - 8;
        t[at + 1] = 3;
        let crc = crc32c(&t[..t.len() - 4]).to_le_bytes();
        let crc_at = t.len() - 4;
        t[crc_at..].copy_from_slice(&crc);
        let bytes = with_table(t);
        assert_eq!(
            open(&bytes),
            Err(corrupt(
                trailer(&bytes) + 17,
                Corruption::NonZeroReserved { value: 3 }
            ))
        );
    }
}
