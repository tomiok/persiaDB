//! Reading the logical stream of a block-framed section (SPEC §5.2): whole, or a range at a time through a
//! [`Cursor`] that keeps the last decoded block, so sequential small reads (postings blocks, skip entries) decode
//! each framed block once.

use persia_format::Reader;
use persia_format::block::BlockReader;

use super::super::SectionKind;
use super::error::{ReadError, SegmentCorruption};

/// A block-framed section whose block table was validated when the segment was opened.
#[derive(Debug, Clone, Copy)]
pub(super) struct Stream<'s> {
    pub(super) kind: SectionKind,
    pub(super) field: u32,
    blocks: BlockReader<'s>,
}

impl<'s> Stream<'s> {
    /// Opens the section `bytes`, which starts at absolute offset `base`.
    pub(super) fn open(
        kind: SectionKind,
        field: u32,
        bytes: &'s [u8],
        base: u64,
    ) -> Result<Self, ReadError> {
        let blocks = BlockReader::open(bytes, base).map_err(|source| ReadError::Block {
            kind,
            field,
            source,
        })?;
        Ok(Self {
            kind,
            field,
            blocks,
        })
    }

    /// Length of the stream.
    pub(super) fn len(&self) -> u64 {
        self.blocks.len()
    }

    /// A corruption of this section at stream position `position`.
    pub(super) fn corrupt(&self, position: u64, reason: SegmentCorruption) -> ReadError {
        ReadError::Section {
            kind: self.kind,
            field: self.field,
            position,
            reason,
        }
    }

    /// Maps an error of a [`Reader`] over this stream's bytes (positions as offsets) or of its blocks.
    pub(super) fn error(&self, e: persia_format::Error) -> ReadError {
        match e {
            persia_format::Error::Corrupt { offset, reason } => self.corrupt(offset, reason.into()),
            source => ReadError::Block {
                kind: self.kind,
                field: self.field,
                source,
            },
        }
    }

    /// The bytes `start..end` of the stream appended to `out`; a range past the stream is corruption (positions
    /// come from the segment).
    pub(super) fn read_range(
        &self,
        start: u64,
        end: u64,
        out: &mut Vec<u8>,
    ) -> Result<(), ReadError> {
        let len = end
            .checked_sub(start)
            .filter(|_| end <= self.len())
            .ok_or_else(|| {
                self.corrupt(
                    start,
                    SegmentCorruption::Invalid {
                        what: "a range past the end of its stream",
                    },
                )
            })?;
        // The bytes are decoded into `out`: reserve first, so a large stream fails cleanly instead of aborting.
        let reserve = usize::try_from(len).unwrap_or(usize::MAX);
        out.try_reserve_exact(reserve)
            .map_err(|_| self.error(persia_format::Error::OutOfMemory { bytes: Some(len) }))?;
        self.blocks
            .read_range(start, len, out)
            .map_err(|e| self.error(e))
    }

    /// The whole stream.
    pub(super) fn read_all(&self) -> Result<Vec<u8>, ReadError> {
        let mut out = Vec::new();
        self.read_range(0, self.len(), &mut out)?;
        Ok(out)
    }

    /// Block `index`, decoded into `out` (cleared first).
    pub(super) fn read_block(&self, index: u32, out: &mut Vec<u8>) -> Result<(), ReadError> {
        out.clear();
        self.blocks
            .read_block(index, out)
            .map_err(|e| self.error(e))
    }

    /// Uncompressed bytes per block.
    pub(super) fn block_size(&self) -> u64 {
        u64::from(self.blocks.block_size())
    }
}

/// Reads ranges of one stream, keeping the last decoded block.
#[derive(Debug, Clone)]
pub(super) struct Cursor<'s> {
    pub(super) stream: Stream<'s>,
    block: Option<u32>,
    buf: Vec<u8>,
    spill: Vec<u8>,
}

impl<'s> Cursor<'s> {
    pub(super) fn new(stream: Stream<'s>) -> Self {
        Self {
            stream,
            block: None,
            buf: Vec::new(),
            spill: Vec::new(),
        }
    }

    /// Stream bytes `start..end`, from the cached block when they lie in one block.
    pub(super) fn read(&mut self, start: u64, end: u64) -> Result<&[u8], ReadError> {
        let block_size = self.stream.block_size();
        let index = start.checked_div(block_size).unwrap_or(0);
        let block_start = index.saturating_mul(block_size);
        let in_one_block = start < end
            && end <= self.stream.len()
            && end.saturating_sub(block_start) <= block_size;
        if !in_one_block {
            self.spill.clear();
            self.stream.read_range(start, end, &mut self.spill)?;
            return Ok(&self.spill);
        }
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        if self.block != Some(index) {
            self.block = None;
            self.stream.read_block(index, &mut self.buf)?;
            self.block = Some(index);
        }
        // start..end lies in the block, which holds block_size bytes (or the stream's rest).
        let from = usize::try_from(start.saturating_sub(block_start)).unwrap_or(usize::MAX);
        let to = usize::try_from(end.saturating_sub(block_start)).unwrap_or(usize::MAX);
        self.buf.get(from..to).ok_or_else(|| {
            self.stream.corrupt(
                start,
                SegmentCorruption::Invalid {
                    what: "a block shorter than its table says",
                },
            )
        })
    }

    /// A [`Reader`] over stream bytes `start..end`, whose offsets are stream positions.
    pub(super) fn reader(&mut self, start: u64, end: u64) -> Result<Reader<'_>, ReadError> {
        let bytes = self.read(start, end)?;
        Ok(Reader::with_base_offset(bytes, start))
    }
}
