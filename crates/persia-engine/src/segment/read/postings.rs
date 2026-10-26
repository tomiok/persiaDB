//! Posting-list iteration (SPEC §5.2.1, §5.3.2, §5.3.3; ROADMAP 5.3.3, 5.3.4, 5.3.7): docids block by block, with
//! skipping through `Skip` entries, frequencies and positions decoded only when asked for, block-max data for
//! block-max WAND, and tombstoned documents left out.

use core::ops::Range;

use persia_format::Reader;
use persia_format::bitpack::BLOCK_LEN;
use persia_format::postings::advance_to;
use roaring::RoaringBitmap;

use super::error::{ReadError, SegmentCorruption};
use super::stream::Cursor;
use super::terms::{Lists, SKIP_ENTRY_LEN, TermInfo};

/// The deleted documents of one segment (SPEC §8.3): left out of every [`Postings`] they are applied to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tombstones {
    deleted: RoaringBitmap,
}

impl Tombstones {
    /// The tombstones `deleted` of a segment of `doc_count` documents.
    ///
    /// # Errors
    /// [`ReadError::Tombstone`] (corruption, SPEC §8.3) if a docid is at or above `doc_count`.
    pub fn new(deleted: RoaringBitmap, doc_count: u32) -> Result<Self, ReadError> {
        match deleted.max() {
            Some(docid) if docid >= doc_count => Err(ReadError::Tombstone { docid, doc_count }),
            _ => Ok(Self { deleted }),
        }
    }

    /// Whether `docid` is deleted.
    pub fn contains(&self, docid: u32) -> bool {
        self.deleted.contains(docid)
    }

    /// Number of deleted documents.
    pub fn len(&self) -> u64 {
        self.deleted.len()
    }

    /// Whether no document is deleted.
    pub fn is_empty(&self) -> bool {
        self.deleted.is_empty()
    }
}

/// Block-level score bounds of a postings block (SPEC §5.3.2), for block-max WAND (SPEC §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockMax {
    /// The block's last docid: the bounds hold for documents up to it.
    pub last_docid: u32,
    /// Largest frequency in the block.
    pub max_tf: u32,
    /// Smallest norm byte of the block's documents (SPEC §5.3.4); 0 for a `keyword` field.
    pub min_norm: u8,
}

/// A `Skip` entry (SPEC §5.3.2).
#[derive(Debug, Clone, Copy)]
struct SkipEntry {
    last_docid: u32,
    max_tf: u32,
    postings_off: u64,
    freqs_off: u64,
    positions_off: u64,
    min_norm: u8,
}

/// Where block `k` of a list lies, and what bounds it.
#[derive(Debug, Clone)]
struct Extent {
    postings: Range<u64>,
    freqs: Range<u64>,
    positions: Option<Range<u64>>,
    /// The previous block's last docid.
    prev: Option<u32>,
    /// The block's last docid, from its skip entry.
    last: Option<u32>,
    max_tf: u32,
}

#[derive(Debug)]
struct ListCursors<'s> {
    postings: Cursor<'s>,
    freqs: Cursor<'s>,
    skip: Option<Cursor<'s>>,
    positions: Option<Cursor<'s>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Unstarted,
    At,
    Done,
}

