//! Reading the logical stream of a block-framed section (SPEC §5.2): whole, a block at a time through the segment's
//! [`BlockCache`] (ROADMAP 5.3.11), or a range at a time through a [`Cursor`] that keeps the last block it read, so
//! sequential reads (postings blocks, skip entries, runs that span blocks) look each block up once.

use std::sync::Arc;

use persia_format::Reader;
use persia_format::block::{BlockReader, ValidatedTable};

use super::super::SectionKind;
use super::cache::BlockCache;
use super::error::{ReadError, SegmentCorruption};

/// A block-framed section whose block table was validated when the segment was opened.
#[derive(Debug, Clone, Copy)]
pub(super) struct Stream<'s> {
    pub(super) kind: SectionKind,
    pub(super) field: u32,
    blocks: BlockReader<'s>,
    /// The cache its blocks go through, and the segment instance they are keyed by; `None` decodes every read.
    cache: Option<(&'s BlockCache, u64)>,
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
            cache: None,
        })
    }

    /// Rebuilds the reader of `bytes`, whose block table [`Stream::open`] validated as `table` (ROADMAP 5.3.10).
    pub(super) fn reopen(
        kind: SectionKind,
        field: u32,
        bytes: &'s [u8],
        base: u64,
        table: ValidatedTable,
    ) -> Result<Self, ReadError> {
        let blocks =
            BlockReader::reopen(bytes, base, table).map_err(|source| ReadError::Block {
                kind,
                field,
                source,
            })?;
        Ok(Self {
            kind,
            field,
            blocks,
            cache: None,
        })
    }

    /// The same stream, its blocks read through `cache` under segment instance `segment`.
    pub(super) fn cached(self, cache: &'s BlockCache, segment: u64) -> Self {
        Self {
            cache: Some((cache, segment)),
            ..self
        }
    }

    /// What [`Stream::open`] validated, for [`Stream::reopen`].
    pub(super) fn validated(&self) -> ValidatedTable {
        self.blocks.validated()
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

    /// Block `index`, decoded into `out` (cleared first), bypassing the cache.
    pub(super) fn read_block(&self, index: u32, out: &mut Vec<u8>) -> Result<(), ReadError> {
        out.clear();
        self.blocks
            .read_block(index, out)
            .map_err(|e| self.error(e))
    }

    /// Block `index`, from the cache when it holds it.
    pub(super) fn block(&self, index: u32) -> Result<Arc<Vec<u8>>, ReadError> {
        let decode = || {
            let mut buf = Vec::new();
            self.read_block(index, &mut buf)?;
            Ok(buf)
        };
        match self.cache {
            Some((cache, segment)) => cache.get(segment, (self.kind, self.field, index), decode),
            None => decode().map(Arc::new),
        }
    }

    /// Fills `out` with the stream bytes from `start`, block by block through the cache.
    pub(super) fn read_into(&self, start: u64, out: &mut [u8]) -> Result<(), ReadError> {
        let end = start
            .checked_add(u64::try_from(out.len()).unwrap_or(u64::MAX))
            .filter(|&e| e <= self.len())
            .ok_or_else(|| {
                self.corrupt(
                    start,
                    SegmentCorruption::Invalid {
                        what: "a range past the end of its stream",
                    },
                )
            })?;
        let block_size = self.block_size();
        let mut pos = start;
        let mut written = 0_usize;
        while pos < end {
            let index = pos.checked_div(block_size).unwrap_or(0);
            let block_start = index.saturating_mul(block_size);
            let block = self.block(u32::try_from(index).unwrap_or(u32::MAX))?;
            let to_pos = end.min(block_start.saturating_add(block_size));
            let bytes =
                slice_of(&block, block_start, pos, to_pos).ok_or_else(|| short(self, pos))?;
            let next = written.saturating_add(bytes.len());
            out.get_mut(written..next)
                .ok_or_else(|| short(self, pos))?
                .copy_from_slice(bytes);
            written = next;
            pos = to_pos;
        }
        Ok(())
    }

    /// The `u64` at stream position `at`.
    pub(super) fn u64_at(&self, at: u64) -> Result<u64, ReadError> {
        let mut bytes = [0_u8; 8];
        self.read_into(at, &mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// Number of framed blocks.
    pub(super) fn block_count(&self) -> u32 {
        self.blocks.block_count()
    }

    /// Uncompressed bytes per block.
    pub(super) fn block_size(&self) -> u64 {
        u64::from(self.blocks.block_size())
    }
}

/// Stream bytes `from..to` of a decoded block that starts at stream position `block_start`.
fn slice_of(block: &[u8], block_start: u64, from: u64, to: u64) -> Option<&[u8]> {
    let from = usize::try_from(from.checked_sub(block_start)?).ok()?;
    let to = usize::try_from(to.checked_sub(block_start)?).ok()?;
    block.get(from..to)
}

fn short(stream: &Stream<'_>, position: u64) -> ReadError {
    stream.corrupt(
        position,
        SegmentCorruption::Invalid {
            what: "a block shorter than its table says",
        },
    )
}

/// Reads ranges of one stream, keeping the last block it read.
#[derive(Debug, Clone)]
pub(super) struct Cursor<'s> {
    pub(super) stream: Stream<'s>,
    block: Option<(u32, Arc<Vec<u8>>)>,
    spill: Vec<u8>,
}

impl<'s> Cursor<'s> {
    pub(super) fn new(stream: Stream<'s>) -> Self {
        Self {
            stream,
            block: None,
            spill: Vec::new(),
        }
    }

    /// Block `index`, kept until another is read.
    fn load(&mut self, index: u32) -> Result<&[u8], ReadError> {
        if self.block.as_ref().is_none_or(|(i, _)| *i != index) {
            self.block = None;
            self.block = Some((index, self.stream.block(index)?));
        }
        Ok(self.block.as_ref().map_or(&[][..], |(_, b)| b.as_slice()))
    }

    /// Stream bytes `start..end`, from the kept block when they lie in one block.
    pub(super) fn read(&mut self, start: u64, end: u64) -> Result<&[u8], ReadError> {
        let block_size = self.stream.block_size();
        let index = start.checked_div(block_size).unwrap_or(0);
        let block_start = index.saturating_mul(block_size);
        let in_one_block = start < end
            && end <= self.stream.len()
            && end.saturating_sub(block_start) <= block_size;
        if !in_one_block {
            self.spill.clear();
            if start < end && end <= self.stream.len() {
                self.read_spanning(start, end)?;
            } else {
                // Empty, or past the stream: `read_range` reports which.
                self.stream.read_range(start, end, &mut self.spill)?;
            }
            return Ok(&self.spill);
        }
        let stream = self.stream;
        let block = self.load(u32::try_from(index).unwrap_or(u32::MAX))?;
        // start..end lies in the block, which holds block_size bytes (or the stream's rest).
        slice_of(block, block_start, start, end).ok_or_else(|| short(&stream, start))
    }

    /// Copies stream bytes `start..end` (several blocks) into `spill` block by block through the kept block, which
    /// ends up holding the last one: a sequential reader looks every block up once.
    fn read_spanning(&mut self, start: u64, end: u64) -> Result<(), ReadError> {
        let block_size = self.stream.block_size();
        let len = usize::try_from(end.saturating_sub(start)).unwrap_or(usize::MAX);
        self.spill.try_reserve_exact(len).map_err(|_| {
            self.stream.error(persia_format::Error::OutOfMemory {
                bytes: end.checked_sub(start),
            })
        })?;
        let stream = self.stream;
        let mut pos = start;
        while pos < end {
            let index = pos.checked_div(block_size).unwrap_or(0);
            let block_start = index.saturating_mul(block_size);
            let to_pos = end.min(block_start.saturating_add(block_size));
            self.load(u32::try_from(index).unwrap_or(u32::MAX))?;
            let block = self.block.as_ref().map_or(&[][..], |(_, b)| b.as_slice());
            let bytes =
                slice_of(block, block_start, pos, to_pos).ok_or_else(|| short(&stream, pos))?;
            self.spill.extend_from_slice(bytes);
            pos = to_pos;
        }
        Ok(())
    }

    /// A [`Reader`] over stream bytes `start..end`, whose offsets are stream positions.
    pub(super) fn reader(&mut self, start: u64, end: u64) -> Result<Reader<'_>, ReadError> {
        let bytes = self.read(start, end)?;
        Ok(Reader::with_base_offset(bytes, start))
    }
}
