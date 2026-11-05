//! Typed readers of `Norms` and `Columns` (SPEC §5.3.4, §5.3.5; ROADMAP 5.3.5, 5.3.12, 5.3.14). Bool bitmaps,
//! presence bitmaps and a keyword column's dictionary are decoded when the reader is opened; norms, numeric keys and
//! keyword ords stay in their blocks, read one block at a time through the segment's block cache (SPEC §5.2.1 makes
//! any value O(1)), so a reader's memory does not grow with the document count. A cursor (ROADMAP 5.3.16) keeps the
//! block it last decoded to itself, so a scan neither locks nor allocates per value.

use core::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use persia_format::for_codec::{ForBlock, key_to_f64, key_to_i64};
use roaring::RoaringBitmap;

use super::super::norms;
use super::super::write::columns::{TAG_BOOL, TAG_KEYWORD, TAG_NUMERIC};
use super::cache::Item;
use super::error::{ReadError, SegmentCorruption};
use super::stream::Stream;
use crate::bitmap::Ascending;
use crate::document::Value;
use crate::schema::FieldType;

/// The quantized field lengths of a `text` field (SPEC §5.3.4), read a block at a time through the segment's block
/// cache (ROADMAP 5.3.14): opening it costs nothing, whatever the document count.
#[derive(Debug, Clone, Copy, Default)]
pub struct Norms<'s> {
    /// `doc_count` bytes; `None` when the field has no tokens in the segment.
    stream: Option<Stream<'s>>,
}

impl<'s> Norms<'s> {
    pub(super) fn read(stream: Option<Stream<'s>>, doc_count: u32) -> Result<Self, ReadError> {
        if let Some(stream) = &stream
            && stream.len() != u64::from(doc_count)
        {
            return Err(stream.corrupt(
                0,
                SegmentCorruption::Invalid {
                    what: "a Norms stream other than doc_count bytes",
                },
            ));
        }
        Ok(Self { stream })
    }

    /// The norm byte of `docid`; 0 for a document without tokens in the field (or a docid past the segment).
    ///
    /// # Errors
    /// A corruption if the block that holds it is.
    pub fn get(&self, docid: u32) -> Result<u8, ReadError> {
        match self.stream {
            Some(stream) if u64::from(docid) < stream.len() => {
                let mut norm = [0_u8];
                stream.read_into(u64::from(docid), &mut norm)?;
                let [norm] = norm;
                Ok(norm)
            }
            _ => Ok(0),
        }
    }

    /// The field length `docid`'s norm stands for: exact below 40 tokens, otherwise rounded down by less than
    /// 12.5 % (SPEC §5.3.4).
    ///
    /// # Errors
    /// As [`Self::get`].
    pub fn length(&self, docid: u32) -> Result<u32, ReadError> {
        self.get(docid).map(norms::decode)
    }

    /// A reader that keeps the last block it read: for scoring many documents, in docid order or close to it.
    pub fn cursor(&self) -> NormsCursor<'s> {
        NormsCursor {
            stream: self.stream,
            block: None,
        }
    }
}

/// Reads [`Norms`] keeping the last block it read, so a scan looks each block up in the cache once (ROADMAP 5.3.16).
#[derive(Debug, Clone)]
pub struct NormsCursor<'s> {
    stream: Option<Stream<'s>>,
    /// The kept block and the stream position of its first byte.
    block: Option<(u64, Arc<Vec<u8>>)>,
}

impl NormsCursor<'_> {
    /// As [`Norms::get`].
    ///
    /// # Errors
    /// As [`Norms::get`].
    #[inline]
    pub fn get(&mut self, docid: u32) -> Result<u8, ReadError> {
        // The hot path of scoring: the norm is in the kept block.
        let at = u64::from(docid);
        if let Some((start, block)) = &self.block
            && let Some(&norm) = at
                .checked_sub(*start)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| block.get(i))
        {
            return Ok(norm);
        }
        self.get_slow(at)
    }

    #[inline(never)]
    fn get_slow(&mut self, at: u64) -> Result<u8, ReadError> {
        let Some(stream) = self.stream else {
            return Ok(0);
        };
        if at >= stream.len() {
            return Ok(0);
        }
        self.block = None;
        let index = at.checked_div(stream.block_size()).unwrap_or(0);
        let start = index.saturating_mul(stream.block_size());
        let block = stream.block(u32::try_from(index).unwrap_or(u32::MAX))?;
        let norm = at
            .checked_sub(start)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| block.get(i))
            .copied()
            .ok_or_else(|| bad(&stream, at, "a block shorter than its table says"))?;
        self.block = Some((start, block));
        Ok(norm)
    }

    /// As [`Norms::length`].
    ///
    /// # Errors
    /// As [`Norms::get`].
    pub fn length(&mut self, docid: u32) -> Result<u32, ReadError> {
        self.get(docid).map(norms::decode)
    }
}

/// Reads the parts of a `Columns` stream in order from a stream position, mapping errors to its section.
struct Parts<'s> {
    stream: Stream<'s>,
    at: u64,
}

impl<'s> Parts<'s> {
    fn new(stream: Stream<'s>) -> Self {
        Self { stream, at: 0 }
    }

    fn bad(&self, position: u64, what: &'static str) -> ReadError {
        bad(&self.stream, position, what)
    }

    /// The position `len` bytes on, which must be in the stream.
    fn skip(&mut self, len: u64) -> Result<u64, ReadError> {
        let at = self.at;
        self.at = at
            .checked_add(len)
            .filter(|&e| e <= self.stream.len())
            .ok_or_else(|| self.bad(at, "a column part past the end of its stream"))?;
        Ok(at)
    }

    fn u64(&mut self) -> Result<u64, ReadError> {
        let at = self.skip(8)?;
        self.stream.u64_at(at)
    }

    fn bytes(&mut self, len: u64) -> Result<Vec<u8>, ReadError> {
        let at = self.skip(len)?;
        let mut out = Vec::new();
        self.stream.read_range(at, self.at, &mut out)?;
        Ok(out)
    }

    /// The tag byte and its 7 zero bytes.
    fn header(&mut self, tag: u8) -> Result<(), ReadError> {
        let at = self.skip(8)?;
        let stream = self.stream;
        stream.with_reader(at, self.at, |r| {
            let got = r.read_u8().map_err(|e| stream.error(e))?;
            if got != tag {
                return Err(bad(&stream, at, "a column tag other than the field type's"));
            }
            r.expect_zeros(7).map_err(|e| stream.error(e))
        })
    }

    /// A length-prefixed roaring bitmap; `None` for length 0. Its docids must be below `doc_count`.
    fn bitmap(&mut self, doc_count: u32) -> Result<Option<RoaringBitmap>, ReadError> {
        let at = self.at;
        let len = self.u64()?;
        if len == 0 {
            return Ok(None);
        }
        let bytes = self.bytes(len)?;
        let bitmap = RoaringBitmap::deserialize_from(bytes.as_slice())
            .map_err(|_| self.bad(at, "a bitmap that does not deserialize"))?;
        if u64::try_from(bitmap.serialized_size()).ok() != Some(len) {
            return Err(self.bad(at, "bytes after a bitmap"));
        }
        if bitmap.max().is_some_and(|d| d >= doc_count) {
            return Err(self.bad(at, "a bitmap docid at or above doc_count"));
        }
        Ok(Some(bitmap))
    }

    /// The stream must end here.
    fn finish(&self) -> Result<(), ReadError> {
        if self.at == self.stream.len() {
            Ok(())
        } else {
            Err(self
                .stream
                .corrupt(self.at, SegmentCorruption::TrailingBytes))
        }
    }
}

fn bad(stream: &Stream<'_>, position: u64, what: &'static str) -> ReadError {
    stream.corrupt(position, SegmentCorruption::Invalid { what })
}

/// Values per FOR block (SPEC §5.2.1).
const PER_BLOCK: u64 = 1024;

/// Bytes of the largest FOR block: 13 header bytes and 1024 values of 64 bits (SPEC §5.2.1).
const MAX_FOR_BLOCK_LEN: u64 = 13 + 8 * 1024;

