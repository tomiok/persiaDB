//! `Columns` sections: doc values of `keyword`, numeric, `datetime` and `bool` fields (SPEC §5.3.5,
//! ROADMAP 5.2.5). Columns are generated block by block into the segment (ROADMAP 5.1.6): a FOR column is encoded
//! twice, once to learn where its blocks end (written before them) and once to write them, so no column is held
//! whole.

use persia_format::Writer;
use persia_format::for_codec::VALUES_PER_BLOCK;
use roaring::RoaringBitmap;

use super::super::SegmentError;
use super::super::builder::Column;
use super::super::builder::inverted::{Grouped, Inverted};
use super::Sink;

/// Column encodings, the first byte of a `Columns` stream (SPEC §5.3.5).
pub(crate) const TAG_NUMERIC: u8 = 1;
pub(crate) const TAG_BOOL: u8 = 2;
pub(crate) const TAG_KEYWORD: u8 = 3;

/// Calls its argument on every value of a column, in order; returns the first error it gets.
type Each<'f> = dyn FnMut(u64) -> Result<(), SegmentError> + 'f;

/// An `i64`, `f64` or `datetime` column: presence, then one key per document.
pub(super) fn numeric(
    column: &Column,
    doc_count: u32,
    out: &mut impl Sink,
) -> Result<(), SegmentError> {
    let mut w = header(TAG_NUMERIC);
    presence(column, doc_count, &mut w)?;
    out.put(w.as_slice())?;
    let keys = |f: &mut Each<'_>| dense_keys(&column.values, doc_count, f);
    let plan = ForPlan::new(u64::from(doc_count), keys)?;
    plan.write(keys, out)
}

/// A `bool` column: presence, then the documents whose value is `true`.
pub(super) fn boolean(
    column: &Column,
    doc_count: u32,
    out: &mut impl Sink,
) -> Result<(), SegmentError> {
    let mut w = header(TAG_BOOL);
    presence(column, doc_count, &mut w)?;
    let trues = column
        .values
        .iter()
        .filter(|&&(_, v)| v != 0)
        .map(|&(d, _)| d);
    bitmap(trues, &mut w)?;
    out.put(w.as_slice())
}

/// A `keyword` column: the dictionary of distinct values (ord = rank in byte order), then per document the
/// ascending ords of its values, as a starts column and an ords column.
pub(super) fn keyword(
    inverted: &Inverted,
    grouped: &Grouped<'_>,
    doc_count: u32,
    out: &mut impl Sink,
) -> Result<(), SegmentError> {
    let too_large = || SegmentError::TooLarge {
        what: "keyword column",
    };
    let postings = inverted.postings.as_slice();
    let count = |n: usize| u64::try_from(n).map_err(|_| too_large());
    let terms = count(grouped.order.len())?;
    let mut values_len = 0_u64;
    for &term in &grouped.order {
        values_len = values_len
            .checked_add(count(inverted.terms.key(term).len())?)
            .ok_or_else(too_large)?;
    }
    // `starts[d]`: the ords of the documents before `d`, one per posting (the log is in docid order).
    let starts = |f: &mut Each<'_>| {
        let mut i = 0_usize;
        for d in 0..=doc_count {
            while postings.get(i).is_some_and(|p| p.doc < d) {
                i = i.saturating_add(1);
            }
            f(count(i)?)?;
        }
        Ok(())
    };
    // Each document's ords, ascending: the ranks of its terms (the log orders them by term id).
    let ords = |f: &mut Each<'_>| {
        let mut doc_ords = Vec::new();
        for doc in postings.chunk_by(|a, b| a.doc == b.doc) {
            doc_ords.clear();
            for p in doc {
                doc_ords.push(grouped.rank_of(p.term).ok_or(SegmentError::Internal {
                    what: "a posting of an unknown term",
                })?);
            }
            doc_ords.sort_unstable();
            for &o in &doc_ords {
                f(u64::from(o))?;
            }
        }
        Ok(())
    };
    let starts_plan = ForPlan::new(u64::from(doc_count).saturating_add(1), starts)?;
    let ords_plan = ForPlan::new(count(postings.len())?, ords)?;
    // The header, the two offsets, the count, the value ends and the values come first.
    let starts_off = terms
        .checked_mul(8)
        .and_then(|n| n.checked_add(32))
        .and_then(|n| n.checked_add(values_len))
        .ok_or_else(too_large)?;
    let ords_off = starts_off
        .checked_add(starts_plan.len()?)
        .ok_or_else(too_large)?;
    let mut w = header(TAG_KEYWORD);
    w.write_u64(starts_off);
    w.write_u64(ords_off);
    w.write_u64(terms);
    let mut end = 0_u64;
    for &term in &grouped.order {
        end = end.saturating_add(count(inverted.terms.key(term).len())?);
        w.write_u64(end);
        flush_at(&mut w, out)?;
    }
    for &term in &grouped.order {
        w.write_bytes(inverted.terms.key(term));
        flush_at(&mut w, out)?;
    }
    out.put(w.as_slice())?;
    starts_plan.write(starts, out)?;
    ords_plan.write(ords, out)
}

/// Bytes a scratch writer gathers before it is put.
const FLUSH_AT: usize = 64 << 10;

/// Puts `w`'s bytes once it holds [`FLUSH_AT`], so writing many small values does not put each.
fn flush_at(w: &mut Writer, out: &mut impl Sink) -> Result<(), SegmentError> {
    if w.len() >= FLUSH_AT {
        out.put(w.as_slice())?;
        w.clear();
    }
    Ok(())
}

