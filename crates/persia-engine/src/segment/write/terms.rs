//! `TermDict`, `TermInfo`, `Postings`, `Freqs`, `Skip` and `Positions` of one field (SPEC §5.3.1–§5.3.3,
//! ROADMAP 5.2.1–5.2.3).

use persia_format::EncodeError;
use persia_format::Writer;
use persia_format::bitpack::{BLOCK_LEN, BitWidth};
use persia_format::block::{BlockWriter, Compression};

use super::super::SegmentError;
use super::super::builder::inverted::{Inverted, Posting};
use super::{Grouped, WriteOptions, raw_section};

/// Bytes of a `TermInfo` record (SPEC §5.3.1).
pub(super) const TERM_INFO_LEN: usize = 48;
/// Bytes of a `Skip` entry (SPEC §5.3.2).
pub(super) const SKIP_ENTRY_LEN: usize = 40;

/// The term sections of one field; `skip` and `positions` only when the field has them (SPEC §5.3).
#[derive(Debug)]
pub(super) struct TermSections {
    pub(super) dict: Vec<u8>,
    pub(super) info: Vec<u8>,
    pub(super) postings: Vec<u8>,
    pub(super) freqs: Vec<u8>,
    pub(super) skip: Option<Vec<u8>>,
    pub(super) positions: Option<Vec<u8>>,
}

/// One postings block's `Skip` entry (SPEC §5.3.2).
#[derive(Debug, Clone, Copy)]
struct SkipEntry {
    last_docid: u32,
    max_tf: u32,
    postings_off: u64,
    freqs_off: u64,
    positions_off: u64,
    min_norm: u8,
}

/// Writes the term sections of `inverted`, whose postings `grouped` holds by term. `norms` is the field's `Norms`
/// array (a `text` field), for `min_norm`.
pub(super) fn write_terms(
    inverted: &Inverted,
    grouped: &Grouped,
    norms: Option<&[u8]>,
    options: WriteOptions,
) -> Result<TermSections, SegmentError> {
    let framed = || BlockWriter::new(options.block_size, Compression::None);
    let mut dict = fst::MapBuilder::memory();
    let mut info = framed()?;
    let mut lists = Lists {
        postings: framed()?,
        freqs: framed()?,
        skip: framed()?,
        positions: match inverted.positions {
            Some(_) => Some(framed()?),
            None => None,
        },
        values: Vec::new(),
    };
    let mut entries = Vec::new();

    for (rank, &term) in grouped.order.iter().enumerate() {
        let list = grouped.postings_of(rank);
        let df =
            u32::try_from(list.len()).map_err(|_| SegmentError::TooLarge { what: "postings" })?;
        entries.clear();
        let mut prev = None;
        for block in list.chunks(BLOCK_LEN) {
            let entry = lists.write_block(inverted, block, prev, norms)?;
            prev = Some(entry.last_docid);
            entries.push(entry);
        }
        let first = entries.first().copied().unwrap_or(SkipEntry::EMPTY);
        // Only lists with a full block have skip entries; a shorter one is a single tail block.
        let skip_off = if list.len() >= BLOCK_LEN {
            lists.write_skip(&entries)?
        } else {
            0
        };
        let ttf = list
            .iter()
            .fold(0_u64, |sum, p| sum.saturating_add(u64::from(p.tf)));

        let mut w = Writer::new();
        w.write_u64(first.postings_off);
        w.write_u64(first.freqs_off);
        w.write_u64(skip_off);
        w.write_u64(first.positions_off);
        w.write_u64(ttf);
        w.write_u32(df);
        w.write_u32(0);
        debug_assert_eq!(w.len(), TERM_INFO_LEN);
        info.write(w.as_slice())?;

        let ordinal = u64::try_from(rank).map_err(|_| SegmentError::TooLarge { what: "terms" })?;
        dict.insert(inverted.terms.key(term), ordinal)
            .map_err(|_| fst_error(rank))?;
    }

    let dict = dict
        .into_inner()
        .map_err(|_| fst_error(grouped.order.len()))?;
    let skip = (lists.skip.position() > 0).then_some(lists.skip);
    Ok(TermSections {
        dict: raw_section(dict),
        info: info.finish()?,
        postings: lists.postings.finish()?,
        freqs: lists.freqs.finish()?,
        skip: skip.map(BlockWriter::finish).transpose()?,
        positions: lists.positions.map(BlockWriter::finish).transpose()?,
    })
}

/// The streams of a field's posting lists, and a buffer reused across blocks.
#[derive(Debug)]
struct Lists {
    postings: BlockWriter,
    freqs: BlockWriter,
    skip: BlockWriter,
    positions: Option<BlockWriter>,
    values: Vec<u32>,
}

