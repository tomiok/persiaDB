//! Typed readers of `Norms` and `Columns` (SPEC §5.3.4, §5.3.5; ROADMAP 5.3.5, 5.3.12). Norms, bool bitmaps,
//! presence bitmaps and a keyword column's dictionary are decoded when the reader is opened; numeric keys and keyword
//! ords stay in their FOR blocks, read one block at a time through the segment's block cache (SPEC §5.2.1 makes any
//! value O(1)), so a reader's memory does not grow with the document count.

use core::ops::Range;

use persia_format::for_codec::{ForBlock, key_to_f64, key_to_i64};
use roaring::RoaringBitmap;

use super::super::norms;
use super::super::write::columns::{TAG_BOOL, TAG_KEYWORD, TAG_NUMERIC};
use super::error::{ReadError, SegmentCorruption};
use super::stream::Stream;
use crate::document::Value;
use crate::schema::FieldType;

/// The quantized field lengths of a `text` field (SPEC §5.3.4).
#[derive(Debug, Clone, Default)]
pub struct Norms {
    /// One byte per document; empty when the field has no tokens in the segment.
    bytes: Vec<u8>,
}

impl Norms {
    pub(super) fn read(stream: Option<Stream<'_>>, doc_count: u32) -> Result<Self, ReadError> {
        let Some(stream) = stream else {
            return Ok(Self::default());
        };
        if stream.len() != u64::from(doc_count) {
            return Err(stream.corrupt(
                0,
                SegmentCorruption::Invalid {
                    what: "a Norms stream other than doc_count bytes",
                },
            ));
        }
        Ok(Self {
            bytes: stream.read_all()?,
        })
    }

    /// The norm byte of `docid`; 0 for a document without tokens in the field (or a docid past the segment).
    pub fn get(&self, docid: u32) -> u8 {
        usize::try_from(docid)
            .ok()
            .and_then(|d| self.bytes.get(d))
            .copied()
            .unwrap_or(0)
    }

    /// The field length `docid`'s norm stands for: exact below 40 tokens, otherwise rounded down by less than
    /// 12.5 % (SPEC §5.3.4).
    pub fn length(&self, docid: u32) -> u32 {
        norms::decode(self.get(docid))
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

    /// Value `i`, which must be below the count.
    fn get(&self, i: u64) -> Result<u64, ReadError> {
        if i >= self.count {
            return Err(self.bad(self.at, "a FOR value past its column"));
        }
        let within = usize::try_from(i % PER_BLOCK).unwrap_or(usize::MAX);
        self.with_block(i / PER_BLOCK, |block, at| {
            block
                .get(within)
                .ok_or_else(|| self.bad(at, "a FOR block count"))
        })
    }

    /// Values `range` appended to `out`; the range must be within the count.
    fn get_range(&self, range: Range<u64>, out: &mut Vec<u64>) -> Result<(), ReadError> {
        if range.end > self.count {
            return Err(self.bad(self.at, "a FOR value past its column"));
        }
        let mut i = range.start;
        while i < range.end {
            let b = i / PER_BLOCK;
            let block_start = b.saturating_mul(PER_BLOCK);
            let to = range.end.min(block_start.saturating_add(PER_BLOCK));
            self.with_block(b, |block, _| {
                let from = usize::try_from(i.saturating_sub(block_start)).unwrap_or(usize::MAX);
                let upto = usize::try_from(to.saturating_sub(block_start)).unwrap_or(usize::MAX);
                out.extend((from..upto).filter_map(|j| block.get(j)));
                Ok(())
            })?;
            i = to;
        }
        Ok(())
    }
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
        Ok(self.key(docid)?.map(|key| match self.field_type {
            FieldType::F64 => Value::F64(key_to_f64(key)),
            FieldType::Datetime => Value::Datetime(key_to_i64(key)),
            _ => Value::I64(key_to_i64(key)),
        }))
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

    /// The documents whose value is `true`.
    pub fn trues(&self) -> &RoaringBitmap {
        &self.trues
    }
}

/// The values of a `keyword` field as doc values (SPEC §5.3.5): a dictionary, decoded when the column is opened, and
/// per document the ordinals of its distinct values, ascending, read from their FOR blocks on demand.
#[derive(Debug, Clone, Default)]
pub struct KeywordColumn<'s> {
    /// The values, back to back; value `i` ends at `value_ends[i]`.
    values: Vec<u8>,
    value_ends: Vec<usize>,
    /// `doc_count + 1` starts into `ords`, and the ords; `None` when no document has a value.
    lists: Option<(ForColumn<'s>, ForColumn<'s>)>,
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
        if p.at != starts_off {
            return Err(p.bad(at, "starts_off"));
        }
        let docs = u64::from(doc_count);
        let starts = ForColumn::open(&mut p, docs.saturating_add(1), strict)?;
        if p.at != ords_off {
            return Err(p.bad(at, "ords_off"));
        }
        let ord_count = starts.get(docs)?;
        if starts.get(0)? != 0 {
            return Err(starts.bad(starts.at, "keyword starts that decrease"));
        }
        let ords = ForColumn::open(&mut p, ord_count, strict)?;
        p.finish()?;
        let column = Self {
            values,
            value_ends,
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
        u64::try_from(self.value_ends.len()).unwrap_or(u64::MAX)
    }

    /// Whether no document has a value.
    pub fn is_empty(&self) -> bool {
        self.value_ends.is_empty()
    }

    /// The value with ordinal `ord` (its rank in byte order).
    pub fn value(&self, ord: u64) -> Option<&[u8]> {
        let i = usize::try_from(ord).ok()?;
        let end = *self.value_ends.get(i)?;
        let start = match i.checked_sub(1) {
            Some(p) => *self.value_ends.get(p)?,
            None => 0,
        };
        self.values.get(start..end)
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
        let Some((starts, ords)) = &self.lists else {
            return Ok(Vec::new());
        };
        let doc = u64::from(docid);
        if doc >= starts.count.saturating_sub(1) {
            return Ok(Vec::new());
        }
        let first = starts.get(doc)?;
        let end = starts.get(doc.saturating_add(1))?;
        // A document's ords are distinct values of the dictionary: at most `len` of them.
        if first > end || end > ords.count || end.saturating_sub(first) > self.len() {
            return Err(starts.bad(starts.at, "keyword starts that decrease"));
        }
        let mut out = Vec::new();
        ords.get_range(first..end, &mut out)?;
        let increasing = out.windows(2).all(|o| matches!(o, [x, y] if x < y));
        if !increasing || out.last().is_some_and(|&o| o >= self.len()) {
            return Err(ords.bad(
                ords.at,
                "a document's ords not increasing or past the dictionary",
            ));
        }
        Ok(out)
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

/// Whether `block` holds its true minimum and the bit length of its largest offset (SPEC §5.2.1: what writers do).
fn minimal(block: &ForBlock<'_>) -> bool {
    let (min, max) = block
        .iter()
        .fold((u64::MAX, 0), |(lo, hi), k| (lo.min(k), hi.max(k)));
    let width = u8::try_from(max.wrapping_sub(min).bit_width()).unwrap_or(u8::MAX);
    min == block.min() && width == block.width()
}
