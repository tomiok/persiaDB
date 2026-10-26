//! Typed readers of `Norms` and `Columns` (SPEC §5.3.4, §5.3.5; ROADMAP 5.3.5). Each decodes and validates its
//! whole section when it is opened, then answers per document in O(1).

use persia_format::Reader;
use persia_format::for_codec::{VALUES_PER_BLOCK, key_to_f64, key_to_i64};
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

/// Reads the parts of a `Columns` stream, mapping errors to its section.
struct Parts<'a, 's> {
    r: Reader<'a>,
    stream: Stream<'s>,
}

impl Parts<'_, '_> {
    fn bad(&self, position: u64, what: &'static str) -> ReadError {
        self.stream
            .corrupt(position, SegmentCorruption::Invalid { what })
    }

    fn u64(&mut self) -> Result<u64, ReadError> {
        self.r.read_u64().map_err(|e| self.stream.error(e))
    }

    /// The tag byte and its 7 zero bytes.
    fn header(&mut self, tag: u8) -> Result<(), ReadError> {
        let at = self.r.offset();
        let got = self.r.read_u8().map_err(|e| self.stream.error(e))?;
        if got != tag {
            return Err(self.bad(at, "a column tag other than the field type's"));
        }
        self.r.expect_zeros(7).map_err(|e| self.stream.error(e))
    }

    /// A length-prefixed roaring bitmap; `None` for length 0. Its docids must be below `doc_count`.
    fn bitmap(&mut self, doc_count: u32) -> Result<Option<RoaringBitmap>, ReadError> {
        let at = self.r.offset();
        let len = self.u64()?;
        if len == 0 {
            return Ok(None);
        }
        let len = usize::try_from(len).map_err(|_| self.bad(at, "a bitmap length"))?;
        let bytes = self.r.read_bytes(len).map_err(|e| self.stream.error(e))?;
        let bitmap = RoaringBitmap::deserialize_from(bytes)
            .map_err(|_| self.bad(at, "a bitmap that does not deserialize"))?;
        if bitmap.serialized_size() != len {
            return Err(self.bad(at, "bytes after a bitmap"));
        }
        if bitmap.max().is_some_and(|d| d >= doc_count) {
            return Err(self.bad(at, "a bitmap docid at or above doc_count"));
        }
        Ok(Some(bitmap))
    }

    /// A FOR column of exactly `count` values (SPEC §5.3.5).
    fn for_column(&mut self, count: u64) -> Result<Vec<u64>, ReadError> {
        let at = self.r.offset();
        if self.u64()? != count {
            return Err(self.bad(at, "a FOR column count"));
        }
        let per_block = u64::try_from(VALUES_PER_BLOCK).unwrap_or(1024);
        let blocks = count.div_ceil(per_block);
        // Each block end takes 8 bytes and each block at least 13: a bound before allocating.
        let remaining = u64::try_from(self.r.remaining()).unwrap_or(u64::MAX);
        if blocks.saturating_mul(21) > remaining {
            return Err(self.bad(at, "a FOR column longer than its stream"));
        }
        let mut ends = Vec::with_capacity(usize::try_from(blocks).unwrap_or(0));
        for _ in 0..blocks {
            ends.push(self.u64()?);
        }
        let data_at = self.r.offset();
        let mut values = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
        let mut start = 0_u64;
        for (i, &end) in ends.iter().enumerate() {
            let len = end
                .checked_sub(start)
                .filter(|&l| l > 0)
                .and_then(|l| usize::try_from(l).ok())
                .ok_or_else(|| self.bad(data_at, "FOR block ends that do not increase"))?;
            let block_at = self.r.offset();
            let mut br = self.r.sub_reader(len).map_err(|e| self.stream.error(e))?;
            let block = br.read_for_block().map_err(|e| self.stream.error(e))?;
            if !br.is_empty() {
                return Err(self.bad(block_at, "bytes after a FOR block"));
            }
            let index = u64::try_from(i).unwrap_or(u64::MAX);
            let expected = count
                .saturating_sub(index.saturating_mul(per_block))
                .min(per_block);
            if u64::try_from(block.len()).ok() != Some(expected) {
                return Err(self.bad(block_at, "a FOR block count"));
            }
            values.extend(block.iter());
            start = end;
        }
        Ok(values)
    }

    fn finish(&self) -> Result<(), ReadError> {
        if self.r.is_empty() {
            Ok(())
        } else {
            Err(self
                .stream
                .corrupt(self.r.offset(), SegmentCorruption::TrailingBytes))
        }
    }
}

/// Decodes `stream` and hands its parts to `f`.
fn parse<T>(
    stream: Stream<'_>,
    f: impl FnOnce(&mut Parts<'_, '_>) -> Result<T, ReadError>,
) -> Result<T, ReadError> {
    let bytes = stream.read_all()?;
    let mut parts = Parts {
        r: Reader::new(&bytes),
        stream,
    };
    let out = f(&mut parts)?;
    parts.finish()?;
    Ok(out)
}

/// The values of an `i64`, `f64` or `datetime` field (SPEC §5.3.5).
#[derive(Debug, Clone)]
pub struct NumericColumn {
    field_type: FieldType,
    /// `None`: every document has a value.
    presence: Option<RoaringBitmap>,
    /// One order-preserving key per document; empty when no document has a value.
    keys: Vec<u64>,
}