impl SkipEntry {
    const EMPTY: Self = Self {
        last_docid: 0,
        max_tf: 0,
        postings_off: 0,
        freqs_off: 0,
        positions_off: 0,
        min_norm: 0,
    };
}

impl Lists {
    /// Writes one postings block (up to 128 postings following `prev`), its freqs and its positions run, and
    /// returns its skip entry.
    fn write_block(
        &mut self,
        inverted: &Inverted,
        block: &[Posting],
        prev: Option<u32>,
        norms: Option<&[u8]>,
    ) -> Result<SkipEntry, SegmentError> {
        let mut entry = SkipEntry {
            last_docid: 0,
            max_tf: 0,
            postings_off: self.postings.position(),
            freqs_off: self.freqs.position(),
            positions_off: self.positions.as_ref().map_or(0, BlockWriter::position),
            min_norm: u8::MAX,
        };
        let mut w = Writer::new();
        write_docids(block, prev, &mut w)?;
        self.postings.write(w.as_slice())?;

        self.values.clear();
        self.values
            .extend(block.iter().map(|p| p.tf.saturating_sub(1)));
        let mut w = Writer::new();
        write_run(&self.values, &mut w)?;
        self.freqs.write(w.as_slice())?;

        if let Some(positions) = &mut self.positions {
            position_deltas(inverted, block, &mut self.values)?;
            let mut w = Writer::new();
            write_run(&self.values, &mut w)?;
            positions.write(w.as_slice())?;
        }

        for p in block {
            entry.max_tf = entry.max_tf.max(p.tf);
            let norm = norms
                .and_then(|n| n.get(p.doc as usize))
                .copied()
                .unwrap_or(0);
            entry.min_norm = entry.min_norm.min(norm);
            entry.last_docid = p.doc;
        }
        Ok(entry)
    }

    /// Writes a list's skip entries and returns where they start.
    fn write_skip(&mut self, entries: &[SkipEntry]) -> Result<u64, SegmentError> {
        let off = self.skip.position();
        let mut w = Writer::new();
        for e in entries {
            w.write_u32(e.last_docid);
            w.write_u32(e.max_tf);
            w.write_u64(e.postings_off);
            w.write_u64(e.freqs_off);
            w.write_u64(e.positions_off);
            w.write_u8(e.min_norm);
            w.write_zeros(7);
            debug_assert_eq!(w.len() % SKIP_ENTRY_LEN, 0);
        }
        self.skip.write(w.as_slice())?;
        Ok(off)
    }
}

/// The FST builder only fails on keys out of order, which sorted, distinct arena keys never are.
pub(super) fn fst_error(index: usize) -> SegmentError {
    SegmentError::Encode(EncodeError::NotStrictlyIncreasing { index })
}

/// A full postings block or the tail block (SPEC §5.2.1).
fn write_docids(block: &[Posting], prev: Option<u32>, w: &mut Writer) -> Result<(), EncodeError> {
    if let Ok(full) = <&[Posting; BLOCK_LEN]>::try_from(block) {
        let docids = full.map(|p| p.doc);
        w.write_postings_block(&docids, prev)
    } else {
        let docids: Vec<u32> = block.iter().map(|p| p.doc).collect();
        w.write_postings_tail(&docids, prev)
    }
}

/// Values as bit-packed 128-groups, then the rest as varints (SPEC §5.3.2, §5.3.3).
pub(super) fn write_run(values: &[u32], w: &mut Writer) -> Result<(), EncodeError> {
    let (groups, rest) = values.as_chunks::<BLOCK_LEN>();
    for group in groups {
        let width = BitWidth::of(group);
        w.write_bit_width(width);
        w.write_packed_block(group, width)?;
    }
    for &v in rest {
        w.write_varint_u32(v);
    }
    Ok(())
}

/// The positions of a block's postings as the `Positions` run stores them: per document, its first position,
/// then the difference to the previous one (SPEC §5.3.3).
fn position_deltas(
    inverted: &Inverted,
    block: &[Posting],
    out: &mut Vec<u32>,
) -> Result<(), SegmentError> {
    out.clear();
    for p in block {
        let mut prev = 0;
        for &position in inverted.positions_of(p) {
            // Positions of a posting ascend (`Inverted::add_doc` sorts them).
            let delta = position.checked_sub(prev).ok_or(SegmentError::Encode(
                EncodeError::NotStrictlyIncreasing { index: out.len() },
            ))?;
            out.push(delta);
            prev = position;
        }
    }
    Ok(())
}