/// A FOR column (SPEC §5.3.5) whose blocks are decoded when a value is asked for. Opening it reads its count and
/// last block end; a block's end, count and contents are checked whenever it is read.
#[derive(Debug, Clone)]
struct ForColumn<'s> {
    stream: Stream<'s>,
    /// Where the column starts (its count), for errors.
    at: u64,
    count: u64,
    /// Stream position of the first block end.
    ends_at: u64,
    /// Stream position of the first block.
    data_at: u64,
    /// Length of the blocks, together.
    data_len: u64,
    last: LastBlock,
    /// Every value, decoded by [`ForColumn::preload`]; `None` reads block by block.
    all: Option<Arc<Vec<u64>>>,
}

impl<'s> ForColumn<'s> {
    /// The FOR column at the parts' position, which must hold exactly `count` values; the parts move past it. With
    /// `strict`, every block is read now and must use its true minimum and the minimal width (what only
    /// [`super::Segment::verify`] checks).
    fn open(parts: &mut Parts<'s>, count: u64, strict: bool) -> Result<Self, ReadError> {
        let at = parts.at;
        if parts.u64()? != count {
            return Err(parts.bad(at, "a FOR column count"));
        }
        let blocks = count.div_ceil(PER_BLOCK);
        let ends_at = parts.at;
        let too_long = |p: &Parts<'_>| p.bad(at, "a FOR column longer than its stream");
        let ends_len = blocks.checked_mul(8).ok_or_else(|| too_long(parts))?;
        parts.skip(ends_len).map_err(|_| too_long(parts))?;
        let data_at = parts.at;
        let data_len = match ends_len.checked_sub(8) {
            Some(last) => parts.stream.u64_at(ends_at.saturating_add(last))?,
            None => 0,
        };
        // Every block takes at least its 13 header bytes.
        if data_len < blocks.saturating_mul(13) {
            return Err(parts.bad(ends_at, "FOR block ends that do not increase"));
        }
        parts.skip(data_len).map_err(|_| too_long(parts))?;
        let column = Self {
            stream: parts.stream,
            at,
            count,
            ends_at,
            data_at,
            data_len,
            last: LastBlock::default(),
            all: None,
        };
        // The last block's start too: a last end at or below the one before is the ends' fault, not trailing bytes.
        if let Some(last) = blocks.checked_sub(1) {
            column.block_range(last)?;
        }
        if strict {
            for b in 0..blocks {
                column.with_block(b, |block, at| {
                    if minimal(block) {
                        Ok(())
                    } else {
                        Err(column.bad(at, "a FOR block whose minimum or width is not minimal"))
                    }
                })?;
            }
        }
        Ok(column)
    }

    fn bad(&self, position: u64, what: &'static str) -> ReadError {
        bad(&self.stream, position, what)
    }

    /// Where block `b` lies in the stream.
    fn block_range(&self, b: u64) -> Result<Range<u64>, ReadError> {
        let end_at = b
            .checked_mul(8)
            .and_then(|o| o.checked_add(self.ends_at))
            .ok_or_else(|| self.bad(self.at, "a FOR block past its column"))?;
        let end = self.stream.u64_at(end_at)?;
        let start = match end_at.checked_sub(8).filter(|_| b > 0) {
            Some(at) => self.stream.u64_at(at)?,
            None => 0,
        };
        if start >= end || end > self.data_len {
            return Err(self.bad(end_at, "FOR block ends that do not increase"));
        }
        // The longest a block can be (SPEC §5.2.1): its header and 1024 values of 64 bits, checked before it is read.
        if end.saturating_sub(start) > MAX_FOR_BLOCK_LEN {
            return Err(self.bad(end_at, "a FOR block longer than any block can be"));
        }
        // Both are at most data_len, and data_at + data_len is in the stream (checked at open).
        Ok(self.data_at.saturating_add(start)..self.data_at.saturating_add(end))
    }

    /// Calls `f` on block `b`, after checking it holds what its place in the column says, and its position.
    fn with_block<T>(
        &self,
        b: u64,
        f: impl FnOnce(&ForBlock<'_>, u64) -> Result<T, ReadError>,
    ) -> Result<T, ReadError> {
        let range = self.block_range(b)?;
        let expected = self
            .count
            .saturating_sub(b.saturating_mul(PER_BLOCK))
            .min(PER_BLOCK);
        let stream = self.stream;
        stream.with_reader(range.start, range.end, |r| {
            let block = r.read_for_block().map_err(|e| stream.error(e))?;
            if !r.is_empty() {
                return Err(bad(&stream, range.start, "bytes after a FOR block"));
            }
            if u64::try_from(block.len()).ok() != Some(expected) {
                return Err(bad(&stream, range.start, "a FOR block count"));
            }
            f(&block, range.start)
        })
    }

    /// Decodes every value now, for a reader that jumps between blocks: [`super::Segment::verify`] checks a keyword
    /// column against each term's postings, and terms come in byte order, not docid order. Costs 8 bytes per value.
    fn preload(&mut self) -> Result<(), ReadError> {
        let n = usize::try_from(self.count).unwrap_or(usize::MAX);
        let mut all = Vec::new();
        all.try_reserve_exact(n).map_err(|_| {
            self.stream.error(persia_format::Error::OutOfMemory {
                bytes: self.count.checked_mul(8),
            })
        })?;
        self.get_range(0..self.count, &mut all)?;
        self.all = Some(Arc::new(all));
        Ok(())
    }

    /// Bounds on block `b`'s keys, from its header (SPEC §5.2.1), the block read and checked but not decoded: each
    /// key is its minimum plus at most
    /// `2^width − 1`. The upper bound saturates at `u64::MAX`; reading the block checked that no value it holds goes
    /// past it.
    fn bound(&self, b: u64) -> Result<(u64, u64), ReadError> {
        self.with_block(b, |block, _| {
            // Width 0: every value is the minimum. Otherwise a shift of 0..=63.
            let span = match block.width() {
                0 => 0,
                w => u64::MAX
                    .checked_shr(64_u32.saturating_sub(u32::from(w)))
                    .unwrap_or(u64::MAX),
            };
            Ok((block.min(), block.min().saturating_add(span)))
        })
    }

    /// Block `b`'s values: the last block read stays decoded, so a scan decodes each block once.
    fn decoded(&self, b: u64) -> Result<Arc<Vec<u64>>, ReadError> {
        if let Some((last, values)) = &*self.last.lock()
            && *last == b
        {
            return Ok(Arc::clone(values));
        }
        let values =
            Arc::new(self.with_block(b, |block, _| Ok(block.iter().collect::<Vec<u64>>()))?);
        *self.last.lock() = Some((b, Arc::clone(&values)));
        Ok(values)
    }

    /// Value `i`, which must be below the count.
    fn get(&self, i: u64) -> Result<u64, ReadError> {
        if i >= self.count {
            return Err(self.bad(self.at, "a FOR value past its column"));
        }
        if let Some(all) = &self.all {
            let i = usize::try_from(i).unwrap_or(usize::MAX);
            return all
                .get(i)
                .copied()
                .ok_or_else(|| self.bad(self.at, "a FOR value past its column"));
        }
        let within = usize::try_from(i % PER_BLOCK).unwrap_or(usize::MAX);
        let values = self.decoded(i / PER_BLOCK)?;
        // `with_block` checked the block holds what its place in the column says.
        values
            .get(within)
            .copied()
            .ok_or_else(|| self.bad(self.at, "a FOR block count"))
    }

    /// Values `range` appended to `out`; the range must be within the count.
    fn get_range(&self, range: Range<u64>, out: &mut Vec<u64>) -> Result<(), ReadError> {
        if range.end > self.count {
            return Err(self.bad(self.at, "a FOR value past its column"));
        }
        if let Some(all) = &self.all {
            let from = usize::try_from(range.start).unwrap_or(usize::MAX);
            let to = usize::try_from(range.end).unwrap_or(usize::MAX);
            out.extend_from_slice(all.get(from..to).unwrap_or_default());
            return Ok(());
        }
        let mut i = range.start;
        while i < range.end {
            let b = i / PER_BLOCK;
            let block_start = b.saturating_mul(PER_BLOCK);
            let to = range.end.min(block_start.saturating_add(PER_BLOCK));
            let values = self.decoded(b)?;
            let from = usize::try_from(i.saturating_sub(block_start)).unwrap_or(usize::MAX);
            let upto = usize::try_from(to.saturating_sub(block_start)).unwrap_or(usize::MAX);
            out.extend_from_slice(values.get(from..upto).unwrap_or_default());
            i = to;
        }
        Ok(())
    }
}

/// The last FOR block a [`ForColumn`] decoded. A clone starts empty.
#[derive(Debug, Default)]
struct LastBlock(Mutex<Option<(u64, Arc<Vec<u64>>)>>);

impl LastBlock {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(u64, Arc<Vec<u64>>)>> {
        // Only ever replaced whole: a panic elsewhere cannot leave it half-written.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Clone for LastBlock {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// The last FOR block a reader decoded, in a buffer of its own (ROADMAP 5.3.16): unlike [`ForColumn::get`], no lock
/// per value, and no allocation once the buffer holds a block. It takes its column on every call, so a reader can
/// own the column beside it ([`NumericKeys`], [`KeywordOrds`]).
#[derive(Debug, Clone, Default)]
struct ForBuf {
    /// The block `values` holds.
    block: Option<u64>,
    values: Vec<u64>,
}

impl ForBuf {
    /// Block `b`'s values.
    fn load(&mut self, column: &ForColumn<'_>, b: u64) -> Result<&[u64], ReadError> {
        if self.block != Some(b) {
            self.block = None;
            let values = &mut self.values;
            values.clear();
            column.with_block(b, |block, _| {
                block.decode_into(values);
                Ok(())
            })?;
            self.block = Some(b);
        }
        Ok(&self.values)
    }

