//! `TermDict`, `TermInfo`, `Postings`, `Freqs`, `Skip` and `Positions` of one field (SPEC §5.3.1–§5.3.3,
//! ROADMAP 5.2.1–5.2.3).

use persia_format::EncodeError;
use persia_format::Writer;
use persia_format::bitpack::{BLOCK_LEN, BitWidth};
use persia_format::block::Compression;
use persia_format::{postings, varint};

use super::super::SegmentError;
use super::super::builder::inverted::{Grouped, Inverted, Posting};
use super::super::kind::SectionKind;
use super::{Framed, Sections, Sink, WriteOptions};

/// Bytes of a `TermInfo` record (SPEC §5.3.1).
pub(super) const TERM_INFO_LEN: usize = 56;
/// Bytes of a `Skip` entry (SPEC §5.3.2).
pub(super) const SKIP_ENTRY_LEN: usize = 40;

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

/// The lists a pass over the postings encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum List {
    Postings,
    Freqs,
    Positions,
}

/// Writes the term sections of `inverted`, whose postings `grouped` holds by term, into `out` (SPEC §5.3.1–§5.3.3).
/// `norms` is the field's `Norms` array (a `text` field), for `min_norm`.
///
/// Sections are written in segment order, each straight into the segment (ROADMAP 5.1.6): the `TermDict` from the
/// terms alone; then one pass measures every list to learn where each starts (from bit widths and varint lengths,
/// ROADMAP 5.1.7), writing the `TermInfo` records and keeping the `Skip` entries (40 bytes per 128 postings); then one
/// pass per stream encodes it and writes it.
pub(super) fn write_terms(
    out: &mut Sections,
    field: u32,
    inverted: &Inverted,
    grouped: &Grouped<'_>,
    norms: Option<&[u8]>,
    options: WriteOptions,
) -> Result<(), SegmentError> {
    out.fst(SectionKind::TermDict, field, |dict| {
        for (rank, &term) in grouped.order.iter().enumerate() {
            let ordinal =
                u64::try_from(rank).map_err(|_| SegmentError::TooLarge { what: "terms" })?;
            dict.insert(inverted.terms.key(term), ordinal)
                .map_err(|_| fst_error(rank))?;
        }
        Ok(())
    })?;
    let mut enc = Encoder::new(inverted);
    let positions = inverted.positions.is_some();
    let (skip, ends) = out.framed(
        SectionKind::TermInfo,
        field,
        Compression::None,
        options,
        |info| enc.term_infos(grouped, norms, info),
    )?;
    for (list, kind, end) in [
        (List::Postings, SectionKind::Postings, ends.postings),
        (List::Freqs, SectionKind::Freqs, ends.freqs),
    ] {
        out.framed(kind, field, Compression::None, options, |s| {
            enc.list(grouped, list, s, end)
        })?;
    }
    if !skip.is_empty() {
        out.framed(SectionKind::Skip, field, Compression::None, options, |s| {
            s.put(skip.as_slice())
        })?;
    }
    if positions {
        out.framed(
            SectionKind::Positions,
            field,
            Compression::None,
            options,
            |s| enc.list(grouped, List::Positions, s, ends.positions),
        )?;
    }
    Ok(())
}

/// Where the lists of a field end in their streams.
#[derive(Debug, Clone, Copy, Default)]
struct Ends {
    postings: u64,
    freqs: u64,
    positions: u64,
}

/// Encodes postings blocks, reusing its buffers across blocks.
#[derive(Debug)]
struct Encoder<'a> {
    inverted: &'a Inverted,
    /// The block's postings.
    block: Vec<Posting>,
    /// Their docids.
    docids: Vec<u32>,
    values: Vec<u32>,
    w: Writer,
}

impl<'a> Encoder<'a> {
    fn new(inverted: &'a Inverted) -> Self {
        Self {
            inverted,
            block: Vec::with_capacity(BLOCK_LEN),
            docids: Vec::with_capacity(BLOCK_LEN),
            values: Vec::with_capacity(BLOCK_LEN),
            w: Writer::new(),
        }
    }

