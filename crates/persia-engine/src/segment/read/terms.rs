//! A field's term dictionary and term info (SPEC §5.3.1, ROADMAP 5.3.2), and term enumeration by range, prefix or
//! automaton (ROADMAP 5.3.8).

use core::ops::{Bound, Range};

use fst::automaton::{AlwaysMatch, Automaton};
use fst::{IntoStreamer, Streamer};
use persia_format::ByteSource;
use persia_format::bitpack::BLOCK_LEN;

use super::error::{ReadError, SegmentCorruption};
use super::postings::Postings;
use super::stream::{Cursor, Stream};

/// Bytes of a `TermInfo` record and of a `Skip` entry (SPEC §5.3.1, §5.3.2).
pub(super) const TERM_INFO_LEN: u64 = 56;
pub(super) const SKIP_ENTRY_LEN: u64 = 40;

/// What a field's `TermInfo` says about one term (SPEC §5.3.1), checked against its neighbours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermInfo {
    ordinal: u64,
    df: u32,
    ttf: u64,
    max_tf: u32,
    min_norm: u8,
    pub(super) postings: Range<u64>,
    pub(super) freqs: Range<u64>,
    /// Start of the term's `Skip` entries; `None` when `df < 128`.
    pub(super) skip: Option<u64>,
    pub(super) positions: Option<Range<u64>>,
    /// The segment id and field id of the index that returned it.
    owner: (u128, u32),
}

impl TermInfo {
    /// The term's rank in the field's terms, in byte order.
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }

    /// Documents with the term (at least 1).
    pub fn df(&self) -> u32 {
        self.df
    }

    /// Sum of the term's frequencies.
    pub fn ttf(&self) -> u64 {
        self.ttf
    }

    /// Largest frequency of the term in any document: a score bound before the list is read (SPEC §7.3).
    pub fn max_tf(&self) -> u32 {
        self.max_tf
    }

    /// Smallest norm byte of the term's documents (SPEC §5.3.4); 0 for a `keyword` field.
    pub fn min_norm(&self) -> u8 {
        self.min_norm
    }
}

/// The streams a field's posting lists live in.
#[derive(Debug, Clone, Copy)]
pub(super) struct Lists<'s> {
    pub(super) postings: Stream<'s>,
    pub(super) freqs: Stream<'s>,
    pub(super) skip: Option<Stream<'s>>,
    pub(super) positions: Option<Stream<'s>>,
}

/// Reads `TermInfo` records.
#[derive(Debug, Clone)]
struct InfoReader<'s> {
    info: Cursor<'s>,
    count: u64,
    doc_count: u32,
    keyword: bool,
    lists: Lists<'s>,
    owner: (u128, u32),
}