    /// As [`ForColumn::get`].
    #[inline]
    fn get(&mut self, column: &ForColumn<'_>, i: u64) -> Result<u64, ReadError> {
        // The hot path of a scan: the value is in the block already decoded.
        if self.block == Some(i / PER_BLOCK)
            && let Some(&v) = usize::try_from(i % PER_BLOCK)
                .ok()
                .and_then(|w| self.values.get(w))
        {
            return Ok(v);
        }
        self.get_slow(column, i)
    }

    #[inline(never)]
    fn get_slow(&mut self, column: &ForColumn<'_>, i: u64) -> Result<u64, ReadError> {
        if column.all.is_some() || i >= column.count {
            return column.get(i);
        }
        let within = usize::try_from(i % PER_BLOCK).unwrap_or(usize::MAX);
        // `with_block` checked the block holds what its place in the column says.
        self.load(column, i / PER_BLOCK)?
            .get(within)
            .copied()
            .ok_or_else(|| column.bad(column.at, "a FOR block count"))
    }

    /// As [`ForColumn::get_range`].
    #[inline]
    fn get_range(
        &mut self,
        column: &ForColumn<'_>,
        range: Range<u64>,
        out: &mut Vec<u64>,
    ) -> Result<(), ReadError> {
        // The hot path of a scan: the range is in the block already decoded.
        let b = range.start / PER_BLOCK;
        if self.block == Some(b)
            && range.start < range.end
            && range.end.saturating_sub(1) / PER_BLOCK == b
            && let Some(values) = usize::try_from(range.start % PER_BLOCK)
                .ok()
                .zip(usize::try_from(range.end.saturating_sub(1) % PER_BLOCK).ok())
                .and_then(|(from, last)| self.values.get(from..=last))
        {
            out.extend_from_slice(values);
            return Ok(());
        }
        self.get_range_slow(column, range, out)
    }

    #[inline(never)]
    fn get_range_slow(
        &mut self,
        column: &ForColumn<'_>,
        range: Range<u64>,
        out: &mut Vec<u64>,
    ) -> Result<(), ReadError> {
        if column.all.is_some() || range.end > column.count {
            return column.get_range(range, out);
        }
        let mut i = range.start;
        while i < range.end {
            let b = i / PER_BLOCK;
            let block_start = b.saturating_mul(PER_BLOCK);
            let to = range.end.min(block_start.saturating_add(PER_BLOCK));
            let values = self.load(column, b)?;
            let from = usize::try_from(i.saturating_sub(block_start)).unwrap_or(usize::MAX);
            let upto = usize::try_from(to.saturating_sub(block_start)).unwrap_or(usize::MAX);
            out.extend_from_slice(values.get(from..upto).unwrap_or_default());
            i = to;
        }
        Ok(())
    }
}

/// Reads a [`ForColumn`] through a [`ForBuf`] of its own.
#[derive(Debug, Clone)]
struct ForCursor<'c, 's> {
    column: &'c ForColumn<'s>,
    buf: ForBuf,
}

impl<'c, 's> ForCursor<'c, 's> {
    fn new(column: &'c ForColumn<'s>) -> Self {
        Self {
            column,
            buf: ForBuf::default(),
        }
    }

    /// As [`ForColumn::get`].
    #[inline]
    fn get(&mut self, i: u64) -> Result<u64, ReadError> {
        self.buf.get(self.column, i)
    }

    /// As [`ForColumn::get_range`].
    #[inline]
    fn get_range(&mut self, range: Range<u64>, out: &mut Vec<u64>) -> Result<(), ReadError> {
        self.buf.get_range(self.column, range, out)
    }
}

/// Bounds on the keys of a run of documents (ROADMAP 7.4.13, 7.5.6): each document of `start..end` with a value has a
/// key in `min..=max`. A segment's runs are its FOR blocks (SPEC §5.2.1), bounded by their headers: a block's minimum,
/// plus at most `2^width − 1`. Writers store the block's minimum for a document without a value and use the minimal
/// width, so `min` is the smallest key present and `max` below twice the span of the keys present above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyBlock {
    /// The run's first docid.
    pub start: u32,
    /// The docid after its last.
    pub end: u32,
    /// No key of the run is below it.
    pub min: u64,
    /// No key of the run is above it.
    pub max: u64,
}

/// How a run of keys bounded by `min..=max` meets what a scan selects ([`NumericColumn::select`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlap {
    /// No key in the bounds is selected: the run is not decoded.
    Disjoint,
    /// Every key in the bounds is: each document of the run with a value is selected, its key unread.
    Within,
    /// Some may be: each key is read.
    Partial,
}

/// The values of an `i64`, `f64` or `datetime` field (SPEC §5.3.5), read from their FOR blocks on demand.
#[derive(Debug, Clone)]
pub struct NumericColumn<'s> {
    field_type: FieldType,
    doc_count: u32,
    /// `None`: every document has a value.
    presence: Option<RoaringBitmap>,
    /// One order-preserving key per document; `None` when no document has a value.
    keys: Option<ForColumn<'s>>,
}

impl<'s> NumericColumn<'s> {
    pub(super) fn read(
        stream: Option<Stream<'s>>,
        field_type: FieldType,
        doc_count: u32,
        strict: bool,
    ) -> Result<Self, ReadError> {
        let Some(stream) = stream else {
            return Ok(Self {
                field_type,
                doc_count,
                presence: Some(RoaringBitmap::new()),
                keys: None,
            });
        };
        let mut p = Parts::new(stream);
        p.header(TAG_NUMERIC)?;
        let presence = p.bitmap(doc_count)?;
        let keys = ForColumn::open(&mut p, u64::from(doc_count), strict)?;
        p.finish()?;
        Ok(Self {
            field_type,
            doc_count,
            presence,
            keys: Some(keys),
        })
    }

    /// Whether `docid` has a value.
    pub fn has_value(&self, docid: u32) -> bool {
        let present = self.presence.as_ref().is_none_or(|p| p.contains(docid));
        present && docid < self.doc_count && self.keys.is_some()
    }

    /// The documents that have a value.
    ///
    /// # Errors
    /// [`ReadError::Corrupt`] when `doc_count` is above [`crate::segment::MAX_DOCS`]: the full-presence range is
    /// not built. A segment [`super::Segment::open`] accepted is within that cap.
    pub fn present(&self) -> Result<RoaringBitmap, ReadError> {
        match (&self.keys, &self.presence) {
            (None, _) => Ok(RoaringBitmap::new()),
            (Some(_), Some(present)) => Ok(present.clone()),
            // Length 0: every document (SPEC §5.3.5). The keys column was opened with `doc_count` values, so the
            // section holds them; the range itself is capped at `MAX_DOCS`.
            (Some(_), None) => every_doc(self.doc_count),
        }
    }