impl NumericColumn {
    pub(super) fn read(
        stream: Option<Stream<'_>>,
        field_type: FieldType,
        doc_count: u32,
    ) -> Result<Self, ReadError> {
        let Some(stream) = stream else {
            return Ok(Self {
                field_type,
                presence: Some(RoaringBitmap::new()),
                keys: Vec::new(),
            });
        };
        parse(stream, |p| {
            p.header(TAG_NUMERIC)?;
            let presence = p.bitmap(doc_count)?;
            let keys = p.for_column(u64::from(doc_count))?;
            Ok(Self {
                field_type,
                presence,
                keys,
            })
        })
    }

    /// Whether `docid` has a value.
    pub fn has_value(&self, docid: u32) -> bool {
        let present = self.presence.as_ref().is_none_or(|p| p.contains(docid));
        present && usize::try_from(docid).is_ok_and(|d| d < self.keys.len())
    }

    /// `docid`'s order-preserving key (SPEC §5.2.1): unsigned order is value order, for range filters and sorting.
    pub fn key(&self, docid: u32) -> Option<u64> {
        if !self.has_value(docid) {
            return None;
        }
        usize::try_from(docid)
            .ok()
            .and_then(|d| self.keys.get(d))
            .copied()
    }

    /// `docid`'s value, typed as the field.
    pub fn get(&self, docid: u32) -> Option<Value> {
        let key = self.key(docid)?;
        Some(match self.field_type {
            FieldType::F64 => Value::F64(key_to_f64(key)),
            FieldType::Datetime => Value::Datetime(key_to_i64(key)),
            _ => Value::I64(key_to_i64(key)),
        })
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
        parse(stream, |p| {
            p.header(TAG_BOOL)?;
            let presence = p.bitmap(doc_count)?;
            let at = p.r.offset();
            let trues = p
                .bitmap(doc_count)?
                .ok_or_else(|| p.bad(at, "a bool column without its true bitmap"))?;
            if presence.as_ref().is_some_and(|pr| !trues.is_subset(pr)) {
                return Err(p.bad(at, "a true value for a document without one"));
            }
            Ok(Self {
                presence,
                trues,
                doc_count,
            })
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

/// The values of a `keyword` field as doc values (SPEC §5.3.5): a dictionary, and per document the ordinals of
/// its distinct values, ascending.
#[derive(Debug, Clone, Default)]
pub struct KeywordColumn {
    /// The values, back to back; value `i` ends at `value_ends[i]`.
    values: Vec<u8>,
    value_ends: Vec<usize>,
    /// `doc_count + 1` starts into `ords`.
    starts: Vec<u64>,
    ords: Vec<u64>,
}

impl KeywordColumn {
    pub(super) fn read(stream: Option<Stream<'_>>, doc_count: u32) -> Result<Self, ReadError> {
        let Some(stream) = stream else {
            return Ok(Self::default());
        };
        parse(stream, |p| {
            p.header(TAG_KEYWORD)?;
            let starts_off = p.u64()?;
            let ords_off = p.u64()?;
            let at = p.r.offset();
            let count = p.u64()?;
            let remaining = u64::try_from(p.r.remaining()).unwrap_or(u64::MAX);
            if count == 0 || count.saturating_mul(8) > remaining {
                return Err(p.bad(at, "a keyword dictionary count"));
            }
            let mut value_ends = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
            for _ in 0..count {
                let end = p.u64()?;
                value_ends.push(usize::try_from(end).map_err(|_| p.bad(at, "a value end"))?);
            }
            let total = value_ends.last().copied().unwrap_or(0);
            let values =
                p.r.read_bytes(total)
                    .map_err(|e| p.stream.error(e))?
                    .to_vec();
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
            if p.r.offset() != starts_off {
                return Err(p.bad(at, "starts_off"));
            }
            let starts = p.for_column(u64::from(doc_count).saturating_add(1))?;
            if p.r.offset() != ords_off {
                return Err(p.bad(at, "ords_off"));
            }
            let ords_at = p.r.offset();
            let ord_count = starts.last().copied().unwrap_or(0);
            let ords = p.for_column(ord_count)?;
            if starts.first() != Some(&0) || starts.windows(2).any(|w| matches!(w, [a, b] if a > b))
            {
                return Err(p.bad(ords_at, "keyword starts that decrease"));
            }
            for w in starts.windows(2) {
                let [a, b] = w else { continue };
                let doc_ords = usize::try_from(*a)
                    .ok()
                    .zip(usize::try_from(*b).ok())
                    .and_then(|(a, b)| ords.get(a..b))
                    .unwrap_or_default();
                let increasing = doc_ords.windows(2).all(|o| matches!(o, [x, y] if x < y));
                if !increasing || doc_ords.last().is_some_and(|&o| o >= count) {
                    return Err(p.bad(
                        ords_at,
                        "a document's ords not increasing or past the dictionary",
                    ));
                }
            }
            Ok(Self {
                values,
                value_ends,
                starts,
                ords,
            })
        })
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

    /// The ordinals of `docid`'s distinct values, ascending; empty for a document without one.
    pub fn ords(&self, docid: u32) -> &[u64] {
        let i = usize::try_from(docid).unwrap_or(usize::MAX);
        let range = self
            .starts
            .get(i)
            .zip(self.starts.get(i.saturating_add(1)))
            .and_then(|(&a, &b)| usize::try_from(a).ok().zip(usize::try_from(b).ok()));
        range
            .and_then(|(a, b)| self.ords.get(a..b))
            .unwrap_or_default()
    }

    /// `docid`'s distinct values, ascending.
    pub fn values(&self, docid: u32) -> impl Iterator<Item = &[u8]> {
        self.ords(docid).iter().filter_map(|&o| self.value(o))
    }
}