    /// Calls `f` on each postings block of the term of rank `rank` (up to 128 postings) and the docid before it.
    fn blocks(
        &mut self,
        grouped: &Grouped<'_>,
        rank: usize,
        mut f: impl FnMut(&mut Self, Option<u32>) -> Result<(), SegmentError>,
    ) -> Result<(), SegmentError> {
        let mut postings = grouped.postings_of(rank);
        let mut prev = None;
        loop {
            self.block.clear();
            self.block.extend(postings.by_ref().take(BLOCK_LEN));
            let Some(last) = self.block.last().map(|p| p.doc) else {
                return Ok(());
            };
            self.docids.clear();
            self.docids.extend(self.block.iter().map(|p| p.doc));
            f(self, prev)?;
            prev = Some(last);
        }
    }

    /// Encodes `list` of the current block into `w` (cleared first).
    fn encode(&mut self, list: List, prev: Option<u32>) -> Result<(), SegmentError> {
        self.w.clear();
        match list {
            List::Postings => write_docids(&self.docids, prev, &mut self.w)?,
            List::Freqs => {
                self.values.clear();
                self.values
                    .extend(self.block.iter().map(|p| p.tf.saturating_sub(1)));
                write_run(&self.values, &mut self.w)?;
            }
            List::Positions => {
                position_deltas(self.inverted, &self.block, &mut self.values)?;
                write_run(&self.values, &mut self.w)?;
            }
        }
        Ok(())
    }

    /// The length of `list`'s encoding of the current block, from bit widths and varint lengths: the `TermInfo` pass
    /// learns every offset without encoding a list (ROADMAP 5.1.7).
    fn encoded_len(&mut self, list: List, prev: Option<u32>) -> Result<u64, SegmentError> {
        let len = match list {
            List::Postings => {
                if let Ok(full) = <&[u32; BLOCK_LEN]>::try_from(self.docids.as_slice()) {
                    postings::block_len(full, prev)?
                } else {
                    postings::tail_len(&self.docids, prev)?
                }
            }
            List::Freqs => {
                self.values.clear();
                self.values
                    .extend(self.block.iter().map(|p| p.tf.saturating_sub(1)));
                run_len(&self.values)?
            }
            List::Positions => {
                position_deltas(self.inverted, &self.block, &mut self.values)?;
                run_len(&self.values)?
            }
        };
        u64::try_from(len).map_err(|_| SegmentError::TooLarge { what: "postings" })
    }

    /// Writes every term's `TermInfo` record to `info`, from a pass that measures every list to learn its offsets;
    /// returns the `Skip` entries and where the lists end.
    fn term_infos(
        &mut self,
        grouped: &Grouped<'_>,
        norms: Option<&[u8]>,
        info: &mut Framed<'_>,
    ) -> Result<(Writer, Ends), SegmentError> {
        let positions = self.inverted.positions.is_some();
        let mut at = Ends::default();
        let mut skip = Writer::new();
        let mut entries = Vec::new();
        let mut record = Writer::new();
        let too_large = || SegmentError::TooLarge { what: "postings" };
        for rank in 0..grouped.order.len() {
            entries.clear();
            self.blocks(grouped, rank, |enc, prev| {
                let mut entry = SkipEntry {
                    last_docid: 0,
                    max_tf: 0,
                    postings_off: at.postings,
                    freqs_off: at.freqs,
                    positions_off: if positions { at.positions } else { 0 },
                    min_norm: u8::MAX,
                };
                let add = |sum: u64, len| sum.checked_add(len).ok_or_else(too_large);
                at.postings = add(at.postings, enc.encoded_len(List::Postings, prev)?)?;
                at.freqs = add(at.freqs, enc.encoded_len(List::Freqs, prev)?)?;
                if positions {
                    at.positions = add(at.positions, enc.encoded_len(List::Positions, prev)?)?;
                }
                for p in &enc.block {
                    entry.max_tf = entry.max_tf.max(p.tf);
                    let norm = match norms {
                        Some(n) => {
                            n.get(p.doc as usize)
                                .copied()
                                .ok_or(SegmentError::Internal {
                                    what: "a docid past the norms",
                                })?
                        }
                        None => 0,
                    };
                    entry.min_norm = entry.min_norm.min(norm);
                    entry.last_docid = p.doc;
                }
                entries.push(entry);
                Ok(())
            })?;
            let df = grouped.df_of(rank);
            let Some(&first) = entries.first() else {
                return Err(SegmentError::Internal {
                    what: "a term without postings",
                });
            };
            let max_tf = entries.iter().map(|e| e.max_tf).max().unwrap_or(0);
            let min_norm = entries.iter().map(|e| e.min_norm).min().unwrap_or(0);
            // Only lists with a full block have skip entries; a shorter one is a single tail block.
            let skip_off = if df >= BLOCK_LEN {
                let off = u64::try_from(skip.len()).map_err(|_| too_large())?;
                write_skip(&entries, &mut skip);
                off
            } else {
                0
            };
            let ttf = grouped
                .postings_of(rank)
                .fold(0_u64, |sum, p| sum.saturating_add(u64::from(p.tf)));

            record.clear();
            record.write_u64(first.postings_off);
            record.write_u64(first.freqs_off);
            record.write_u64(skip_off);
            record.write_u64(first.positions_off);
            record.write_u64(ttf);
            record.write_u32(u32::try_from(df).map_err(|_| too_large())?);
            record.write_u32(max_tf);
            record.write_u8(min_norm);
            record.write_zeros(7);
            debug_assert_eq!(record.len(), TERM_INFO_LEN);
            info.put(record.as_slice())?;
        }
        Ok((skip, at))
    }