    /// `docid`'s order-preserving key (SPEC §5.2.1): unsigned order is value order, for range filters and sorting.
    ///
    /// # Errors
    /// A corruption if the FOR block that holds it is.
    pub fn key(&self, docid: u32) -> Result<Option<u64>, ReadError> {
        match &self.keys {
            Some(keys) if self.has_value(docid) => keys.get(u64::from(docid)).map(Some),
            _ => Ok(None),
        }
    }

    /// `docid`'s value, typed as the field.
    ///
    /// # Errors
    /// As [`Self::key`].
    pub fn get(&self, docid: u32) -> Result<Option<Value>, ReadError> {
        Ok(self.key(docid)?.map(|key| self.value(key)))
    }

    /// The documents with a value whose key `keep` accepts (ROADMAP 7.4.13). A FOR block is read only where
    /// `cover` of its bounds ([`KeyBlock`]) is [`Overlap::Partial`]; `step` runs once per block with a value and
    /// once per key read, so a search's interrupts stop a scan of a large column.
    ///
    /// # Errors
    /// A corruption of a block with a value: each is read and checked as a FOR block (SPEC §5.2.1), and only one
    /// [`Overlap::Partial`] is decoded, so a key past the type's range is found only there, as by a scan; what `step`
    /// returns.
    ///
    /// ```
    /// # use persia_analysis::AnalyzerRegistry;
    /// # use persia_engine::document::{Document, Value};
    /// # use persia_engine::schema::{FieldKind, SchemaBuilder};
    /// # use persia_engine::segment::{Overlap, Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    /// # use persia_format::ByteSource;
    /// # use persia_format::for_codec::i64_to_key;
    /// # let schema = SchemaBuilder::new("c").field("n", FieldKind::I64 { stored: false }).build()?;
    /// # let n = schema.field("n").map(|f| f.id()).ok_or("no field")?;
    /// # let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
    /// for (id, v) in [("a", 5), ("b", 50), ("c", 7)] {
    ///     builder.add(&Document::new(id).with(n, Value::I64(v)))?;
    /// }
    /// # let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
    /// # let segment = Segment::open(ByteSource::new(SegmentWriter::new(&builder, meta).finish()?.into()), &schema)?;
    /// let (low, high) = (i64_to_key(0), i64_to_key(9));
    /// let docs = segment.numeric_column(n)?.select(
    ///     // One block, keys 5..=50: some may match.
    ///     |min, max| if max < low || min > high { Overlap::Disjoint } else { Overlap::Partial },
    ///     |key| (low..=high).contains(&key),
    ///     || Ok(()),
    /// )?;
    /// assert_eq!(docs.iter().collect::<Vec<_>>(), [0, 2]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn select(
        &self,
        cover: impl Fn(u64, u64) -> Overlap,
        keep: impl Fn(u64) -> bool,
        step: impl Fn() -> Result<(), ReadError>,
    ) -> Result<RoaringBitmap, ReadError> {
        let Some(keys) = &self.keys else {
            return Ok(RoaringBitmap::new());
        };
        let present = self.present()?;
        let mut whole = RoaringBitmap::new();
        let mut some = Ascending::default();
        let mut buf = ForBuf::default();
        let mut docs = present.iter();
        while let Some(first) = docs.next() {
            step()?;
            let b = u64::from(first) / PER_BLOCK;
            // Within the column: the block holds `first`, below `doc_count`.
            let start = u32::try_from(b.saturating_mul(PER_BLOCK)).unwrap_or(u32::MAX);
            let end = start
                .saturating_add(u32::try_from(PER_BLOCK).unwrap_or(u32::MAX))
                .min(self.doc_count);
            let (min, max) = keys.bound(b)?;
            match cover(min, max) {
                Overlap::Disjoint => {}
                Overlap::Within => {
                    whole.insert_range(start..end);
                }
                Overlap::Partial => {
                    let values = buf.load(keys, b)?;
                    for docid in present.range(first..end) {
                        step()?;
                        // `with_block` checked the block holds what its place in the column says.
                        let at = usize::try_from(docid.saturating_sub(start)).unwrap_or(usize::MAX);
                        let key = values
                            .get(at)
                            .copied()
                            .ok_or_else(|| keys.bad(keys.at, "a FOR block count"))?;
                        if keep(key) {
                            some.push(docid);
                        }
                    }
                }
            }
            docs.advance_to(end);
        }
        whole &= &present;
        Ok(whole | some.finish())
    }

    /// A reader of the keys that owns the column (ROADMAP 7.5.1): as [`Self::cursor`], for a caller with nowhere to
    /// keep the column.
    pub fn into_keys(self) -> NumericKeys<'s> {
        NumericKeys {
            column: self,
            buf: ForBuf::default(),
            block: None,
        }
    }

    /// A reader for scans (filters, sorting): it keeps the FOR block it last decoded to itself, so reading the keys
    /// in docid order decodes each block once, without a lock or an allocation per value (ROADMAP 5.3.16).
    ///
    /// ```
    /// # use persia_analysis::AnalyzerRegistry;
    /// # use persia_engine::document::{Document, Value};
    /// # use persia_engine::schema::{FieldKind, SchemaBuilder};
    /// # use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    /// # use persia_format::ByteSource;
    /// # let schema = SchemaBuilder::new("c").field("n", FieldKind::I64 { stored: false }).build()?;
    /// # let n = schema.field("n").map(|f| f.id()).ok_or("no field")?;
    /// # let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
    /// # builder.add(&Document::new("a").with(n, Value::I64(-3)))?;
    /// # builder.add(&Document::new("b"))?;
    /// # let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
    /// # let segment = Segment::open(ByteSource::new(SegmentWriter::new(&builder, meta).finish()?.into()), &schema)?;
    /// let column = segment.numeric_column(n)?;
    /// let mut cursor = column.cursor();
    /// let values: Vec<_> = (0..segment.doc_count()).map(|d| cursor.get(d)).collect::<Result<_, _>>()?;
    /// assert_eq!(values, [Some(Value::I64(-3)), None]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn cursor(&self) -> NumericCursor<'_, 's> {
        NumericCursor {
            keys: self.keys.as_ref().map(ForCursor::new),
            column: self,
        }
    }

    /// `key` typed as the field.
    fn value(&self, key: u64) -> Value {
        match self.field_type {
            FieldType::F64 => Value::F64(key_to_f64(key)),
            FieldType::Datetime => Value::Datetime(key_to_i64(key)),
            _ => Value::I64(key_to_i64(key)),
        }
    }
}

/// A [`NumericColumn`] reader for scans, from [`NumericColumn::cursor`].
#[derive(Debug, Clone)]
pub struct NumericCursor<'c, 's> {
    column: &'c NumericColumn<'s>,
    keys: Option<ForCursor<'c, 's>>,
}

impl NumericCursor<'_, '_> {
    /// As [`NumericColumn::key`].
    ///
    /// # Errors
    /// As [`NumericColumn::key`].
    #[inline]
    pub fn key(&mut self, docid: u32) -> Result<Option<u64>, ReadError> {
        match &mut self.keys {
            Some(keys) if self.column.has_value(docid) => keys.get(u64::from(docid)).map(Some),
            _ => Ok(None),
        }
    }

    /// As [`NumericColumn::get`].
    ///
    /// # Errors
    /// As [`NumericColumn::key`].
    pub fn get(&mut self, docid: u32) -> Result<Option<Value>, ReadError> {
        Ok(self.key(docid)?.map(|key| self.column.value(key)))
    }
}

/// A [`NumericColumn`] reader that owns the column, from [`NumericColumn::into_keys`]: what a view hands a sort
/// (ROADMAP 7.5.1), which reads keys in docid order, with no column to borrow from.
#[derive(Debug, Clone)]
pub struct NumericKeys<'s> {
    column: NumericColumn<'s>,
    buf: ForBuf,
    /// The bounds [`Self::block`] read last.
    block: Option<KeyBlock>,
}

