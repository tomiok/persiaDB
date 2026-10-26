//! `Columns` sections: doc values of `keyword`, numeric, `datetime` and `bool` fields (SPEC §5.3.5,
//! ROADMAP 5.2.5).

use persia_format::Writer;
use persia_format::for_codec::VALUES_PER_BLOCK;
use roaring::RoaringBitmap;

use super::super::SegmentError;
use super::super::builder::Column;
use super::super::builder::inverted::{Grouped, Inverted};

/// Column encodings, the first byte of a `Columns` stream (SPEC §5.3.5).
pub(crate) const TAG_NUMERIC: u8 = 1;
pub(crate) const TAG_BOOL: u8 = 2;
pub(crate) const TAG_KEYWORD: u8 = 3;

/// An `i64`, `f64` or `datetime` column: presence, then one key per document.
pub(super) fn numeric(column: &Column, doc_count: u32) -> Result<Vec<u8>, SegmentError> {
    let mut w = header(TAG_NUMERIC);
    presence(column, doc_count, &mut w)?;
    let keys = dense_keys(&column.values, doc_count)?;
    for_column(&keys, &mut w)?;
    Ok(w.into_inner())
}

/// A `bool` column: presence, then the documents whose value is `true`.
pub(super) fn boolean(column: &Column, doc_count: u32) -> Result<Vec<u8>, SegmentError> {
    let mut w = header(TAG_BOOL);
    presence(column, doc_count, &mut w)?;
    let trues = column
        .values
        .iter()
        .filter(|&&(_, v)| v != 0)
        .map(|&(d, _)| d);
    bitmap(trues, &mut w)?;
    Ok(w.into_inner())
}

/// A `keyword` column: the dictionary of distinct values (ord = rank in byte order), then per document the
/// ascending ords of its values, as a starts column and an ords column.
pub(super) fn keyword(
    inverted: &Inverted,
    grouped: &Grouped,
    doc_count: u32,
) -> Result<Vec<u8>, SegmentError> {
    let too_large = || SegmentError::TooLarge {
        what: "keyword column",
    };
    // The dictionary, after the header and the two offsets (8 + 16 bytes).
    let mut w = Writer::with_base_offset(24);
    w.write_u64(u64::try_from(grouped.order.len()).map_err(|_| too_large())?);
    let mut end = 0_u64;
    for &term in &grouped.order {
        let len = u64::try_from(inverted.terms.key(term).len()).map_err(|_| too_large())?;
        end = end.checked_add(len).ok_or_else(too_large)?;
        w.write_u64(end);
    }
    for &term in &grouped.order {
        w.write_bytes(inverted.terms.key(term));
    }

    // Counting sort of (doc, ord) by doc; ords are visited in ascending order, so each document's stay sorted.
    let docs = doc_count as usize;
    let mut starts = vec![0_u64; docs.saturating_add(1)];
    for p in &grouped.postings {
        if let Some(c) = starts.get_mut((p.doc as usize).saturating_add(1)) {
            *c = c.saturating_add(1);
        }
    }
    for i in 1..starts.len() {
        let prev = starts.get(i.wrapping_sub(1)).copied().unwrap_or(0);
        if let Some(s) = starts.get_mut(i) {
            *s = s.saturating_add(prev);
        }
    }
    let mut next = starts.clone();
    let mut ords = vec![0_u64; grouped.postings.len()];
    for rank in 0..grouped.order.len() {
        let ord = u64::try_from(rank).map_err(|_| too_large())?;
        for p in grouped.postings_of(rank) {
            let Some(n) = next.get_mut(p.doc as usize) else {
                continue;
            };
            let slot = usize::try_from(*n).map_err(|_| too_large())?;
            if let Some(o) = ords.get_mut(slot) {
                *o = ord;
            }
            *n = n.saturating_add(1);
        }
    }
    let starts_off = w.offset();
    for_column(&starts, &mut w)?;
    let ords_off = w.offset();
    for_column(&ords, &mut w)?;
    let mut out = header(TAG_KEYWORD);
    out.write_u64(starts_off);
    out.write_u64(ords_off);
    out.write_bytes(w.as_slice());
    Ok(out.into_inner())
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

/// One key per document; a document without a value gets its FOR block's minimum, so it costs no bits.
fn dense_keys(values: &[(u32, u64)], doc_count: u32) -> Result<Vec<u64>, SegmentError> {
    let mut keys = vec![0_u64; doc_count as usize];
    let mut rest = values;
    for (block_index, block) in keys.chunks_mut(VALUES_PER_BLOCK).enumerate() {
        let block_end = block_index
            .checked_add(1)
            .and_then(|b| b.checked_mul(VALUES_PER_BLOCK))
            .ok_or(SegmentError::TooLarge { what: "column" })?;
        let split = rest.partition_point(|&(d, _)| (d as usize) < block_end);
        let (here, later) = rest.split_at(split);
        rest = later;
        let min = here.iter().map(|&(_, k)| k).min().unwrap_or(0);
        block.fill(min);
        let base = block_index.saturating_mul(VALUES_PER_BLOCK);
        for &(d, k) in here {
            if let Some(slot) = (d as usize)
                .checked_sub(base)
                .and_then(|i| block.get_mut(i))
            {
                *slot = k;
            }
        }
    }
    Ok(keys)
}

/// A FOR column (SPEC §5.3.5): the value count, the end of each 1024-value FOR block (SPEC §5.2.1), the blocks.
pub(super) fn for_column(values: &[u64], w: &mut Writer) -> Result<(), SegmentError> {
    let too_large = || SegmentError::TooLarge { what: "column" };
    let mut blocks = Writer::new();
    let mut ends = Vec::new();
    for block in values.chunks(VALUES_PER_BLOCK) {
        blocks.write_for_block(block)?;
        ends.push(u64::try_from(blocks.len()).map_err(|_| too_large())?);
    }
    w.write_u64(u64::try_from(values.len()).map_err(|_| too_large())?);
    for end in ends {
        w.write_u64(end);
    }
    w.write_bytes(blocks.as_slice());
    Ok(())
}