    /// Writes `list` of every term to `out`, which must end at `end`, where the `TermInfo` pass found it does.
    fn list(
        &mut self,
        grouped: &Grouped<'_>,
        list: List,
        out: &mut Framed<'_>,
        end: u64,
    ) -> Result<(), SegmentError> {
        for rank in 0..grouped.order.len() {
            self.blocks(grouped, rank, |enc, prev| {
                enc.encode(list, prev)?;
                out.put(enc.w.as_slice())
            })?;
        }
        if out.position() == end {
            Ok(())
        } else {
            Err(SegmentError::Internal {
                what: "a list encoded to another length than its offsets say",
            })
        }
    }
}

/// Appends a list's skip entries to `skip`.
fn write_skip(entries: &[SkipEntry], skip: &mut Writer) {
    for e in entries {
        skip.write_u32(e.last_docid);
        skip.write_u32(e.max_tf);
        skip.write_u64(e.postings_off);
        skip.write_u64(e.freqs_off);
        skip.write_u64(e.positions_off);
        skip.write_u8(e.min_norm);
        skip.write_zeros(7);
        debug_assert_eq!(skip.len() % SKIP_ENTRY_LEN, 0);
    }
}

/// The FST builder only fails on keys out of order, which sorted, distinct arena keys never are.
pub(super) fn fst_error(index: usize) -> SegmentError {
    SegmentError::Encode(EncodeError::NotStrictlyIncreasing { index })
}

/// A full postings block or the tail block (SPEC §5.2.1).
fn write_docids(docids: &[u32], prev: Option<u32>, w: &mut Writer) -> Result<(), EncodeError> {
    if let Ok(full) = <&[u32; BLOCK_LEN]>::try_from(docids) {
        w.write_postings_block(full, prev)
    } else {
        w.write_postings_tail(docids, prev)
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

/// The bytes [`write_run`] appends for `values`, without encoding them.
fn run_len(values: &[u32]) -> Result<usize, SegmentError> {
    let (groups, rest) = values.as_chunks::<BLOCK_LEN>();
    let packed = groups.iter().map(|g| 1 + BitWidth::of(g).packed_len());
    let varints = rest.iter().map(|&v| varint::encoded_len(u64::from(v)));
    packed
        .chain(varints)
        .try_fold(0_usize, usize::checked_add)
        .ok_or(SegmentError::TooLarge { what: "postings" })
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
        let positions = inverted.positions_of(p);
        if positions.len() != p.tf as usize {
            return Err(SegmentError::Internal {
                what: "a posting without its positions",
            });
        }
        let mut prev = 0;
        for &position in positions {
            // Positions of a posting never decrease (`Inverted::add_doc` sorts them).
            let delta = position.checked_sub(prev).ok_or(SegmentError::Internal {
                what: "positions out of order",
            })?;
            out.push(delta);
            prev = position;
        }
    }
    Ok(())
}