impl InfoReader<'_> {
    fn corrupt(&self, position: u64, what: &'static str) -> ReadError {
        self.info
            .stream
            .corrupt(position, SegmentCorruption::Invalid { what })
    }

    /// Record `ordinal`, with the list ends the next record (or the streams' ends) gives it.
    fn info(&mut self, ordinal: u64) -> Result<TermInfo, ReadError> {
        if ordinal >= self.count {
            return Err(ReadError::Section {
                kind: super::super::SectionKind::TermDict,
                field: self.info.stream.field,
                position: 0,
                reason: SegmentCorruption::Fst,
            });
        }
        // ordinal < count, and count · 56 is the stream length (checked at open).
        let at = ordinal.saturating_mul(TERM_INFO_LEN);
        let last = ordinal.saturating_add(1) == self.count;
        let end = at.saturating_add(TERM_INFO_LEN.saturating_mul(if last { 1 } else { 2 }));
        let stream = self.info.stream;
        let mut r = self.info.reader(at, end)?;
        let mut u64_ = || r.read_u64().map_err(|e| stream.error(e));
        let postings_off = u64_()?;
        let freqs_off = u64_()?;
        let skip_off = u64_()?;
        let positions_off = u64_()?;
        let ttf = u64_()?;
        let df = r.read_u32().map_err(|e| stream.error(e))?;
        let max_tf = r.read_u32().map_err(|e| stream.error(e))?;
        let min_norm = r.read_u8().map_err(|e| stream.error(e))?;
        r.expect_zeros(7).map_err(|e| stream.error(e))?;
        let (postings_end, freqs_end, positions_end) = if last {
            (
                self.lists.postings.len(),
                self.lists.freqs.len(),
                self.lists.positions.map_or(0, |p| p.len()),
            )
        } else {
            let mut u64_ = || r.read_u64().map_err(|e| stream.error(e));
            let p = u64_()?;
            let f = u64_()?;
            u64_()?;
            let x = u64_()?;
            (p, f, x)
        };

        let blocks = u64::from(df).div_ceil(u64::try_from(BLOCK_LEN).unwrap_or(128));
        if df == 0 || df > self.doc_count {
            return Err(self.corrupt(at, "a df of 0 or above doc_count"));
        }
        let ttf_bound = u64::from(df).saturating_mul(u64::from(max_tf));
        if max_tf == 0 || ttf < u64::from(df) || ttf < u64::from(max_tf) || ttf > ttf_bound {
            return Err(self.corrupt(at, "ttf or max_tf inconsistent with df"));
        }
        if self.keyword && min_norm != 0 {
            return Err(self.corrupt(at, "a min_norm in a keyword field"));
        }
        // Lists are back to back in term order (SPEC §5.3.1): each starts where the previous ends, so a list's end
        // is the next one's start, and no list is empty.
        let range =
            |start: u64, end: u64, len: u64| (start < end && end <= len).then_some(start..end);
        let postings = range(postings_off, postings_end, self.lists.postings.len())
            .ok_or_else(|| self.corrupt(at, "postings offsets"))?;
        let freqs = range(freqs_off, freqs_end, self.lists.freqs.len())
            .ok_or_else(|| self.corrupt(at, "freqs offsets"))?;
        let positions = match self.lists.positions {
            Some(p) => Some(
                range(positions_off, positions_end, p.len())
                    .ok_or_else(|| self.corrupt(at, "positions offsets"))?,
            ),
            None if positions_off == 0 => None,
            None => return Err(self.corrupt(at, "a positions_off without Positions")),
        };
        // Lists are back to back from the start of their streams (SPEC §5.3.1).
        let starts_late = postings.start != 0
            || freqs.start != 0
            || positions.as_ref().is_some_and(|p| p.start != 0);
        if ordinal == 0 && starts_late {
            return Err(self.corrupt(at, "bytes before the first list"));
        }
        let skip = if u64::from(df) >= u64::try_from(BLOCK_LEN).unwrap_or(128) {
            let fits = blocks
                .checked_mul(SKIP_ENTRY_LEN)
                .and_then(|n| skip_off.checked_add(n))
                .zip(self.lists.skip.map(|s| s.len()))
                .is_some_and(|(end, len)| end <= len);
            if !fits {
                return Err(self.corrupt(at, "skip_off, or a list of 128 or more without Skip"));
            }
            Some(skip_off)
        } else if skip_off != 0 {
            return Err(self.corrupt(at, "a skip_off for a list shorter than 128"));
        } else {
            None
        };
        Ok(TermInfo {
            ordinal,
            df,
            ttf,
            max_tf,
            min_norm,
            postings,
            freqs,
            skip,
            positions,
            owner: self.owner,
        })
    }
}

/// The inverted index of one `text` or `keyword` field of a segment: its terms and their posting lists.
///
/// A field without terms in the segment (no sections, SPEC §5.3 "Presence") reads as empty.
#[derive(Debug, Clone)]
pub struct TermIndex<'s> {
    dict: Option<&'s fst::Map<ByteSource>>,
    infos: Option<InfoReader<'s>>,
}

impl<'s> TermIndex<'s> {
    pub(super) fn empty() -> Self {
        Self {
            dict: None,
            infos: None,
        }
    }

    pub(super) fn new(
        dict: &'s fst::Map<ByteSource>,
        info: Stream<'s>,
        lists: Lists<'s>,
        doc_count: u32,
        keyword: bool,
        owner: (u128, u32),
    ) -> Self {
        let count = u64::try_from(dict.len()).unwrap_or(u64::MAX);
        Self {
            dict: Some(dict),
            infos: Some(InfoReader {
                info: Cursor::new(info),
                count,
                doc_count,
                keyword,
                lists,
                owner,
            }),
        }
    }

    /// Number of distinct terms.
    pub fn len(&self) -> u64 {
        self.infos.as_ref().map_or(0, |i| i.count)
    }

    /// Whether the field has no terms in this segment.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The term info of `term`, if the field has it.
    ///
    /// # Errors
    /// [`ReadError::Section`] or [`ReadError::Block`] if its record is corrupt.
    pub fn get(&mut self, term: &[u8]) -> Result<Option<TermInfo>, ReadError> {
        let (Some(dict), Some(infos)) = (self.dict, self.infos.as_mut()) else {
            return Ok(None);
        };
        dict.get(term)
            .map(|ordinal| infos.info(ordinal))
            .transpose()
    }