fn header(tag: u8) -> Writer {
    let mut w = Writer::new();
    w.write_u8(tag);
    w.write_zeros(7);
    w
}

/// The documents that have a value: length 0 when all do, else a roaring bitmap.
fn presence(column: &Column, doc_count: u32, w: &mut Writer) -> Result<(), SegmentError> {
    let all = u32::try_from(column.values.len()).is_ok_and(|n| n == doc_count);
    if all {
        w.write_u64(0);
        Ok(())
    } else {
        bitmap(column.values.iter().map(|&(d, _)| d), w)
    }
}

/// A length-prefixed roaring bitmap in the portable format.
fn bitmap(docs: impl Iterator<Item = u32>, w: &mut Writer) -> Result<(), SegmentError> {
    // Docids arrive ascending and distinct, and writing to a `Vec` cannot fail.
    let internal = |what| SegmentError::Internal { what };
    let bitmap =
        RoaringBitmap::from_sorted_iter(docs).map_err(|_| internal("docids out of order"))?;
    let mut bytes = Vec::with_capacity(bitmap.serialized_size());
    bitmap
        .serialize_into(&mut bytes)
        .map_err(|_| internal("bitmap serialization"))?;
    w.write_u64(u64::try_from(bytes.len()).map_err(|_| SegmentError::TooLarge { what: "bitmap" })?);
    w.write_bytes(&bytes);
    Ok(())
}

/// One key per document, to `f`; a document without a value gets its FOR block's minimum, so it costs no bits.
fn dense_keys(values: &[(u32, u64)], doc_count: u32, f: &mut Each<'_>) -> Result<(), SegmentError> {
    let per_block = u32::try_from(VALUES_PER_BLOCK).unwrap_or(u32::MAX);
    let mut rest = values;
    let mut start = 0_u32;
    while start < doc_count {
        let end = start.saturating_add(per_block).min(doc_count);
        let split = rest.partition_point(|&(d, _)| d < end);
        let (here, later) = rest.split_at(split);
        rest = later;
        let min = here.iter().map(|&(_, k)| k).min().unwrap_or(0);
        let mut present = here.iter().peekable();
        for d in start..end {
            match present.next_if(|&&(pd, _)| pd == d) {
                Some(&(_, k)) => f(k)?,
                None => f(min)?,
            }
        }
        start = end;
    }
    Ok(())
}

/// A FOR column (SPEC §5.3.5): the value count, the end of each 1024-value FOR block (SPEC §5.2.1), the blocks.
#[derive(Debug)]
struct ForPlan {
    count: u64,
    /// Each block's end, relative to the first block.
    ends: Vec<u64>,
}

impl ForPlan {
    /// Encodes the `count` values `each` yields, keeping only where each block ends.
    fn new(
        count: u64,
        each: impl FnMut(&mut Each<'_>) -> Result<(), SegmentError>,
    ) -> Result<Self, SegmentError> {
        let too_large = || SegmentError::TooLarge { what: "column" };
        let mut w = Writer::new();
        let mut ends = Vec::new();
        let mut end = 0_u64;
        let got = for_blocks(each, |block| {
            w.clear();
            w.write_for_block(block)?;
            let len = u64::try_from(w.len()).map_err(|_| too_large())?;
            end = end.checked_add(len).ok_or_else(too_large)?;
            ends.push(end);
            Ok(())
        })?;
        if got != count {
            return Err(SegmentError::Internal {
                what: "a column with another value count than its documents",
            });
        }
        Ok(Self { count, ends })
    }

    /// Bytes of the column.
    fn len(&self) -> Result<u64, SegmentError> {
        let blocks = u64::try_from(self.ends.len()).ok();
        blocks
            .and_then(|b| b.checked_add(1))
            .and_then(|n| n.checked_mul(8))
            .and_then(|n| n.checked_add(self.ends.last().copied().unwrap_or(0)))
            .ok_or(SegmentError::TooLarge { what: "column" })
    }

    /// Writes the column, encoding again the values `each` yields, which must be the ones [`ForPlan::new`] saw.
    fn write(
        &self,
        each: impl FnMut(&mut Each<'_>) -> Result<(), SegmentError>,
        out: &mut impl Sink,
    ) -> Result<(), SegmentError> {
        let mut w = Writer::new();
        w.write_u64(self.count);
        for &end in &self.ends {
            w.write_u64(end);
            flush_at(&mut w, out)?;
        }
        out.put(w.as_slice())?;
        let mut ends = self.ends.iter();
        let mut end = 0_u64;
        for_blocks(each, |block| {
            w.clear();
            w.write_for_block(block)?;
            end = end.saturating_add(u64::try_from(w.len()).unwrap_or(u64::MAX));
            if ends.next() != Some(&end) {
                return Err(SegmentError::Internal {
                    what: "a column encoded to another length than planned",
                });
            }
            out.put(w.as_slice())
        })?;
        Ok(())
    }
}

/// Calls `block` on the values `each` yields, 1024 at a time (the last block may hold fewer); returns their count.
fn for_blocks(
    mut each: impl FnMut(&mut Each<'_>) -> Result<(), SegmentError>,
    mut block: impl FnMut(&[u64]) -> Result<(), SegmentError>,
) -> Result<u64, SegmentError> {
    let mut buf = Vec::with_capacity(VALUES_PER_BLOCK);
    let mut count = 0_u64;
    each(&mut |v| {
        buf.push(v);
        count = count.saturating_add(1);
        if buf.len() == VALUES_PER_BLOCK {
            block(&buf)?;
            buf.clear();
        }
        Ok(())
    })?;
    if !buf.is_empty() {
        block(&buf)?;
    }
    Ok(count)
}