/// The documents of one term, in docid order, with their frequencies and positions.
///
/// A new iterator is before its first document: [`Self::next`] or [`Self::advance`] moves it onto one. Docids are
/// checked as they are decoded (order, bounds, agreement with the skip entries); any violation is a
/// [`ReadError`], after which the iterator is exhausted.
///
/// ```
/// # use persia_analysis::AnalyzerRegistry;
/// # use persia_engine::document::{Document, Value};
/// # use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder};
/// # use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
/// # use persia_format::ByteSource;
/// # let schema = SchemaBuilder::new("c").field("tag", FieldKind::Keyword(KeywordOptions::default())).build()?;
/// # let tag = schema.field("tag").map(|f| f.id()).ok_or("no field")?;
/// # let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
/// for i in 0..300 {
///     let value = if i % 3 == 0 { "fizz" } else { "other" };
///     b.add(&Document::new(format!("d{i}")).with(tag, Value::String(value.into())))?;
/// }
/// # let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
/// # let bytes = SegmentWriter::new(&b, meta).finish()?;
/// let segment = Segment::open(ByteSource::new(bytes.into()), &schema)?;
/// let mut terms = segment.terms(tag)?;
/// let info = terms.get(b"fizz")?.ok_or("no term")?;
/// let mut postings = terms.postings(&info)?;
/// assert_eq!(postings.next()?, Some(0));
/// assert_eq!(postings.advance(200)?, Some(201)); // skips whole blocks through the Skip entries
/// assert_eq!(postings.freq()?, 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Postings<'s> {
    info: TermInfo,
    doc_count: u32,
    keyword: bool,
    lists: Option<ListCursors<'s>>,
    block_count: u32,
    /// The term's skip entries read so far (a validated prefix).
    entries: Vec<SkipEntry>,
    /// The decoded block, its docids and the current one.
    block: Option<u32>,
    docs: [u32; BLOCK_LEN],
    len: usize,
    cur: usize,
    state: State,
    /// The block whose frequencies `tfs` holds.
    tfs_of: Option<u32>,
    tfs: [u32; BLOCK_LEN],
    /// The block whose positions `positions` holds; `position_ends[i]` ends document `i`'s.
    positions_of: Option<u32>,
    positions: Vec<u32>,
    position_ends: Vec<usize>,
    tombstones: Option<&'s Tombstones>,
}

impl<'s> Postings<'s> {
    pub(super) fn new(
        info: TermInfo,
        lists: Option<Lists<'s>>,
        doc_count: u32,
        keyword: bool,
    ) -> Self {
        let block_len = u32::try_from(BLOCK_LEN).unwrap_or(128);
        let block_count = info.df().div_ceil(block_len);
        let lists = lists.map(|l| ListCursors {
            postings: Cursor::new(l.postings),
            freqs: Cursor::new(l.freqs),
            skip: l.skip.map(Cursor::new),
            positions: l.positions.map(Cursor::new),
        });
        let state = if lists.is_some() {
            State::Unstarted
        } else {
            State::Done
        };
        Self {
            info,
            doc_count,
            keyword,
            lists,
            block_count,
            entries: Vec::new(),
            block: None,
            docs: [0; BLOCK_LEN],
            len: 0,
            cur: 0,
            state,
            tfs_of: None,
            tfs: [0; BLOCK_LEN],
            positions_of: None,
            positions: Vec::new(),
            position_ends: Vec::new(),
            tombstones: None,
        }
    }

    /// Leaves out the documents `tombstones` deletes (ROADMAP 5.3.7). They must be this segment's.
    #[must_use]
    pub fn with_tombstones(mut self, tombstones: &'s Tombstones) -> Self {
        self.tombstones = Some(tombstones);
        self
    }

    /// The term's document frequency: an upper bound on the documents this iterator yields.
    pub fn df(&self) -> u32 {
        self.info.df()
    }

    /// The current document, `None` before the first and after the last.
    pub fn doc(&self) -> Option<u32> {
        match self.state {
            State::At => self.docs.get(self.cur).copied(),
            State::Unstarted | State::Done => None,
        }
    }