impl NumericKeys<'_> {
    /// As [`NumericColumn::key`].
    ///
    /// # Errors
    /// As [`NumericColumn::key`].
    #[inline]
    pub fn key(&mut self, docid: u32) -> Result<Option<u64>, ReadError> {
        match &self.column.keys {
            Some(keys) if self.column.has_value(docid) => {
                self.buf.get(keys, u64::from(docid)).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Bounds on the keys of the FOR block that holds `docid` (ROADMAP 7.5.6), from its header; `None` past the
    /// column or when no document has a value. The block is read and checked, not decoded. The last bounds read are
    /// kept, so asking along a block reads it once.
    ///
    /// # Errors
    /// A corruption of the block's header or position.
    pub fn block(&mut self, docid: u32) -> Result<Option<KeyBlock>, ReadError> {
        if let Some(block) = self.block
            && (block.start..block.end).contains(&docid)
        {
            return Ok(Some(block));
        }
        let Some(keys) = &self.column.keys else {
            return Ok(None);
        };
        if docid >= self.column.doc_count {
            return Ok(None);
        }
        let b = u64::from(docid) / PER_BLOCK;
        let (min, max) = keys.bound(b)?;
        let start = u32::try_from(b.saturating_mul(PER_BLOCK)).unwrap_or(u32::MAX);
        let block = KeyBlock {
            start,
            end: start
                .saturating_add(u32::try_from(PER_BLOCK).unwrap_or(u32::MAX))
                .min(self.column.doc_count),
            min,
            max,
        };
        self.block = Some(block);
        Ok(Some(block))
    }
}

/// The values of a `bool` field (SPEC §5.3.5).
#[derive(Debug, Clone)]
pub struct BoolColumn {
    /// `None`: every document has a value.
    presence: Option<RoaringBitmap>,
    trues: RoaringBitmap,
    doc_count: u32,
}

impl BoolColumn {
    pub(super) fn read(stream: Option<Stream<'_>>, doc_count: u32) -> Result<Self, ReadError> {
        let Some(stream) = stream else {
            return Ok(Self {
                presence: Some(RoaringBitmap::new()),
                trues: RoaringBitmap::new(),
                doc_count,
            });
        };
        let mut p = Parts::new(stream);
        p.header(TAG_BOOL)?;
        let presence = p.bitmap(doc_count)?;
        let at = p.at;
        let trues = p
            .bitmap(doc_count)?
            .ok_or_else(|| p.bad(at, "a bool column without its true bitmap"))?;
        if presence.as_ref().is_some_and(|pr| !trues.is_subset(pr)) {
            return Err(p.bad(at, "a true value for a document without one"));
        }
        p.finish()?;
        Ok(Self {
            presence,
            trues,
            doc_count,
        })
    }

    /// `docid`'s value.
    pub fn get(&self, docid: u32) -> Option<bool> {
        let present =
            docid < self.doc_count && self.presence.as_ref().is_none_or(|p| p.contains(docid));
        present.then(|| self.trues.contains(docid))
    }

    /// The documents that have a value.
    ///
    /// # Errors
    /// [`ReadError::Corrupt`] when `doc_count` is above [`crate::segment::MAX_DOCS`]: the full-presence range is
    /// not built. A segment [`super::Segment::open`] accepted is within that cap.
    pub fn present(&self) -> Result<RoaringBitmap, ReadError> {
        match &self.presence {
            Some(present) => Ok(present.clone()),
            // Length 0: every document (SPEC §5.3.5), a roaring run-range capped at `MAX_DOCS`.
            None => every_doc(self.doc_count),
        }
    }

    /// The documents whose value is `true`.
    pub fn trues(&self) -> &RoaringBitmap {
        &self.trues
    }
}

/// The values of a `keyword` field as doc values (SPEC §5.3.5): a dictionary, decoded and checked once per open
/// segment while the block cache keeps it (ROADMAP 7.5.5), and per document the ordinals of its distinct values,
/// ascending, read from their FOR blocks on demand.
#[derive(Debug, Clone, Default)]
pub struct KeywordColumn<'s> {
    dictionary: Arc<KeywordDictionary>,
    /// `doc_count + 1` starts into `ords`, and the ords; `None` when no document has a value.
    lists: Option<(ForColumn<'s>, ForColumn<'s>)>,
}

/// A `keyword` column's distinct values in byte order, checked strictly increasing.
#[derive(Debug, Default)]
struct KeywordDictionary {
    /// The values, back to back; value `i` ends at `value_ends[i]`.
    values: Vec<u8>,
    value_ends: Vec<usize>,
}

/// What a cached dictionary costs besides its bytes: the `Arc`, the two vectors' headers, and the cache entry.
const DICTIONARY_OVERHEAD: u64 = 128;

impl KeywordDictionary {
    /// The dictionary at `p` (its count, the value ends, then the values), which must end at `end`; and what it
    /// holds in memory, in bytes.
    fn read(p: &mut Parts<'_>, end: u64) -> Result<(Self, u64), ReadError> {
        let stream = p.stream;
        let at = p.at;
        let count = p.u64()?;
        let remaining = stream.len().saturating_sub(p.at);
        if count == 0 || count.saturating_mul(8) > remaining {
            return Err(p.bad(at, "a keyword dictionary count"));
        }
        let ends = p.bytes(count.saturating_mul(8))?;
        let mut value_ends = Vec::new();
        value_ends
            .try_reserve_exact(ends.len() / 8)
            .map_err(|_| stream.error(persia_format::Error::OutOfMemory { bytes: None }))?;
        for end in ends.as_chunks::<8>().0 {
            let end =
                usize::try_from(u64::from_le_bytes(*end)).map_err(|_| p.bad(at, "a value end"))?;
            value_ends.push(end);
        }
        let total = value_ends.last().copied().unwrap_or(0);
        let values = p.bytes(u64::try_from(total).unwrap_or(u64::MAX))?;
        let mut start = 0;
        let mut prev: Option<&[u8]> = None;
        for &end in &value_ends {
            let value = values
                .get(start..end)
                .ok_or_else(|| p.bad(at, "keyword value ends that decrease"))?;
            if prev.is_some_and(|prev| prev >= value) {
                return Err(p.bad(at, "keyword values not strictly increasing"));
            }
            prev = Some(value);
            start = end;
        }
        if p.at != end {
            return Err(p.bad(at, "starts_off"));
        }
        let held = values
            .capacity()
            .saturating_add(value_ends.capacity().saturating_mul(size_of::<usize>()));
        let cost = u64::try_from(held)
            .unwrap_or(u64::MAX)
            .saturating_add(DICTIONARY_OVERHEAD);
        Ok((Self { values, value_ends }, cost))
    }
}

impl<'s> KeywordColumn<'s> {
    pub(super) fn read(
        stream: Option<Stream<'s>>,
        doc_count: u32,
        strict: bool,
    ) -> Result<Self, ReadError> {
        let Some(stream) = stream else {
            return Ok(Self::default());
        };
        let mut p = Parts::new(stream);
        p.header(TAG_KEYWORD)?;
        let starts_off = p.u64()?;
        let ords_off = p.u64()?;
        // The dictionary spans the header to `starts_off`. A segment's bytes do not change, so one decoded and
        // checked before is the same for this open segment; `verify` (strict) reads it again.
        let at = p.at;
        let mut dict = Parts { stream, at };
        let dictionary = if strict {
            Arc::new(KeywordDictionary::read(&mut dict, starts_off)?.0)
        } else {
            stream.cached_item(Item::KeywordDictionary, || {
                KeywordDictionary::read(&mut dict, starts_off)
            })?
        };
        let skip = starts_off
            .checked_sub(p.at)
            .ok_or_else(|| p.bad(p.at, "starts_off"))?;
        p.skip(skip)?;
        let docs = u64::from(doc_count);
        let mut starts = ForColumn::open(&mut p, docs.saturating_add(1), strict)?;
        if p.at != ords_off {
            return Err(p.bad(at, "ords_off"));
        }
        let ord_count = starts.get(docs)?;
        if starts.get(0)? != 0 {
            return Err(starts.bad(starts.at, "keyword starts that decrease"));
        }
        let mut ords = ForColumn::open(&mut p, ord_count, strict)?;
        p.finish()?;
        if strict {
            // `verify` reads a document's ords once per posting, in term order: decode them once.
            starts.preload()?;
            ords.preload()?;
        }
        let column = Self {
            dictionary,
            lists: Some((starts, ords)),
        };
        if strict {
            for d in 0..doc_count {
                column.ords(d)?;
            }
        }
        Ok(column)
    }

    /// Number of distinct values.
    pub fn len(&self) -> u64 {
        u64::try_from(self.dictionary.value_ends.len()).unwrap_or(u64::MAX)
    }

    /// Whether no document has a value.
    pub fn is_empty(&self) -> bool {
        self.dictionary.value_ends.is_empty()
    }

    /// The value with ordinal `ord` (its rank in byte order).
    pub fn value(&self, ord: u64) -> Option<&[u8]> {
        let i = usize::try_from(ord).ok()?;
        let dictionary = &*self.dictionary;
        let end = *dictionary.value_ends.get(i)?;
        let start = match i.checked_sub(1) {
            Some(p) => *dictionary.value_ends.get(p)?,
            None => 0,
        };
        dictionary.values.get(start..end)
    }

    /// The ordinal of `value`, if a document has it.
    pub fn ord(&self, value: &[u8]) -> Option<u64> {
        let (mut lo, mut hi) = (0_u64, self.len());
        while lo < hi {
            let mid = lo.midpoint(hi);
            match self.value(mid)?.cmp(value) {
                core::cmp::Ordering::Less => lo = mid.saturating_add(1),
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// The ordinals of `docid`'s distinct values, ascending; empty for a document without one (or a docid past the
    /// segment).
    ///
    /// # Errors
    /// A corruption if the FOR blocks that hold them are, or if they do not increase or pass the dictionary.
    pub fn ords(&self, docid: u32) -> Result<Vec<u64>, ReadError> {
        let mut out = Vec::new();
        if let Some((starts, ords)) = &self.lists {
            self.ords_into(
                docid,
                (starts, |i| starts.get(i)),
                (ords, |r, out| ords.get_range(r, out)),
                &mut out,
            )?;
        }
        Ok(out)
    }

    /// A reader of the ords that owns the column (ROADMAP 7.5.1, 7.5.3): as [`Self::cursor`], for a caller with
    /// nowhere to keep the column.
    pub fn into_ords(self) -> KeywordOrds<'s> {
        KeywordOrds {
            column: self,
            starts: ForBuf::default(),
            ords: ForBuf::default(),
            out: Vec::new(),
        }
    }

    /// A reader for scans (filters, facets): it keeps the FOR blocks it last decoded and its output buffer to itself,
    /// so reading every document's ords in docid order decodes each block once, without a lock or an allocation per
    /// document (ROADMAP 5.3.16).
    ///
    /// ```
    /// # use persia_analysis::AnalyzerRegistry;
    /// # use persia_engine::document::{Document, Value};
    /// # use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder};
    /// # use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    /// # use persia_format::ByteSource;
    /// # let schema = SchemaBuilder::new("c").field("tag", FieldKind::Keyword(KeywordOptions::default())).build()?;
    /// # let tag = schema.field("tag").map(|f| f.id()).ok_or("no field")?;
    /// # let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
    /// # builder.add(&Document::new("a").with(tag, Value::Strings(vec!["x".into(), "w".into()])))?;
    /// # builder.add(&Document::new("b"))?;
    /// # let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
    /// # let segment = Segment::open(ByteSource::new(SegmentWriter::new(&builder, meta).finish()?.into()), &schema)?;
    /// let column = segment.keyword_column(tag)?;
    /// let mut cursor = column.cursor();
    /// assert_eq!(cursor.ords(0)?, [0, 1]); // "w", "x"
    /// assert_eq!(cursor.ords(1)?, [0_u64; 0]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn cursor(&self) -> KeywordCursor<'_, 's> {
        KeywordCursor {
            column: self,
            lists: self
                .lists
                .as_ref()
                .map(|(s, o)| (ForCursor::new(s), ForCursor::new(o))),
            out: Vec::new(),
        }
    }

    /// Appends `docid`'s ords to `out` (empty first), reading the `starts` and `ords` FOR columns through `start_at`
    /// and `ords_in`, and checks them.
    fn ords_into(
        &self,
        docid: u32,
        (starts, mut start_at): (&ForColumn<'_>, impl FnMut(u64) -> Result<u64, ReadError>),
        (ords, ords_in): (
            &ForColumn<'_>,
            impl FnOnce(Range<u64>, &mut Vec<u64>) -> Result<(), ReadError>,
        ),
        out: &mut Vec<u64>,
    ) -> Result<(), ReadError> {
        out.clear();
        let doc = u64::from(docid);
        if doc >= starts.count.saturating_sub(1) {
            return Ok(());
        }
        let first = start_at(doc)?;
        let end = start_at(doc.saturating_add(1))?;
        // A document's ords are distinct values of the dictionary: at most `len` of them.
        if first > end || end > ords.count || end.saturating_sub(first) > self.len() {
            return Err(starts.bad(starts.at, "keyword starts that decrease, pass the ords, or span more ords than the dictionary holds",
            ));
        }
        // At most `len` values (checked above), but the dictionary may be large: fail cleanly rather than abort.
        let n = usize::try_from(end.saturating_sub(first)).unwrap_or(usize::MAX);
        out.try_reserve(n).map_err(|_| {
            ords.stream.error(persia_format::Error::OutOfMemory {
                bytes: end.saturating_sub(first).checked_mul(8),
            })
        })?;
        ords_in(first..end, out)?;
        let increasing = out.windows(2).all(|o| matches!(o, [x, y] if x < y));
        if !increasing || out.last().is_some_and(|&o| o >= self.len()) {
            return Err(ords.bad(
                ords.at,
                "a document's ords not increasing or past the dictionary",
            ));
        }
        Ok(())
    }

    /// `docid`'s distinct values, ascending.
    ///
    /// # Errors
    /// As [`Self::ords`].
    pub fn values(&self, docid: u32) -> Result<Vec<&[u8]>, ReadError> {
        Ok(self
            .ords(docid)?
            .into_iter()
            .filter_map(|o| self.value(o))
            .collect())
    }
}

/// A [`KeywordColumn`] reader for scans, from [`KeywordColumn::cursor`].
#[derive(Debug, Clone)]
pub struct KeywordCursor<'c, 's> {
    column: &'c KeywordColumn<'s>,
    /// Cursors over the `starts` and the ords.
    lists: Option<(ForCursor<'c, 's>, ForCursor<'c, 's>)>,
    out: Vec<u64>,
}

impl<'c> KeywordCursor<'c, '_> {
    /// As [`KeywordColumn::ords`], into a buffer the cursor reuses.
    ///
    /// # Errors
    /// As [`KeywordColumn::ords`].
    #[inline]
    pub fn ords(&mut self, docid: u32) -> Result<&[u64], ReadError> {
        let column = self.column;
        self.out.clear();
        if let Some((starts, ords)) = &mut self.lists {
            let (s, o) = (starts.column, ords.column);
            column.ords_into(
                docid,
                (s, |i| starts.get(i)),
                (o, |r, out| ords.get_range(r, out)),
                &mut self.out,
            )?;
        }
        Ok(&self.out)
    }

    /// The column it reads, for [`KeywordColumn::value`].
    pub fn column(&self) -> &'c KeywordColumn<'_> {
        self.column
    }
}

/// A [`KeywordColumn`] reader that owns the column, from [`KeywordColumn::into_ords`]: what a view hands a sort or a
/// facet (ROADMAP 7.5.1, 7.5.3), as [`KeywordCursor`] without a column to borrow from.
#[derive(Debug, Clone)]
pub struct KeywordOrds<'s> {
    column: KeywordColumn<'s>,
    starts: ForBuf,
    ords: ForBuf,
    out: Vec<u64>,
}

impl<'s> KeywordOrds<'s> {
    /// As [`KeywordColumn::ords`], into a buffer the reader reuses.
    ///
    /// # Errors
    /// As [`KeywordColumn::ords`].
    #[inline]
    pub fn ords(&mut self, docid: u32) -> Result<&[u64], ReadError> {
        let Self {
            column,
            starts: starts_buf,
            ords: ords_buf,
            out,
        } = self;
        out.clear();
        if let Some((starts, ords)) = &column.lists {
            column.ords_into(
                docid,
                (starts, |i| starts_buf.get(starts, i)),
                (ords, |r, o| ords_buf.get_range(ords, r, o)),
                out,
            )?;
        }
        Ok(out)
    }

    /// The column it reads, for [`KeywordColumn::value`].
    pub fn column(&self) -> &KeywordColumn<'s> {
        &self.column
    }
}