    /// The term info of the term with rank `ordinal`.
    ///
    /// # Errors
    /// As [`Self::get`]; [`ReadError::OrdinalOutOfRange`] for an `ordinal` at or above [`Self::len`].
    pub fn info(&mut self, ordinal: u64) -> Result<TermInfo, ReadError> {
        let len = self.len();
        match self.infos.as_mut() {
            Some(infos) if ordinal < len => infos.info(ordinal),
            _ => Err(ReadError::OrdinalOutOfRange { ordinal, len }),
        }
    }

    /// The posting list of a term this index returned.
    ///
    /// # Errors
    /// [`ReadError::ForeignTermInfo`] for a [`TermInfo`] of another field or segment.
    pub fn postings(&self, info: &TermInfo) -> Result<Postings<'s>, ReadError> {
        match &self.infos {
            Some(i) if i.owner == info.owner => Ok(Postings::new(
                info.clone(),
                Some(i.lists),
                i.doc_count,
                i.keyword,
            )),
            _ => Err(ReadError::ForeignTermInfo),
        }
    }

    /// The terms in `range` (byte order), with their term info.
    pub fn range(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> TermStream<'s, AlwaysMatch> {
        self.search_range(AlwaysMatch, lower, upper)
    }

    /// The terms that start with `prefix`.
    pub fn prefix(&self, prefix: &[u8]) -> TermStream<'s, AlwaysMatch> {
        let upper = prefix_successor(prefix);
        let upper = upper.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
        self.range(Bound::Included(prefix), upper)
    }

    /// The terms `automaton` accepts, e.g. a Levenshtein automaton for fuzzy matching (SPEC §7.6, used in M8).
    pub fn search<A: Automaton>(&self, automaton: A) -> TermStream<'s, A> {
        self.search_range(automaton, Bound::Unbounded, Bound::Unbounded)
    }

    /// The terms in `range` that `automaton` accepts.
    pub fn search_range<A: Automaton>(
        &self,
        automaton: A,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> TermStream<'s, A> {
        let (Some(dict), Some(infos)) = (self.dict, self.infos.clone()) else {
            return TermStream {
                stream: None,
                infos: None,
            };
        };
        let mut b = dict.search(automaton);
        b = match lower {
            Bound::Included(t) => b.ge(t),
            Bound::Excluded(t) => b.gt(t),
            Bound::Unbounded => b,
        };
        b = match upper {
            Bound::Included(t) => b.le(t),
            Bound::Excluded(t) => b.lt(t),
            Bound::Unbounded => b,
        };
        TermStream {
            stream: Some(b.into_stream()),
            infos: Some(infos),
        }
    }
}

/// The smallest byte string above every string that starts with `prefix`; `None` if there is none (`prefix` is
/// empty or all `0xFF`).
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let keep = prefix.iter().rposition(|&b| b != u8::MAX)?;
    let mut out = prefix.get(..=keep)?.to_vec();
    let last = out.last_mut()?;
    *last = last.checked_add(1)?;
    Some(out)
}

/// Terms of a field in byte order, from [`TermIndex::range`], [`TermIndex::prefix`] or [`TermIndex::search`].
pub struct TermStream<'s, A: Automaton> {
    stream: Option<fst::map::Stream<'s, A>>,
    infos: Option<InfoReader<'s>>,
}

impl<A: Automaton> core::fmt::Debug for TermStream<'_, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TermStream").finish_non_exhaustive()
    }
}

impl<A: Automaton> TermStream<'_, A> {
    /// The next term and its info, or `None` after the last.
    ///
    /// # Errors
    /// As [`TermIndex::get`].
    #[allow(
        clippy::should_implement_trait,
        reason = "a lending stream: the term borrows it"
    )]
    pub fn next(&mut self) -> Option<Result<(&[u8], TermInfo), ReadError>> {
        let (stream, infos) = (self.stream.as_mut()?, self.infos.as_mut()?);
        let (term, ordinal) = stream.next()?;
        Some(infos.info(ordinal).map(|info| (term, info)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_successors() {
        assert_eq!(prefix_successor(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(prefix_successor(b"a\xff\xff"), Some(b"b".to_vec()));
        assert_eq!(prefix_successor(b"\xff"), None);
        assert_eq!(prefix_successor(b""), None);
    }
}