    /// Moves to the next document and returns it, or `None` at the end.
    ///
    /// # Errors
    /// [`ReadError::Section`] or [`ReadError::Block`] if the list is corrupt; the iterator is then exhausted.
    #[allow(
        clippy::should_implement_trait,
        reason = "fallible: an Iterator of Results would hide the end of a corrupt list"
    )]
    pub fn next(&mut self) -> Result<Option<u32>, ReadError> {
        let result = self.next_live();
        self.done_on_error(result)
    }

    /// Moves to the first document at or after `target` and returns it, or `None` if there is none. Never moves
    /// back: when the current document is already at or after `target`, it stays. Skips whole blocks through the
    /// `Skip` entries without decoding them.
    ///
    /// # Errors
    /// As [`Self::next`].
    pub fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        let result = self.advance_inner(target);
        self.done_on_error(result)
    }

    /// The current document's frequency (occurrences of the term in the field, at least 1); 0 when not on a
    /// document. Decodes the block's frequencies the first time one is asked for.
    ///
    /// # Errors
    /// As [`Self::next`].
    pub fn freq(&mut self) -> Result<u32, ReadError> {
        if self.state != State::At {
            return Ok(0);
        }
        let result = self
            .load_freqs()
            .map(|()| self.tfs.get(self.cur).copied().unwrap_or(0));
        self.done_on_error(result)
    }

    /// The current document's positions in the field (SPEC §5.3.3), ascending (stacked tokens may repeat one);
    /// empty when not on a document or when the field does not record positions.
    ///
    /// # Errors
    /// As [`Self::next`].
    pub fn positions(&mut self) -> Result<&[u32], ReadError> {
        if self.state != State::At || self.info.positions.is_none() {
            return Ok(&[]);
        }
        if let Err(e) = self.load_positions() {
            self.state = State::Done;
            return Err(e);
        }
        let start = match self.cur.checked_sub(1) {
            Some(prev) => self.position_ends.get(prev).copied().unwrap_or(0),
            None => 0,
        };
        let end = self.position_ends.get(self.cur).copied().unwrap_or(0);
        Ok(self.positions.get(start..end).unwrap_or_default())
    }

    /// The score bounds of the current document's block; `None` when not on a document.
    pub fn block_max(&self) -> Option<BlockMax> {
        if self.state != State::At {
            return None;
        }
        let k = usize::try_from(self.block?).ok()?;
        if self.info.skip.is_some() {
            let e = self.entries.get(k)?;
            return Some(block_max(e));
        }
        Some(BlockMax {
            last_docid: self.docs.get(self.len.checked_sub(1)?).copied()?,
            max_tf: self.info.max_tf(),
            min_norm: self.info.min_norm(),
        })
    }

    /// The score bounds of the block that holds the list's first document at or after `target`, wherever the
    /// iterator is (an earlier block too), without moving it or decoding docids when the list has skip entries;
    /// `None` if no document is at or after `target`. For block-max WAND (SPEC §7.3):
    /// tombstones are not applied, so the bounds may cover deleted documents.
    ///
    /// # Errors
    /// As [`Self::next`].
    pub fn shallow_advance(&mut self, target: u32) -> Result<Option<BlockMax>, ReadError> {
        let result = self.shallow_inner(target);
        self.done_on_error(result)
    }

    fn done_on_error<T>(&mut self, result: Result<T, ReadError>) -> Result<T, ReadError> {
        if result.is_err() {
            self.state = State::Done;
        }
        result
    }

    fn deleted(&self, docid: u32) -> bool {
        self.tombstones.is_some_and(|t| t.contains(docid))
    }

    fn next_live(&mut self) -> Result<Option<u32>, ReadError> {
        loop {
            match self.step()? {
                Some(d) if self.deleted(d) => {}
                other => return Ok(other),
            }
        }
    }

    /// The next document, deleted or not.
    fn step(&mut self) -> Result<Option<u32>, ReadError> {
        match self.state {
            State::Done => Ok(None),
            State::Unstarted => self.enter(0, 0),
            State::At => {
                let next = self.cur.saturating_add(1);
                if next < self.len {
                    self.cur = next;
                    return Ok(self.doc());
                }
                let k = self.block.map_or(0, |b| b.saturating_add(1));
                if k < self.block_count {
                    self.enter(k, 0)
                } else {
                    self.state = State::Done;
                    Ok(None)
                }
            }
        }
    }

    /// Moves to document `i` of block `k`, decoding the block if needed.
    fn enter(&mut self, k: u32, i: usize) -> Result<Option<u32>, ReadError> {
        if self.block != Some(k) {
            self.load_block(k)?;
        }
        if i >= self.len {
            self.state = State::Done;
            return Ok(None);
        }
        self.cur = i;
        self.state = State::At;
        Ok(self.doc())
    }

    fn advance_inner(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        match self.state {
            State::Done => return Ok(None),
            State::At if self.doc().is_some_and(|d| d >= target) => return Ok(self.doc()),
            State::At | State::Unstarted => {}
        }
        let mut k = match self.state {
            State::At => self.block.unwrap_or(0),
            State::Unstarted | State::Done => 0,
        };
        if self.info.skip.is_some() {
            while k < self.block_count && self.entry(k)?.last_docid < target {
                k = k.saturating_add(1);
            }
            if k >= self.block_count {
                self.state = State::Done;
                return Ok(None);
            }
        }
        let from = if self.state == State::At && self.block == Some(k) {
            self.cur
        } else {
            0
        };
        if self.block != Some(k) {
            self.load_block(k)?;
        }
        let docs = self.docs.get(..self.len).unwrap_or_default();
        let i = advance_to(docs, from, target);
        match self.enter(k, i)? {
            Some(d) if self.deleted(d) => self.next_live(),
            other => Ok(other),
        }
    }

    fn shallow_inner(&mut self, target: u32) -> Result<Option<BlockMax>, ReadError> {
        if self.lists.is_none() {
            return Ok(None);
        }
        if self.info.skip.is_none() {
            if self.block != Some(0) {
                self.load_block(0)?;
            }
            let last = self
                .docs
                .get(self.len.saturating_sub(1))
                .copied()
                .unwrap_or(0);
            return Ok((last >= target).then_some(BlockMax {
                last_docid: last,
                max_tf: self.info.max_tf(),
                min_norm: self.info.min_norm(),
            }));
        }
        // Position-independent: the entries read so far are a prefix from block 0, so binary-search them, and read
        // further ones only past it.
        let cached = self.entries.partition_point(|e| e.last_docid < target);
        let mut k = u32::try_from(cached).unwrap_or(u32::MAX);
        while k < self.block_count {
            let e = self.entry(k)?;
            if e.last_docid >= target {
                return Ok(Some(block_max(&e)));
            }
            k = k.saturating_add(1);
        }
        Ok(None)
    }

    /// Documents in block `k`: 128, but the last block holds the rest.
    fn count_in(&self, k: u32) -> usize {
        let before = u64::from(k).saturating_mul(128);
        let left = u64::from(self.info.df()).saturating_sub(before);
        usize::try_from(left.min(128)).unwrap_or(BLOCK_LEN)
    }

    /// A failed allocation while reading `kind`: a resource error, not corruption.
    fn out_of_memory(&self, kind: Which) -> ReadError {
        let source = persia_format::Error::OutOfMemory { bytes: None };
        let stream = self.lists.as_ref().map(|l| match kind {
            Which::Skip => l.skip.as_ref().map_or(l.postings.stream, |s| s.stream),
            Which::Postings | Which::Freqs | Which::Positions => l.postings.stream,
        });
        match stream {
            Some(s) => s.error(source),
            None => ReadError::Format(source),
        }
    }

    fn corrupt(&self, kind: Which, position: u64, what: &'static str) -> ReadError {
        let reason = SegmentCorruption::Invalid { what };
        match (&self.lists, kind) {
            (Some(l), Which::Postings) => l.postings.stream.corrupt(position, reason),
            (Some(l), Which::Freqs) => l.freqs.stream.corrupt(position, reason),
            (Some(l), Which::Skip) => match &l.skip {
                Some(s) => s.stream.corrupt(position, reason),
                None => l.postings.stream.corrupt(position, reason),
            },
            (Some(l), Which::Positions) => match &l.positions {
                Some(p) => p.stream.corrupt(position, reason),
                None => l.postings.stream.corrupt(position, reason),
            },
            (None, _) => ReadError::Section {
                kind: super::super::SectionKind::Postings,
                field: u32::MAX,
                position,
                reason,
            },
        }
    }

    /// Skip entry `k`, reading and validating the entries before it first.
    fn entry(&mut self, k: u32) -> Result<SkipEntry, ReadError> {
        let k = usize::try_from(k).unwrap_or(usize::MAX);
        while self.entries.len() <= k {
            let e = self.read_entry(self.entries.len())?;
            // At most one entry per 128 docids, but a long list can still need more than the allocator has.
            if self.entries.try_reserve(1).is_err() {
                return Err(self.out_of_memory(Which::Skip));
            }
            self.entries.push(e);
        }
        self.entries
            .get(k)
            .copied()
            .ok_or_else(|| self.corrupt(Which::Skip, 0, "a missing skip entry"))
    }

    fn read_entry(&mut self, j: usize) -> Result<SkipEntry, ReadError> {
        let skip_off = self
            .info
            .skip
            .ok_or_else(|| self.corrupt(Which::Skip, 0, "a missing skip entry"))?;
        let j_u32 = u32::try_from(j).unwrap_or(u32::MAX);
        if j_u32 >= self.block_count {
            return Err(self.corrupt(Which::Skip, skip_off, "a skip entry past the list"));
        }
        let at = u64::try_from(j)
            .ok()
            .and_then(|j| j.checked_mul(SKIP_ENTRY_LEN))
            .and_then(|o| o.checked_add(skip_off))
            .ok_or_else(|| self.corrupt(Which::Skip, skip_off, "skip_off"))?;
        let cursor = self
            .lists
            .as_mut()
            .and_then(|l| l.skip.as_mut())
            .ok_or_else(|| missing(Which::Skip))?;
        let stream = cursor.stream;
        let mut r = cursor.reader(at, at.saturating_add(SKIP_ENTRY_LEN))?;
        let e = (|| {
            let last_docid = r.read_u32()?;
            let max_tf = r.read_u32()?;
            let postings_off = r.read_u64()?;
            let freqs_off = r.read_u64()?;
            let positions_off = r.read_u64()?;
            let min_norm = r.read_u8()?;
            r.expect_zeros(7)?;
            Ok(SkipEntry {
                last_docid,
                max_tf,
                postings_off,
                freqs_off,
                positions_off,
                min_norm,
            })
        })()
        .map_err(|e| stream.error(e))?;

        let bad = |what| Err(self.corrupt(Which::Skip, at, what));
        let info = &self.info;
        let positions_start = info.positions.as_ref().map_or(0, |p| p.start);
        let positions_end = info.positions.as_ref().map_or(1, |p| p.end);
        match self.entries.last() {
            None => {
                if (e.postings_off, e.freqs_off, e.positions_off)
                    != (info.postings.start, info.freqs.start, positions_start)
                {
                    return bad("the first skip entry's offsets differ from the term info's");
                }
            }
            Some(p) => {
                let positions_grow = info.positions.is_none() || e.positions_off > p.positions_off;
                if e.postings_off <= p.postings_off || e.freqs_off <= p.freqs_off || !positions_grow
                {
                    return bad("skip entry offsets that do not increase");
                }
            }
        }
        if info.positions.is_none() && e.positions_off != 0 {
            return bad("a positions_off without Positions");
        }
        if e.postings_off >= info.postings.end
            || e.freqs_off >= info.freqs.end
            || e.positions_off >= positions_end
        {
            return bad("skip entry offsets past the term's lists");
        }
        // The block's `n` docids follow the previous block's last: its last is at least `prev + n`.
        let n = u64::try_from(self.count_in(j_u32)).unwrap_or(128);
        let min_last = self
            .entries
            .last()
            .map_or(0, |p| u64::from(p.last_docid).saturating_add(1))
            .saturating_add(n.saturating_sub(1));
        if u64::from(e.last_docid) < min_last || e.last_docid >= self.doc_count {
            return bad("a skip entry's last_docid");
        }
        if e.max_tf == 0 || e.max_tf > info.max_tf() || e.min_norm < info.min_norm() {
            return bad("skip entry bounds outside the term's");
        }
        if self.keyword && e.min_norm != 0 {
            return bad("a min_norm in a keyword field");
        }
        Ok(e)
    }

    fn extent(&mut self, k: u32) -> Result<Extent, ReadError> {
        if self.info.skip.is_none() {
            return Ok(Extent {
                postings: self.info.postings.clone(),
                freqs: self.info.freqs.clone(),
                positions: self.info.positions.clone(),
                prev: None,
                last: None,
                max_tf: self.info.max_tf(),
            });
        }
        let e = self.entry(k)?;
        let next_k = k.saturating_add(1);
        let next = if next_k < self.block_count {
            Some(self.entry(next_k)?)
        } else {
            None
        };
        let prev = match k.checked_sub(1) {
            Some(p) => Some(self.entry(p)?.last_docid),
            None => None,
        };
        let info = &self.info;
        Ok(Extent {
            postings: e.postings_off..next.map_or(info.postings.end, |n| n.postings_off),
            freqs: e.freqs_off..next.map_or(info.freqs.end, |n| n.freqs_off),
            positions: info
                .positions
                .as_ref()
                .map(|p| e.positions_off..next.map_or(p.end, |n| n.positions_off)),
            prev,
            last: Some(e.last_docid),
            max_tf: e.max_tf,
        })
    }

    /// Decodes block `k`'s docids.
    fn load_block(&mut self, k: u32) -> Result<(), ReadError> {
        self.block = None;
        self.tfs_of = None;
        self.positions_of = None;
        let ext = self.extent(k)?;
        let n = self.count_in(k);
        let lists = self
            .lists
            .as_mut()
            .ok_or_else(|| missing(Which::Postings))?;
        let stream = lists.postings.stream;
        // The largest a block can be (SPEC §5.2.1), checked before anything is read: a crafted extent must not make
        // the reader decode more.
        let max = if n == BLOCK_LEN {
            1 + 4 + 16 * 32
        } else {
            1 + 5 * MAX_TAIL
        };
        if ext.postings.end.saturating_sub(ext.postings.start) > max {
            return Err(stream.corrupt(
                ext.postings.start,
                SegmentCorruption::Invalid {
                    what: "a postings block longer than any block can be",
                },
            ));
        }
        let mut r = lists
            .postings
            .reader(ext.postings.start, ext.postings.end)?;
        if n == BLOCK_LEN {
            r.read_postings_block(ext.prev, &mut self.docs)
                .map_err(|e| stream.error(e))?;
        } else {
            let got = r
                .read_postings_tail(ext.prev, &mut self.docs)
                .map_err(|e| stream.error(e))?;
            if got != n {
                return Err(stream.corrupt(
                    ext.postings.start,
                    SegmentCorruption::Invalid {
                        what: "a tail count other than df mod 128",
                    },
                ));
            }
        }
        if !r.is_empty() {
            return Err(stream.corrupt(r.offset(), SegmentCorruption::TrailingBytes));
        }
        let last = self.docs.get(n.saturating_sub(1)).copied().unwrap_or(0);
        if last >= self.doc_count {
            return Err(stream.corrupt(
                ext.postings.start,
                SegmentCorruption::Invalid {
                    what: "a docid at or above doc_count",
                },
            ));
        }
        if ext.last.is_some_and(|l| l != last) {
            return Err(stream.corrupt(
                ext.postings.start,
                SegmentCorruption::Invalid {
                    what: "a block whose last docid differs from its skip entry",
                },
            ));
        }
        self.len = n;
        self.block = Some(k);
        Ok(())
    }

    /// Decodes the current block's frequencies.
    fn load_freqs(&mut self) -> Result<(), ReadError> {
        let Some(k) = self.block else {
            return Ok(());
        };
        if self.tfs_of == Some(k) {
            return Ok(());
        }
        let ext = self.extent(k)?;
        let n = self.len;
        let lists = self.lists.as_mut().ok_or_else(|| missing(Which::Freqs))?;
        let stream = lists.freqs.stream;
        let n_u64 = u64::try_from(n).unwrap_or(128);
        if ext.freqs.end.saturating_sub(ext.freqs.start) > max_run_len(n_u64) {
            return Err(stream.corrupt(
                ext.freqs.start,
                SegmentCorruption::Invalid {
                    what: "a frequency run longer than any run of its length can be",
                },
            ));
        }
        let mut r = lists.freqs.reader(ext.freqs.start, ext.freqs.end)?;
        let tfs = self.tfs.get_mut(..n).unwrap_or_default();
        read_run_into(&mut r, tfs).map_err(|e| stream.error(e))?;
        if !r.is_empty() {
            return Err(stream.corrupt(r.offset(), SegmentCorruption::TrailingBytes));
        }
        for tf in &mut *tfs {
            *tf = tf
                .checked_add(1)
                .filter(|&t| t <= ext.max_tf)
                .ok_or_else(|| {
                    stream.corrupt(
                        ext.freqs.start,
                        SegmentCorruption::Invalid {
                            what: "a frequency above its block's max_tf",
                        },
                    )
                })?;
        }
        self.tfs_of = Some(k);
        Ok(())
    }

    /// Decodes the current block's positions run (SPEC §5.3.3).
    fn load_positions(&mut self) -> Result<(), ReadError> {
        let Some(k) = self.block else {
            return Ok(());
        };
        if self.positions_of == Some(k) {
            return Ok(());
        }
        self.load_freqs()?;
        let ext = self.extent(k)?;
        let Some(range) = ext.positions else {
            return Ok(());
        };
        let tfs = self.tfs.get(..self.len).unwrap_or_default();
        let total = tfs
            .iter()
            .fold(0_u64, |s, &t| s.saturating_add(u64::from(t)));
        let lists = self
            .lists
            .as_mut()
            .ok_or_else(|| missing(Which::Positions))?;
        let cursor = lists
            .positions
            .as_mut()
            .ok_or_else(|| missing(Which::Positions))?;
        let stream = cursor.stream;
        // Every value costs at least one byte per 128 (a packed group of width 0): a bound before allocating.
        let bytes = range.end.saturating_sub(range.start);
        if total > bytes.saturating_mul(128)
            || total > self.info.ttf()
            || bytes > max_run_len(total)
        {
            return Err(stream.corrupt(
                range.start,
                SegmentCorruption::Invalid {
                    what: "a positions run whose length does not fit its frequencies",
                },
            ));
        }
        if total > MAX_BLOCK_POSITIONS {
            // A valid segment may hold such a block, but this reader decodes a block's positions at once.
            return Err(stream.error(persia_format::Error::OutOfMemory {
                bytes: total.checked_mul(4),
            }));
        }
        let total_usize = usize::try_from(total).unwrap_or(usize::MAX);
        self.positions.clear();
        self.positions.try_reserve_exact(total_usize).map_err(|_| {
            stream.error(persia_format::Error::OutOfMemory {
                bytes: total.checked_mul(4),
            })
        })?;
        self.positions.resize(total_usize, 0);
        let mut r = cursor.reader(range.start, range.end)?;
        read_run_into(&mut r, &mut self.positions).map_err(|e| stream.error(e))?;
        if !r.is_empty() {
            return Err(stream.corrupt(r.offset(), SegmentCorruption::TrailingBytes));
        }
        // Per document: the first position as is, then deltas.
        self.position_ends.clear();
        let mut at = 0_usize;
        for &tf in tfs {
            let end = at.saturating_add(usize::try_from(tf).unwrap_or(usize::MAX));
            let mut position = 0_u32;
            for p in self.positions.get_mut(at..end).unwrap_or_default() {
                position = position.checked_add(*p).ok_or_else(|| {
                    stream.corrupt(
                        range.start,
                        SegmentCorruption::Invalid {
                            what: "a position above u32::MAX",
                        },
                    )
                })?;
                *p = position;
            }
            self.position_ends.push(end);
            at = end;
        }
        self.positions_of = Some(k);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum Which {
    Postings,
    Freqs,
    Skip,
    Positions,
}

fn block_max(e: &SkipEntry) -> BlockMax {
    BlockMax {
        last_docid: e.last_docid,
        max_tf: e.max_tf,
        min_norm: e.min_norm,
    }
}

/// Reads a run of `out.len()` values (SPEC §5.3.2): bit-packed 128-groups, then the rest as varints.
fn read_run_into(r: &mut Reader<'_>, out: &mut [u32]) -> persia_format::Result<()> {
    let (groups, rest) = out.as_chunks_mut::<BLOCK_LEN>();
    for group in groups {
        let width = r.read_bit_width()?;
        r.read_packed_block(width, group)?;
    }
    for v in rest {
        *v = r.read_varint_u32()?;
    }
    Ok(())
}

/// A posting list without the section it needs: only an iterator of an empty index has none.
fn missing(which: Which) -> ReadError {
    let kind = match which {
        Which::Postings => super::super::SectionKind::Postings,
        Which::Freqs => super::super::SectionKind::Freqs,
        Which::Skip => super::super::SectionKind::Skip,
        Which::Positions => super::super::SectionKind::Positions,
    };
    ReadError::Section {
        kind,
        field: u32::MAX,
        position: 0,
        reason: SegmentCorruption::Invalid {
            what: "a posting list without its section",
        },
    }
}

/// Most docids in a tail block.
const MAX_TAIL: u64 = 127;
/// Most positions this reader decodes for one postings block (256 MiB of `u32`).
const MAX_BLOCK_POSITIONS: u64 = 1 << 26;

/// The longest a run of `n` values can be (SPEC §5.3.2): a width byte and 512 bytes per 128-group, then a varint of
/// at most 5 bytes per value.
fn max_run_len(n: u64) -> u64 {
    (n / 128)
        .saturating_mul(1 + 16 * 32)
        .saturating_add((n % 128).saturating_mul(5))
}