/// Whether `block` holds its true minimum and the bit length of its largest offset (SPEC §5.2.1: what writers do).
fn minimal(block: &ForBlock<'_>) -> bool {
    let (min, max) = block
        .iter()
        .fold((u64::MAX, 0), |(lo, hi), k| (lo.min(k), hi.max(k)));
    let width = u8::try_from(max.wrapping_sub(min).bit_width()).unwrap_or(u8::MAX);
    min == block.min() && width == block.width()
}

/// Docids `0..doc_count`: what a presence bitmap of length 0 stands for (SPEC §5.3.5). One roaring run per 2^16
/// docids. A count above [`crate::segment::MAX_DOCS`] is corruption and builds nothing.
///
/// # Errors
/// As [`crate::bitmap::doc_range`].
pub(crate) fn every_doc(doc_count: u32) -> Result<RoaringBitmap, ReadError> {
    crate::bitmap::doc_range(doc_count)
}

#[cfg(test)]
mod tests {
    //! Cursors over columns opened strictly (ROADMAP 5.3.16): `Segment::verify` opens keyword columns with every
    //! value preloaded, a path no public reader reaches, so it is checked here against the documents.
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::too_many_lines
    )]

    use std::collections::BTreeSet;

    use persia_analysis::AnalyzerRegistry;
    use persia_format::ByteSource;
    use persia_format::for_codec::{f64_to_key, i64_to_key};

    use super::*;
    use crate::document::Document;
    use crate::schema::{FieldKind, KeywordOptions, SchemaBuilder};
    use crate::segment::{SectionKind, Segment, SegmentBuilder, SegmentMeta, SegmentWriter};

    const DOCS: u32 = 2600;

    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state >> 33
    }

    /// Per document: the `i64` value (some absent), the `f64` value (always), the keyword values (0..=3, with
    /// duplicates). Three FOR blocks of keys, of starts, and at least three of ords.
    fn documents() -> Vec<(Option<i64>, f64, Vec<String>)> {
        let mut state = 0x00c0_ffee;
        (0..DOCS)
            .map(|i| {
                let n = (!lcg(&mut state).is_multiple_of(5))
                    .then(|| lcg(&mut state).cast_signed() - (1 << 30));
                // Never -0.0, which documents store as 0.0.
                let f = f64::from(i) * -0.5 - 1.0;
                let k = (0..lcg(&mut state) % 4)
                    .map(|_| format!("v{:02}", lcg(&mut state) % 40))
                    .collect();
                (n, f, k)
            })
            .collect()
    }

    /// Docids in a scrambled order, the FOR block edges and docids past the segment, each twice.
    fn order() -> Vec<u32> {
        let mut state = 99;
        let mut order: Vec<u32> = (0..DOCS + 3).collect();
        for i in (1..order.len()).rev() {
            order.swap(i, (lcg(&mut state) as usize) % (i + 1));
        }
        order.extend([1023, 1024, 1025, 2047, 2048, 2049, DOCS - 1, DOCS, u32::MAX]);
        let again: Vec<u32> = order.iter().rev().copied().collect();
        order.extend(again);
        order
    }

    #[test]
    fn cursors_over_strict_and_preloaded_columns_equal_the_documents() {
        let schema = SchemaBuilder::new("c")
            .field("n", FieldKind::I64 { stored: false })
            .field("f", FieldKind::F64 { stored: false })
            .field("k", FieldKind::Keyword(KeywordOptions::default()))
            .build()
            .unwrap();
        let fid = |name: &str| schema.field(name).unwrap().id();
        let (n, f, k) = (fid("n"), fid("f"), fid("k"));
        let docs = documents();
        let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        for (i, (nv, fv, kv)) in docs.iter().enumerate() {
            let mut d = Document::new(format!("d{i:05}")).with(f, Value::F64(*fv));
            if let Some(nv) = nv {
                d = d.with(n, Value::I64(*nv));
            }
            if !kv.is_empty() {
                d = d.with(k, Value::Strings(kv.clone()));
            }
            builder.add(&d).unwrap();
        }
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let bytes = SegmentWriter::new(&builder, meta).finish().unwrap();
        let seg = Segment::open(ByteSource::new(bytes.into()), &schema).unwrap();
        let stream =
            |field: crate::schema::FieldId| seg.stream(SectionKind::Columns, field.get()).unwrap();

        // Keyword: the lazy column (what queries open) and the preloaded one (what `verify` opens).
        let lazy = seg.keyword_column(k).unwrap();
        let strict = KeywordColumn::read(stream(k), DOCS, true).unwrap();
        let (starts, ords) = strict.lists.as_ref().unwrap();
        assert!(
            starts.all.is_some() && ords.all.is_some(),
            "the strict column is preloaded"
        );
        assert!(
            ords.count.div_ceil(PER_BLOCK) >= 3,
            "ords span three FOR blocks"
        );
        let expected_ords = |d: u32| -> Vec<u64> {
            let Some((_, _, values)) = docs.get(d as usize) else {
                return Vec::new();
            };
            let distinct: BTreeSet<&[u8]> = values.iter().map(String::as_bytes).collect();
            distinct.into_iter().map(|v| lazy.ord(v).unwrap()).collect()
        };
        let (mut lazy_cursor, mut strict_cursor) = (lazy.cursor(), strict.cursor());

        // Numeric: lazily and strictly opened (strict checks every block's minimality; it does not preload).
        let numeric = [(n, FieldType::I64), (f, FieldType::F64)];
        let opened: Vec<_> = numeric
            .iter()
            .map(|&(field, ty)| {
                (
                    seg.numeric_column(field).unwrap(),
                    NumericColumn::read(stream(field), ty, DOCS, true).unwrap(),
                )
            })
            .collect();
        let mut cursors: Vec<_> = opened
            .iter()
            .map(|(l, s)| (l.cursor(), s.cursor()))
            .collect();
        let expected_key = |field: usize, d: u32| -> Option<u64> {
            let (nv, fv, _) = docs.get(d as usize)?;
            if field == 0 {
                nv.map(i64_to_key)
            } else {
                Some(f64_to_key(*fv))
            }
        };

        for d in order() {
            let want = expected_ords(d);
            assert_eq!(
                lazy_cursor.ords(d).unwrap(),
                want.as_slice(),
                "lazy cursor, doc {d}"
            );
            assert_eq!(
                strict_cursor.ords(d).unwrap(),
                want.as_slice(),
                "preloaded cursor, doc {d}"
            );
            assert_eq!(strict.ords(d).unwrap(), want, "preloaded column, doc {d}");
            assert_eq!(lazy.ords(d).unwrap(), want, "lazy column, doc {d}");
            for (i, ((lc, sc), (lazy, strict))) in cursors.iter_mut().zip(&opened).enumerate() {
                let want = expected_key(i, d);
                assert_eq!(lc.key(d).unwrap(), want, "field {i} lazy cursor, doc {d}");
                assert_eq!(sc.key(d).unwrap(), want, "field {i} strict cursor, doc {d}");
                assert_eq!(lazy.key(d).unwrap(), want, "field {i} lazy, doc {d}");
                assert_eq!(strict.key(d).unwrap(), want, "field {i} strict, doc {d}");
            }
        }
        // The values behind the ords are the documents' own.
        for d in [0, 1500, DOCS - 1] {
            let mut want: Vec<&[u8]> = docs[d as usize].2.iter().map(String::as_bytes).collect();
            want.sort_unstable();
            want.dedup();
            let ords = strict_cursor.ords(d).unwrap().to_vec();
            let got: Vec<&[u8]> = ords
                .iter()
                .map(|&o| strict_cursor.column().value(o).unwrap())
                .collect();
            assert_eq!(got, want, "doc {d}");
        }
    }

    /// A segment with `k` a keyword field of many distinct values, opened with `cache`.
    fn keyword_segment(
        cache: &Arc<crate::segment::BlockCache>,
    ) -> (Segment, crate::schema::FieldId) {
        let schema = SchemaBuilder::new("c")
            .field("k", FieldKind::Keyword(KeywordOptions::default()))
            .build()
            .unwrap();
        let k = schema.field("k").unwrap().id();
        let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        for i in 0..300_u32 {
            let values = vec![format!("v{:03}", i % 97), format!("w{:03}", i % 13)];
            builder
                .add(&Document::new(format!("d{i}")).with(k, Value::Strings(values)))
                .unwrap();
        }
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let bytes = SegmentWriter::new(&builder, meta).finish().unwrap();
        let source = ByteSource::new(bytes.into());
        let segment = Segment::open_with_cache(source, &schema, Arc::clone(cache)).unwrap();
        (segment, k)
    }

    /// ROADMAP 7.5.5: a keyword column's dictionary is decoded once per open segment while the block cache keeps it,
    /// and the columns over it read as one decoded each time; a cache too small for it decodes it per open, and a
    /// dropped segment's dictionary leaves the cache.
    #[test]
    fn a_keyword_dictionary_is_decoded_once_while_cached() {
        let cache = Arc::new(crate::segment::BlockCache::new(1 << 20));
        let (segment, k) = keyword_segment(&cache);
        let first = segment.keyword_column(k).unwrap();
        let again = segment.keyword_column(k).unwrap();
        assert!(Arc::ptr_eq(&first.dictionary, &again.dictionary));
        assert_eq!(first.len(), 97 + 13);
        let strict = KeywordColumn::read(
            segment.stream(SectionKind::Columns, k.get()).unwrap(),
            segment.doc_count(),
            true,
        )
        .unwrap();
        // `verify`'s strict open reads its own copy, equal to the cached one.
        assert!(!Arc::ptr_eq(&strict.dictionary, &first.dictionary));
        for d in 0..segment.doc_count() {
            assert_eq!(again.values(d).unwrap(), strict.values(d).unwrap(), "{d}");
        }
        let cached = cache.stats().bytes;
        let dictionary_bytes = u64::try_from(first.dictionary.values.capacity()).unwrap();
        assert!(cached >= dictionary_bytes, "{cached} cached");
        drop((first, again, strict));
        drop(segment);
        assert_eq!(cache.stats().bytes, 0, "the segment's entries left with it");

        let tiny = Arc::new(crate::segment::BlockCache::new(0));
        let (segment, k) = keyword_segment(&tiny);
        let a = segment.keyword_column(k).unwrap();
        let b = segment.keyword_column(k).unwrap();
        assert!(!Arc::ptr_eq(&a.dictionary, &b.dictionary));
        for d in 0..segment.doc_count() {
            assert_eq!(a.values(d).unwrap(), b.values(d).unwrap());
        }
    }

    /// `n` by FOR block (ROADMAP 7.4.13, 7.5.6): a narrow band, a band with absent values, the two ends of the key
    /// space (width 64), one constant (width 0), and a short last block of a band again.
    fn banded(i: u32, state: &mut u64) -> Option<i64> {
        let r = i64::try_from(lcg(state) % 100).unwrap();
        match i / 1024 {
            0 => Some(r),
            1 => (!i.is_multiple_of(3)).then_some(1_000 + r),
            2 => Some(if i.is_multiple_of(2) {
                i64::MIN
            } else {
                i64::MAX
            }),
            3 => Some(7),
            _ => Some(-5_000 + r),
        }
    }

    const BANDED: u32 = 4 * 1024 + 300;

    fn banded_segment() -> (Segment, crate::schema::FieldId, Vec<Option<u64>>) {
        let schema = SchemaBuilder::new("c")
            .field("n", FieldKind::I64 { stored: false })
            .build()
            .unwrap();
        let n = schema.field("n").unwrap().id();
        let mut state = 0x7413;
        let keys: Vec<Option<i64>> = (0..BANDED).map(|i| banded(i, &mut state)).collect();
        let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        for (i, v) in keys.iter().enumerate() {
            let mut d = Document::new(format!("d{i:05}"));
            if let Some(v) = v {
                d = d.with(n, Value::I64(*v));
            }
            builder.add(&d).unwrap();
        }
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let bytes = SegmentWriter::new(&builder, meta).finish().unwrap();
        let seg = Segment::open(ByteSource::new(bytes.into()), &schema).unwrap();
        (
            seg,
            n,
            keys.into_iter().map(|v| v.map(i64_to_key)).collect(),
        )
    }

    #[test]
    fn block_bounds_hold_every_key_of_their_block() {
        let (seg, n, keys) = banded_segment();
        let mut reader = seg.numeric_column(n).unwrap().into_keys();
        for d in order() {
            let block = reader.block(d).unwrap();
            if d >= BANDED {
                assert_eq!(block, None, "doc {d}");
                continue;
            }
            let block = block.unwrap();
            let start = d / 1024 * 1024;
            assert_eq!(
                (block.start, block.end),
                (start, (start + 1024).min(BANDED))
            );
            let present: Vec<u64> = keys[start as usize..block.end as usize]
                .iter()
                .flatten()
                .copied()
                .collect();
            // Writers use the true minimum and the minimal width (SPEC §5.2.1).
            assert_eq!(block.min, *present.iter().min().unwrap(), "doc {d}");
            let max = *present.iter().max().unwrap();
            assert!(block.max >= max, "doc {d}");
            // The minimal width: the span is below twice the true one, plus one.
            assert!(
                block.max - block.min <= (max - block.min).saturating_mul(2).saturating_add(1),
                "doc {d}"
            );
            // The key itself, read after the bounds, is the document's.
            assert_eq!(reader.key(d).unwrap(), keys[d as usize]);
        }
        let mut reader = seg.numeric_column(n).unwrap().into_keys();
        let constant = reader.block(3 * 1024).unwrap().unwrap();
        assert_eq!((constant.min, constant.max), (i64_to_key(7), i64_to_key(7)));
        let ends = reader.block(2 * 1024).unwrap().unwrap();
        assert_eq!((ends.min, ends.max), (0, u64::MAX));
    }

    #[test]
    fn a_select_skipping_blocks_equals_a_scan_of_every_key() {
        let (seg, n, keys) = banded_segment();
        let column = seg.numeric_column(n).unwrap();
        let ok = || Ok(());
        let between = |low: u64, high: u64| {
            move |min: u64, max: u64| {
                if max < low || min > high {
                    Overlap::Disjoint
                } else if low <= min && max <= high {
                    Overlap::Within
                } else {
                    Overlap::Partial
                }
            }
        };
        let k = i64_to_key;
        for (low, high) in [
            (0, u64::MAX),
            (k(0), k(99)),
            (k(10), k(1_050)),
            (k(1_000), k(1_099)),
            (k(7), k(7)),
            (k(8), k(999)),
            (u64::MAX, u64::MAX),
            (0, 0),
            (k(-5_000), k(-4_950)),
            (k(-4_990), k(5)),
        ] {
            let want: RoaringBitmap = keys
                .iter()
                .enumerate()
                .filter(|(_, key)| key.is_some_and(|key| (low..=high).contains(&key)))
                .map(|(d, _)| u32::try_from(d).unwrap())
                .collect();
            let got = column
                .select(between(low, high), |key| (low..=high).contains(&key), ok)
                .unwrap();
            assert_eq!(got, want, "{low}..={high}");
        }
        // `Within` selects what has a value, keys unread; `Disjoint` nothing, whatever `keep` says.
        let present = column.present().unwrap();
        assert_eq!(present.len(), keys.iter().flatten().count() as u64);
        let within = column
            .select(|_, _| Overlap::Within, |_| false, ok)
            .unwrap();
        assert_eq!(within, present);
        let disjoint = column
            .select(|_, _| Overlap::Disjoint, |_| true, ok)
            .unwrap();
        assert!(disjoint.is_empty());
        // Steps count the work: one per block with a value, one per key of a block decoded. `n == 7` decodes only the
        // blocks it cuts, the first (0..=99) and the third (both ends of the key space), of 1 024 values each, and takes
        // the fourth whole (ROADMAP 7.4.13).
        let steps = core::cell::Cell::new(0_u32);
        let seven = k(7);
        let got = column
            .select(
                between(seven, seven),
                |key| key == seven,
                || {
                    steps.set(steps.get() + 1);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(
            got.len(),
            1024 + keys[..1024].iter().filter(|k| **k == Some(seven)).count() as u64
        );
        assert_eq!(steps.get(), 5 + 2 * 1024);
        // A step's error stops the scan.
        let stopped = column.select(
            |_, _| Overlap::Partial,
            |_| true,
            || {
                Err(ReadError::DocOutOfRange {
                    docid: 0,
                    doc_count: 0,
                })
            },
        );
        assert!(stopped.is_err());
    }
}
