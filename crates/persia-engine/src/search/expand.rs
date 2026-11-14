//! `prefix` and `fuzzy` expansion over the collection's terms, once per plan (SPEC §7.1, ROADMAP 7.4.12): every
//! view's terms are walked, and the `max_expansions` with the highest collection `df` are kept, ties by bytes.
//!
//! The prefix and the fuzzy term are raw bytes. This walk does not run the field analyzer: a prefix `Run` does not
//! meet a term the analyzer stored as `running`. Scoring of the terms kept (one shared `idf`, and a fuzzy
//! edit-distance weight) is in `plan`. M8 typo tolerance must analyze before it builds these queries.

use core::cmp::Reverse;
use core::ops::Bound;
use std::collections::{BTreeSet, BinaryHeap};
use std::sync::Arc;

use fst::Automaton;

use crate::memtable::MemtableView;
use crate::schema::FieldId;
use crate::segment::{ReadError, SectionKind, Segment, SegmentCorruption};
use crate::stats::{MemtableDf, SegmentsDf, StatisticsProvider, TermCounts};
use crate::view::{SearchView, TermEnum};

/// What [`TermSource::each_term`] calls with each term: its bytes, and what the views' own walks counted of it.
pub type EachTerm<'e> = dyn FnMut(&[u8], TermCounts<'_>) -> Result<(), ReadError> + 'e;

/// The views a plan runs on, as a `prefix` or `fuzzy` expands over their terms (SPEC §7.1): a single view, or a
/// collection's segments and memtable ([`crate::db::CollectionViews`]).
pub trait TermSource {
    /// Calls `each` once with every term of the `text` or `keyword` field `field` between `lower` and `upper` (byte
    /// order) that `expansion` accepts in any view ([`SearchView::terms`]), in byte order, with what the views'
    /// walks counted of it ([`TermCounts`]): the memtable view's live documents holding it (ROADMAP 7.4.14) and the
    /// segments' documents (ROADMAP 7.4.15), so the statistics need not look it up in each view again. A source
    /// that lists a term more than once has its `df` read each time.
    ///
    /// # Errors
    /// As [`SearchView::terms`]; what `each` returns.
    fn each_term(
        &self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        expansion: &Expansion,
        each: &mut EachTerm<'_>,
    ) -> Result<(), ReadError>;
}

impl<V: SearchView> TermSource for V {
    fn each_term(
        &self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        expansion: &Expansion,
        each: &mut EachTerm<'_>,
    ) -> Result<(), ReadError> {
        let walk = TermWalk::of(self, field, (lower, upper), expansion)?;
        each_merged_term(vec![walk], each)
    }
}

/// What a view in a merged term walk is: whose counts its walk gives.
#[derive(Debug, Clone, Copy)]
enum Walked<'a> {
    Memtable(&'a MemtableView),
    Segment(&'a Segment),
    /// Neither: its counts are dropped, and the statistics look its terms up.
    Other,
}

/// One view's terms in a merged walk ([`each_merged_term`]).
pub(crate) struct TermWalk<'a> {
    terms: Box<dyn TermEnum + 'a>,
    walked: Walked<'a>,
    /// The field walked, for the error of a walk out of order.
    field: FieldId,
}

impl<'a> TermWalk<'a> {
    /// The terms of `field` in `range` that `expansion` accepts in `view`.
    ///
    /// # Errors
    /// As [`SearchView::terms`].
    pub(crate) fn of<V: SearchView>(
        view: &'a V,
        field: FieldId,
        (lower, upper): (Bound<&[u8]>, Bound<&[u8]>),
        expansion: &Expansion,
    ) -> Result<Self, ReadError> {
        let walked = match (view.as_memtable(), view.as_segment()) {
            (Some(memtable), _) => Walked::Memtable(memtable),
            (None, Some(segment)) => Walked::Segment(segment),
            (None, None) => Walked::Other,
        };
        let terms = view.terms(field, lower, upper, expansion.clone())?;
        Ok(Self {
            terms: Box::new(terms),
            walked,
            field,
        })
    }
}

/// Calls `each` once per term any of `walks` lists, in byte order, with the counts of the views that list it
/// ([`TermSource::each_term`]): the walks are merged as they stream, so no term is looked up in a view again. A
/// view that does not list a term in range holds none of it, so its count is 0: the counts are complete for the
/// views walked. The memtable's is passed only when one memtable view is walked.
///
/// # Errors
/// As [`TermEnum::next`]; what `each` returns; [`ReadError::Section`] when a walk lists a term after one not below it.
pub(crate) fn each_merged_term(
    mut walks: Vec<TermWalk<'_>>,
    each: &mut EachTerm<'_>,
) -> Result<(), ReadError> {
    let mut memtables = walks.iter().filter_map(|w| match w.walked {
        Walked::Memtable(m) => Some(m),
        _ => None,
    });
    let memtable = match (memtables.next(), memtables.next()) {
        (Some(m), None) => Some(m),
        _ => None,
    };
    let segments: Vec<&Segment> = walks
        .iter()
        .filter_map(|w| match w.walked {
            Walked::Segment(s) => Some(s),
            _ => None,
        })
        .collect();
    let counts = |memtable_df: u32, segments_df: u64| {
        TermCounts::new(
            memtable.map(|m| MemtableDf::new(m, memtable_df)),
            Some(SegmentsDf::new(&segments, segments_df)),
        )
    };
    // One view (a single view, or a collection with only a memtable): its walk is the merge.
    if let [walk] = walks.as_mut_slice() {
        let (walked, field) = (walk.walked, walk.field);
        let mut prev: Option<Vec<u8>> = None;
        while let Some((term, df)) = walk.terms.next()? {
            if prev.as_deref().is_some_and(|p| term <= p) {
                return Err(out_of_order(field));
            }
            let last = prev.get_or_insert_with(Vec::new);
            last.clear();
            last.extend_from_slice(term);
            match walked {
                Walked::Memtable(_) => each(term, counts(df, 0))?,
                Walked::Segment(_) => each(term, counts(0, u64::from(df)))?,
                Walked::Other => each(term, counts(0, 0))?,
            }
        }
        return Ok(());
    }
    // Each walk's current term, the smallest first; ties by walk.
    let mut heads = BinaryHeap::<Reverse<(Vec<u8>, usize, u32)>>::with_capacity(walks.len());
    for (i, walk) in walks.iter_mut().enumerate() {
        if let Some((term, df)) = walk.terms.next()? {
            heads.push(Reverse((term.to_vec(), i, df)));
        }
    }
    // Buffers of terms already passed, reused for the walks' next ones.
    let mut spare: Vec<Vec<u8>> = Vec::new();
    let mut listing: Vec<(usize, u32)> = Vec::with_capacity(walks.len());
    while let Some(Reverse((term, first, df))) = heads.pop() {
        listing.clear();
        listing.push((first, df));
        while heads
            .peek()
            .is_some_and(|Reverse((next, ..))| *next == term)
        {
            if let Some(Reverse((same, i, df))) = heads.pop() {
                listing.push((i, df));
                spare.push(same);
            }
        }
        let (mut memtable_df, mut segments_df) = (0_u32, 0_u64);
        for &(i, df) in &listing {
            match walks.get(i).map(|w| w.walked) {
                Some(Walked::Memtable(_)) => memtable_df = memtable_df.saturating_add(df),
                Some(Walked::Segment(_)) => segments_df = segments_df.saturating_add(u64::from(df)),
                _ => {}
            }
        }
        each(&term, counts(memtable_df, segments_df))?;
        for &(i, _) in &listing {
            if let Some(walk) = walks.get_mut(i)
                && let Some((next, df)) = walk.terms.next()?
            {
                // A walk lists each term once, ascending: one that does not would be merged with part of its count.
                if next <= term.as_slice() {
                    return Err(out_of_order(walk.field));
                }
                let mut bytes = spare.pop().unwrap_or_default();
                bytes.clear();
                bytes.extend_from_slice(next);
                heads.push(Reverse((bytes, i, df)));
            }
        }
        spare.push(term);
    }
    Ok(())
}

/// A view listed the terms of `field` out of byte order, or one twice: its dictionary is corrupt (SPEC §5.3.1).
fn out_of_order(field: FieldId) -> ReadError {
    ReadError::Section {
        kind: SectionKind::TermDict,
        field: field.get(),
        position: 0,
        reason: SegmentCorruption::Invalid {
            what: "terms not strictly increasing in a term walk",
        },
    }
}

/// Which terms in range an expansion takes: every one (a `prefix`, whose range is the prefix's), or those within
/// a `fuzzy`'s distance. An [`Automaton`] over the term bytes.
#[derive(Debug, Clone)]
pub struct Expansion {
    fuzzy: Option<Arc<Fuzzy>>,
}

impl Expansion {
    /// Every term.
    pub(crate) fn all() -> Self {
        Self { fuzzy: None }
    }

    /// The terms within `distance` of `term` (SPEC §7.1: optimal string alignment over Unicode scalar values);
    /// `distance` is 1 or 2 ([`MAX_DISTANCE`] at most: a larger one is read as it, which a query never asks, SPEC §7.1).
    /// Public for the fuzz target only.
    #[doc(hidden)]
    pub fn fuzzy(term: &str, distance: u8) -> Self {
        Self {
            fuzzy: Some(Arc::new(Fuzzy {
                query: term.chars().map(u32::from).collect(),
                distance: distance.min(MAX_DISTANCE),
            })),
        }
    }

    /// The optimal string alignment distance of `term` when this is a fuzzy expansion that accepts it (SPEC §7.1).
    /// `None` for a prefix, or for a term farther than the distance. The query bytes are not analyzed. Public for the
    /// fuzz target only.
    #[doc(hidden)]
    pub fn distance(&self, term: &[u8]) -> Option<u8> {
        let fuzzy = self.fuzzy.as_ref()?;
        let mut state = fuzzy.start();
        for &byte in term {
            state = fuzzy.accept(state, byte);
        }
        fuzzy.flush(&mut state);
        let d = fuzzy.end(&state);
        (d <= fuzzy.distance).then_some(d)
    }
}

/// The largest `fuzzy` distance (SPEC §7.1).
const MAX_DISTANCE: u8 = 2;

/// The cells of a row the automaton keeps: the band of `2 · distance + 1` columns around the diagonal, at most
/// [`MAX_DISTANCE`]'s: `2 · 2 + 1`.
const BAND: usize = 5;

/// A `fuzzy` query's characters and distance.
#[derive(Debug)]
struct Fuzzy {
    query: Vec<u32>,
    distance: u8,
}

/// What a term byte that is not part of valid UTF-8 counts as: a character no query character equals.
const INVALID: u32 = u32::MAX;

/// Where a walk over a term's bytes is, against a `fuzzy` query (ROADMAP 8.2.4.1): the band of the optimal string
/// alignment table's row for the term's characters so far, the band of the row before (for a swap), the last
/// character, and a UTF-8 sequence not finished yet.
///
/// Row `i` holds the distances of the term's first `i` characters to each prefix of the query, `D[i][j]`. As
/// `D[i][j] ≥ |i − j|`, only the `2 · distance + 1` columns `j` in `i − distance ..= i + distance` can be within the
/// distance; every other one is above it, as is a column outside the table. So the state keeps those cells only,
/// capped at `distance + 1`, cell `k` being column `i − distance + k`: a step costs O(distance), not O(query
/// length), and the state is a few bytes on the stack, not a row allocated per byte (SPEC §7.1: a query term may be
/// 32 KiB).
#[derive(Debug, Clone, Copy)]
pub struct FuzzyState {
    row: [u8; BAND],
    prev: [u8; BAND],
    /// `i`: the term characters read so far.
    chars: usize,
    last: Option<u32>,
    /// The code point so far, its bytes, and the bytes it still needs.
    pending: (u32, u8, u8),
}

impl Fuzzy {
    fn cap(&self) -> u8 {
        self.distance.saturating_add(1)
    }

    /// The cells of a row: `2 · distance + 1`.
    fn width(&self) -> usize {
        2 * usize::from(self.distance) + 1
    }

    /// The column of cell `k` in row `i`, `None` outside the table (left of column 0 or right of the query's last).
    fn column(&self, i: usize, k: usize) -> Option<usize> {
        let j = i
            .saturating_add(k)
            .checked_sub(usize::from(self.distance))?;
        (j <= self.query.len()).then_some(j)
    }

    /// The row after the term character `c` (Wagner–Fischer, with the optimal string alignment swap), over the band.
    #[allow(clippy::many_single_char_names)] // The table's own names: row `i`, column `j`, cell `k`, character `c`, `q`.
    fn step(&self, state: &mut FuzzyState, c: u32) {
        let cap = self.cap();
        let i = state.chars.saturating_add(1);
        let at = |r: &[u8; BAND], k: usize| r.get(k).copied().unwrap_or(cap);
        let mut row = [cap; BAND];
        for k in 0..self.width() {
            let Some(j) = self.column(i, k) else {
                continue;
            };
            let Some(before) = j.checked_sub(1) else {
                // Column 0: `i` deletions.
                if let Some(cell) = row.get_mut(k) {
                    *cell = u8::try_from(i).unwrap_or(cap).min(cap);
                }
                continue;
            };
            let q = self.query.get(before).copied().unwrap_or(INVALID);
            // Row `i − 1`'s column `j − 1` is its cell `k`, its column `j` cell `k + 1`; this row's column `j − 1` is
            // cell `k − 1`.
            let substitute = at(&state.row, k).saturating_add(u8::from(q != c));
            let insert = at(&state.row, k + 1).saturating_add(1);
            let delete = k
                .checked_sub(1)
                .map_or(cap, |l| at(&row, l))
                .saturating_add(1);
            let mut d = substitute.min(insert).min(delete);
            // A swap of the query's characters `j − 2` and `j − 1` with the term's last two: row `i − 2`'s column
            // `j − 2` is its cell `k` too.
            if let Some(two) = j.checked_sub(2)
                && state.last == Some(q)
                && self.query.get(two) == Some(&c)
            {
                d = d.min(at(&state.prev, k).saturating_add(1));
            }
            if let Some(cell) = row.get_mut(k) {
                *cell = d.min(cap);
            }
        }
        state.prev = core::mem::replace(&mut state.row, row);
        state.chars = i;
        state.last = Some(c);
    }

    /// Ends a UTF-8 sequence that cannot complete: each of its bytes is a character.
    fn flush(&self, state: &mut FuzzyState) {
        let (_, len, _) = state.pending;
        state.pending = (0, 0, 0);
        for _ in 0..len {
            self.step(state, INVALID);
        }
    }

    fn accept(&self, mut state: FuzzyState, byte: u8) -> FuzzyState {
        let (cp, len, need) = state.pending;
        if need > 0 {
            if byte & 0xC0 == 0x80 {
                let cp = (cp << 6) | u32::from(byte & 0x3F);
                let len = len.saturating_add(1);
                if need > 1 {
                    state.pending = (cp, len, need - 1);
                    return state;
                }
                // The shortest form, and a scalar value: otherwise each byte is a character.
                let least = match len {
                    2 => 0x80,
                    3 => 0x800,
                    _ => 0x1_0000,
                };
                if cp >= least && char::from_u32(cp).is_some() {
                    state.pending = (0, 0, 0);
                    self.step(&mut state, cp);
                } else {
                    state.pending = (0, len, 0);
                    self.flush(&mut state);
                }
                return state;
            }
            self.flush(&mut state);
        }
        match byte {
            0x00..=0x7F => self.step(&mut state, u32::from(byte)),
            0xC2..=0xDF => state.pending = (u32::from(byte & 0x1F), 1, 1),
            0xE0..=0xEF => state.pending = (u32::from(byte & 0x0F), 1, 2),
            0xF0..=0xF4 => state.pending = (u32::from(byte & 0x07), 1, 3),
            _ => self.step(&mut state, INVALID),
        }
        state
    }

    /// The distance of the whole term to the whole query, capped at `distance + 1`: the row's cell of the query's last
    /// column, which is outside the band (so above the distance) when the lengths differ by more than it.
    fn end(&self, state: &FuzzyState) -> u8 {
        self.query
            .len()
            .saturating_add(usize::from(self.distance))
            .checked_sub(state.chars)
            .filter(|&k| k < self.width())
            .and_then(|k| state.row.get(k).copied())
            .unwrap_or(self.cap())
    }

    fn is_match(&self, state: &FuzzyState) -> bool {
        let mut end = *state;
        self.flush(&mut end);
        self.end(&end) <= self.distance
    }

    /// No cell within the distance: every later one is at least the row's smallest (a swap adds 1 to the row before,
    /// whose diagonal bounds this row), and every column outside the band is above the distance, so no extension of
    /// the term matches.
    fn can_match(&self, state: &FuzzyState) -> bool {
        state.row.iter().any(|&d| d <= self.distance)
    }

    fn start(&self) -> FuzzyState {
        let cap = self.cap();
        let mut row = [cap; BAND];
        for (k, cell) in row.iter_mut().enumerate().take(self.width()) {
            // Row 0: column `j` is `j` insertions.
            if let Some(j) = self.column(0, k) {
                *cell = u8::try_from(j).unwrap_or(cap).min(cap);
            }
        }
        FuzzyState {
            row,
            prev: [cap; BAND],
            chars: 0,
            last: None,
            pending: (0, 0, 0),
        }
    }
}

impl Automaton for Expansion {
    type State = Option<FuzzyState>;

    fn start(&self) -> Self::State {
        self.fuzzy.as_ref().map(|f| f.start())
    }

    fn is_match(&self, state: &Self::State) -> bool {
        match (&self.fuzzy, state) {
            (Some(f), Some(s)) => f.is_match(s),
            _ => true,
        }
    }

    fn can_match(&self, state: &Self::State) -> bool {
        match (&self.fuzzy, state) {
            (Some(f), Some(s)) => f.can_match(s),
            _ => true,
        }
    }

    fn will_always_match(&self, _: &Self::State) -> bool {
        self.fuzzy.is_none()
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        match (&self.fuzzy, state) {
            (Some(f), Some(s)) => Some(f.accept(*s, byte)),
            _ => None,
        }
    }
}

/// The terms of `field` in range that `expansion` takes from `terms`, at most `cap`: the highest collection `df`
/// (`stats`) first, ties by bytes; returned in byte order with their `df`s (SPEC §7.1).
///
/// `terms` lists each term once with the views' own counts ([`TermSource::each_term`]), so each `df` is read once
/// ([`StatisticsProvider::expansion_df`]) without looking the term up in every view again (ROADMAP 7.4.14, 7.4.15),
/// and only the kept terms reach the statistics' cache ([`StatisticsProvider::expansion_kept`]).
///
/// # Errors
/// As [`TermSource::each_term`]; a `df` that cannot be read.
pub(crate) fn expand(
    field: FieldId,
    range: (Bound<&[u8]>, Bound<&[u8]>),
    expansion: &Expansion,
    cap: usize,
    terms: &(impl TermSource + ?Sized),
    stats: &(impl StatisticsProvider + ?Sized),
) -> Result<Vec<(Vec<u8>, u64)>, ReadError> {
    let mut kept = BTreeSet::<(Reverse<u64>, Vec<u8>)>::new();
    if cap == 0 {
        return Ok(Vec::new());
    }
    terms.each_term(field, range.0, range.1, expansion, &mut |term, counts| {
        let df = stats.expansion_df(field, term, counts)?;
        // Full: a term ranking after the last kept one (or that one again) changes nothing. One evicted before
        // ranks after it too, since the last kept only moves up.
        if kept.len() >= cap
            && kept
                .last()
                .is_some_and(|(last, bytes)| (Reverse(df), term) >= (*last, bytes.as_slice()))
        {
            return Ok(());
        }
        if kept.insert((Reverse(df), term.to_vec())) && kept.len() > cap {
            kept.pop_last();
        }
        Ok(())
    })?;
    let mut out: Vec<(Vec<u8>, u64)> = kept.into_iter().map(|(Reverse(df), t)| (t, df)).collect();
    out.sort_unstable();
    for (term, _) in &out {
        stats.expansion_kept(field, term)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use fst::Automaton;
    use proptest::prelude::*;

    use super::Expansion;

    fn accepts(a: &Expansion, term: &[u8]) -> bool {
        let mut s = a.start();
        for &b in term {
            if !a.can_match(&s) {
                return false;
            }
            s = a.accept(&s, b);
        }
        a.is_match(&s)
    }

    /// Optimal string alignment distance by the textbook table, over characters (each invalid byte one).
    #[allow(
        clippy::many_single_char_names,
        clippy::needless_range_loop,
        clippy::indexing_slicing
    )]
    fn osa(a: &[u32], b: &[u32]) -> usize {
        let (n, m) = (a.len(), b.len());
        let mut d = vec![vec![0_usize; m + 1]; n + 1];
        for (i, row) in d.iter_mut().enumerate() {
            row[0] = i;
        }
        for j in 0..=m {
            d[0][j] = j;
        }
        for i in 1..=n {
            for j in 1..=m {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                let mut v = (d[i - 1][j] + 1)
                    .min(d[i][j - 1] + 1)
                    .min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    v = v.min(d[i - 2][j - 2] + 1);
                }
                d[i][j] = v;
            }
        }
        d[n][m]
    }

    /// The characters of `bytes` as SPEC §7.1 counts them: valid UTF-8 by scalar value, every other byte one.
    fn chars(bytes: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            match core::str::from_utf8(rest) {
                Ok(s) => {
                    out.extend(s.chars().map(u32::from));
                    break;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    out.extend(
                        core::str::from_utf8(valid)
                            .unwrap_or_default()
                            .chars()
                            .map(u32::from),
                    );
                    let bad = e.error_len().unwrap_or(after.len());
                    out.extend(core::iter::repeat_n(u32::MAX, bad));
                    rest = after.get(bad..).unwrap_or_default();
                }
            }
        }
        out
    }

    #[test]
    fn fuzzy_by_hand() {
        let one = Expansion::fuzzy("shoes", 1);
        for (term, want) in [
            ("shoes", true),
            ("shoe", true),
            ("shoess", true),
            ("shose", true),  // a swap
            ("hsoes", true),  // a swap at the start
            ("shxes", true),  // a substitution
            ("sheos", true),  // a swap in the middle
            ("sohse", false), // two swaps
            ("s", false),
            ("", false),
        ] {
            assert_eq!(accepts(&one, term.as_bytes()), want, "{term}");
        }
        // Characters, not bytes: `é` is one substitution of `e`.
        assert!(accepts(&Expansion::fuzzy("cafe", 1), "café".as_bytes()));
        assert!(!accepts(&Expansion::fuzzy("cafe", 1), "cafés".as_bytes()));
        assert!(accepts(&Expansion::fuzzy("cafe", 2), "cafés".as_bytes()));
        assert!(accepts(&Expansion::fuzzy("ab", 2), b""));
        // Optimal string alignment edits no character twice: `ca` to `abc` is 3, not 2 (swap, then insert).
        assert!(!accepts(&Expansion::fuzzy("ca", 2), b"abc"));
        assert!(accepts(&Expansion::fuzzy("ca", 2), b"ac"));
        assert!(accepts(&Expansion::all(), b"anything"));
        // The distance the score multiplies by, not only accept or reject.
        let apple = Expansion::fuzzy("apple", 1);
        assert_eq!(apple.distance(b"apple"), Some(0));
        assert_eq!(apple.distance(b"appel"), Some(1)); // a swap
        assert_eq!(apple.distance(b"apply"), Some(1));
        assert_eq!(apple.distance(b"zzzz"), None);
        assert_eq!(Expansion::all().distance(b"apple"), None);
        assert_eq!(Expansion::fuzzy("ca", 2).distance(b"abc"), None);
        assert_eq!(Expansion::fuzzy("ca", 2).distance(b"ac"), Some(1));
    }

    /// Pieces of terms: characters of 1 to 4 bytes, and bytes that are not UTF-8 (a lone lead, a stray continuation).
    const PIECES: [&[u8]; 8] = [
        b"a",
        b"b",
        b"c",
        "\u{e9}".as_bytes(),
        "\u{20ac}".as_bytes(),
        "\u{1f600}".as_bytes(),
        &[0xC3],
        &[0x80],
    ];

    /// A query of up to 40 characters and a term made from it by a few random edits (insertions, deletions,
    /// substitutions, swaps, and bytes cut from a character), so that terms near the distance are common.
    fn near() -> impl Strategy<Value = (String, Vec<u8>)> {
        let query = proptest::collection::vec(0..6_usize, 0..40).prop_map(|v| {
            v.into_iter()
                .map(|i| ['a', 'b', 'c', '\u{e9}', '\u{20ac}', '\u{1f600}'][i])
                .collect::<String>()
        });
        let edits = proptest::collection::vec(
            (0..5_u8, any::<prop::sample::Index>(), 0..PIECES.len()),
            0..4,
        );
        (query, edits).prop_map(|(query, edits)| {
            let mut term: Vec<Vec<u8>> =
                query.chars().map(|c| c.to_string().into_bytes()).collect();
            for (op, at, piece) in edits {
                let piece = PIECES[piece].to_vec();
                let i = if term.is_empty() {
                    0
                } else {
                    at.index(term.len())
                };
                match op {
                    0 => term.insert(i, piece),
                    1 if !term.is_empty() => {
                        term.remove(i);
                    }
                    2 if !term.is_empty() => term[i] = piece,
                    3 if i + 1 < term.len() => term.swap(i, i + 1),
                    // Half a multi-byte character: its bytes count one each.
                    4 if !term.is_empty() && term[i].len() > 1 => term[i].truncate(1),
                    _ => {}
                }
            }
            (query, term.concat())
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 20_000 } else { 2_000 }))]

        /// The banded automaton (ROADMAP 8.2.4.1) agrees with the textbook table: a term is accepted, with every
        /// prefix of it pruned by `can_match` only when no extension matches, exactly when its distance is within the
        /// query's, and `distance` gives that distance.
        #[test]
        fn the_band_is_the_table((query, term) in near(), distance in 1..=2_u8) {
            let a = Expansion::fuzzy(&query, distance);
            let q: Vec<u32> = query.chars().map(u32::from).collect();
            let d = osa(&q, &chars(&term));
            let within = d <= usize::from(distance);
            prop_assert_eq!(accepts(&a, &term), within, "{:?} ~{} {:?}: {}", query, distance, term, d);
            prop_assert_eq!(a.distance(&term).map(usize::from), within.then_some(d));
        }
    }

    /// The automaton agrees with the table on every pair over a small alphabet, invalid UTF-8 included.
    #[test]
    fn fuzzy_matches_the_table() {
        // Valid 1- to 4-byte characters; a lone lead, a stray continuation, a byte never in UTF-8; overlong 3- and
        // 4-byte forms, a surrogate, a value past U+10FFFF, and sequences cut short.
        let alphabet: [&[u8]; 15] = [
            b"a",
            b"b",
            "é".as_bytes(),
            "€".as_bytes(),
            "😀".as_bytes(),
            &[0xC3],
            &[0xFF],
            &[0x80],
            &[0xE0, 0x80, 0x80],
            &[0xE0, 0xA0],
            &[0xED, 0xA0, 0x80],
            &[0xF0, 0x80, 0x80, 0x80],
            &[0xF4, 0x90, 0x80, 0x80],
            &[0xE2, 0x82],
            &[0xF0, 0x9F, 0x98],
        ];
        let mut x = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            usize::try_from(x % n as u64).unwrap_or(0)
        };
        for _ in 0..20_000 {
            let mut word = |max: usize| {
                let mut w = Vec::new();
                for _ in 0..next(max) {
                    w.extend_from_slice(alphabet[next(alphabet.len())]);
                }
                w
            };
            let query: String = String::from_utf8_lossy(&word(5)).replace('\u{FFFD}', "a");
            let term = word(6);
            for distance in [1, 2] {
                let a = Expansion::fuzzy(&query, distance);
                let q: Vec<u32> = query.chars().map(u32::from).collect();
                let want = osa(&q, &chars(&term)) <= usize::from(distance);
                assert_eq!(accepts(&a, &term), want, "{query:?} ~{distance} {term:?}");
            }
        }
    }
}

#[cfg(test)]
mod df_tests {
    use core::cell::RefCell;
    use core::ops::Bound;
    use std::sync::Arc;

    use persia_analysis::AnalyzerRegistry;

    use super::{EachTerm, Expansion, TermSource, expand};
    use crate::memtable::{Memtable, MemtableView};
    use crate::schema::{CollectionSchema, FieldId, FieldKind, SchemaBuilder, TextOptions};
    use crate::segment::ReadError;
    use crate::stats::{FieldTotals, MemtableDf, StatisticsProvider, TermCounts};

    /// A source listing each term once, in byte order, some with a memtable count.
    struct Lists<'a> {
        memtable: &'a MemtableView,
        terms: Vec<(&'static str, Option<u32>)>,
    }

    impl TermSource for Lists<'_> {
        fn each_term(
            &self,
            _field: FieldId,
            _lower: Bound<&[u8]>,
            _upper: Bound<&[u8]>,
            _expansion: &Expansion,
            each: &mut EachTerm<'_>,
        ) -> Result<(), ReadError> {
            for &(term, df) in &self.terms {
                let memtable = df.map(|df| MemtableDf::new(self.memtable, df));
                each(term.as_bytes(), TermCounts::new(memtable, None))?;
            }
            Ok(())
        }
    }

    /// `df` = the term's length times 10, plus the memtable's count when given; records what it was asked.
    #[derive(Default)]
    struct Counting {
        asked: RefCell<Vec<(String, Option<u32>)>>,
        kept: RefCell<Vec<String>>,
    }

    impl StatisticsProvider for Counting {
        fn doc_count(&self) -> u64 {
            100
        }

        fn field(&self, _: FieldId) -> FieldTotals {
            FieldTotals::default()
        }

        fn df(&self, _: FieldId, _: &[u8]) -> Result<u64, ReadError> {
            panic!("an expansion asks expansion_df")
        }

        fn expansion_df(
            &self,
            _: FieldId,
            term: &[u8],
            counts: TermCounts<'_>,
        ) -> Result<u64, ReadError> {
            let term = String::from_utf8(term.to_vec()).unwrap();
            let memtable = counts.memtable().map(|m| m.df());
            let df = term.len() as u64 * 10 + memtable.map_or(0, u64::from);
            self.asked.borrow_mut().push((term, memtable));
            Ok(df)
        }

        fn expansion_kept(&self, _: FieldId, term: &[u8]) -> Result<(), ReadError> {
            self.kept
                .borrow_mut()
                .push(String::from_utf8(term.to_vec()).unwrap());
            Ok(())
        }
    }

    fn view() -> (Arc<CollectionSchema>, MemtableView) {
        let schema = SchemaBuilder::new("c")
            .field("body", FieldKind::Text(TextOptions::default()))
            .build()
            .unwrap();
        let mem = Memtable::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        (Arc::new(schema), mem.view())
    }

    /// ROADMAP 7.4.14: each term's `df` is asked once, with the counts the source gave; only the kept terms are
    /// reported kept.
    #[test]
    fn each_term_is_asked_once_and_only_winners_are_kept() {
        let (schema, memtable) = view();
        let field = schema.field("body").unwrap().id();
        let lists = Lists {
            memtable: &memtable,
            terms: vec![
                ("a", None),
                ("ab", Some(5)),
                ("abc", None),
                ("abcd", Some(1)),
                ("abcde", None),
            ],
        };
        let stats = Counting::default();
        let unbounded = (Bound::Unbounded, Bound::Unbounded);
        let kept = expand(field, unbounded, &Expansion::all(), 2, &lists, &stats).unwrap();
        // df: a 10, ab 25, abc 30, abcd 41, abcde 50: the top two.
        assert_eq!(kept, [(b"abcd".to_vec(), 41), (b"abcde".to_vec(), 50)]);
        let asked: Vec<(String, Option<u32>)> = stats.asked.into_inner();
        let expected: Vec<(String, Option<u32>)> = lists
            .terms
            .iter()
            .map(|&(t, m)| (t.to_owned(), m))
            .collect();
        assert_eq!(asked, expected);
        assert_eq!(stats.kept.into_inner(), ["abcd", "abcde"]);
        // A cap of 0 asks nothing.
        let none = Counting::default();
        let capped = expand(field, unbounded, &Expansion::all(), 0, &lists, &none).unwrap();
        assert_eq!(capped, []);
        assert_eq!(none.asked.into_inner(), []);
    }
}

#[cfg(test)]
mod merge_tests {
    use core::ops::Bound;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use persia_analysis::AnalyzerRegistry;
    use persia_format::ByteSource;
    use proptest::prelude::*;

    use super::{Expansion, TermSource, TermWalk, Walked, each_merged_term};
    use crate::document::{Document, Value};
    use crate::memtable::{Memtable, MemtableView};
    use crate::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
    use crate::search::{Interrupt, Interruptible, Interrupts, Steps};
    use crate::segment::{ReadError, Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    use crate::stats::TermCounts;
    use crate::view::{SegmentView, TermEnum};

    struct Never;

    impl Interrupts for Never {
        fn check(&self) -> Result<(), Interrupt> {
            Ok(())
        }
    }

    /// Terms in byte order with their counts, as a view lists them.
    struct Listed {
        terms: Vec<(Vec<u8>, u32)>,
        at: usize,
    }

    impl TermEnum for Listed {
        fn next(&mut self) -> Result<Option<(&[u8], u32)>, ReadError> {
            let next = self.terms.get(self.at).map(|(t, df)| (t.as_slice(), *df));
            self.at += 1;
            Ok(next)
        }
    }

    fn schema() -> CollectionSchema {
        SchemaBuilder::new("c")
            .field("body", FieldKind::Text(TextOptions::default()))
            .build()
            .unwrap()
    }

    fn segment(schema: &CollectionSchema, docs: &[&str]) -> Segment {
        let body = schema.field("body").unwrap().id();
        let mut b = SegmentBuilder::new(schema, &AnalyzerRegistry::builtin()).unwrap();
        for (i, text) in docs.iter().enumerate() {
            b.add(&Document::new(format!("s{i}")).with(body, Value::String((*text).into())))
                .unwrap();
        }
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
        Segment::open(ByteSource::new(bytes.into()), schema).unwrap()
    }

    fn memtable(schema: &CollectionSchema, docs: &[&str]) -> MemtableView {
        let body = schema.field("body").unwrap().id();
        let mut mem = Memtable::new(schema, &AnalyzerRegistry::builtin()).unwrap();
        for (i, text) in docs.iter().enumerate() {
            mem.put(&Document::new(format!("m{i}")).with(body, Value::String((*text).into())))
                .unwrap();
        }
        mem.view()
    }

    /// What a merged walk passed: each term with its memtable and segments counts, and how many segments it named.
    type Passed = Vec<(Vec<u8>, Option<u32>, Option<(u64, usize)>)>;

    fn merged(walks: Vec<TermWalk<'_>>) -> Passed {
        let mut passed = Vec::new();
        each_merged_term(walks, &mut |term, counts: TermCounts<'_>| {
            passed.push((
                term.to_vec(),
                counts.memtable().map(|m| m.df()),
                counts.segments().map(|s| (s.df(), s.segments().len())),
            ));
            Ok(())
        })
        .unwrap();
        passed
    }

    proptest! {
        /// ROADMAP 7.4.15: the merge passes every term any walk lists once, in byte order, with the memtable walk's
        /// count (0 where it does not list the term) and the sum of the segment walks'.
        #[test]
        fn the_merge_is_the_union_with_summed_counts(
            views in prop::collection::vec(
                (any::<bool>(), prop::collection::btree_map(prop::collection::vec(0_u8..4, 0..4), 1_u32..100, 0..12)),
                0..5,
            ),
        ) {
            let schema = schema();
            let seg = segment(&schema, &["x"]);
            let mem = memtable(&schema, &[]);
            // At most one memtable walk; the others are segments (the same one, which the merge does not mind).
            let mut has_memtable = false;
            let mut walks = Vec::new();
            let mut want = BTreeMap::<Vec<u8>, (u32, u64)>::new();
            for (is_memtable, terms) in &views {
                let is_memtable = *is_memtable && !has_memtable;
                has_memtable |= is_memtable;
                for (term, df) in terms {
                    let entry = want.entry(term.clone()).or_default();
                    if is_memtable {
                        entry.0 = *df;
                    } else {
                        entry.1 += u64::from(*df);
                    }
                }
                walks.push(TermWalk {
                    terms: Box::new(Listed { terms: terms.clone().into_iter().collect(), at: 0 }),
                    walked: if is_memtable { Walked::Memtable(&mem) } else { Walked::Segment(&seg) },
                    field: schema.field("body").unwrap().id(),
                });
            }
            let segments = views.len() - usize::from(has_memtable);
            let want: Passed = want
                .into_iter()
                .map(|(t, (m, s))| (t, has_memtable.then_some(m), Some((s, segments))))
                .collect();
            prop_assert_eq!(merged(walks), want);
        }
    }

    /// Real views, bare or under a search's interrupts: a memtable and two segments merged give each term the
    /// memtable's live count and the segments' summed `df`, as looking it up in each view does (ROADMAP 7.4.14,
    /// 7.4.15).
    #[test]
    fn merged_views_count_as_their_lookups() {
        let schema = Arc::new(schema());
        let body = schema.field("body").unwrap().id();
        let mem = memtable(&schema, &["run runs", "run", "red"]);
        let a = segment(&schema, &["run rust", "rare"]);
        let b = segment(&schema, &["runs", "rust run", "rye"]);
        let (va, vb) = (SegmentView::new(&a, None), SegmentView::new(&b, None));
        let range = (
            Bound::Included(b"r".as_slice()),
            Bound::Excluded(b"s".as_slice()),
        );
        let all = Expansion::all();
        let bare = vec![
            TermWalk::of(&mem, body, range, &all).unwrap(),
            TermWalk::of(&va, body, range, &all).unwrap(),
            TermWalk::of(&vb, body, range, &all).unwrap(),
        ];
        let lookup = |term: &str| {
            let df = |s: &Segment| {
                s.terms(body)
                    .unwrap()
                    .get(term.as_bytes())
                    .unwrap()
                    .map_or(0, |i| u64::from(i.df()))
            };
            (
                term.as_bytes().to_vec(),
                Some(mem.df(body, term.as_bytes()).unwrap()),
                Some((df(&a) + df(&b), 2)),
            )
        };
        let want: Passed = ["rare", "red", "run", "runs", "rust", "rye"]
            .into_iter()
            .map(lookup)
            .collect();
        assert_eq!(merged(bare), want);
        let steps = Steps::new(&Never);
        let (wm, wa, wb) = (
            Interruptible::new(&mem, &steps),
            Interruptible::new(&va, &steps),
            Interruptible::new(&vb, &steps),
        );
        let watched = vec![
            TermWalk::of(&wm, body, range, &all).unwrap(),
            TermWalk::of(&wa, body, range, &all).unwrap(),
            TermWalk::of(&wb, body, range, &all).unwrap(),
        ];
        assert_eq!(merged(watched), want);
    }

    /// Two memtable walks: whose count would it be? None is passed, and the statistics count the term themselves.
    #[test]
    fn two_memtable_walks_pass_no_memtable_count() {
        let schema = schema();
        let body = schema.field("body").unwrap().id();
        let mem = memtable(&schema, &["run"]);
        let all = Expansion::all();
        let unbounded = (Bound::Unbounded, Bound::Unbounded);
        let walks = vec![
            TermWalk::of(&mem, body, unbounded, &all).unwrap(),
            TermWalk::of(&mem, body, unbounded, &all).unwrap(),
        ];
        assert_eq!(merged(walks), [(b"run".to_vec(), None, Some((0, 0)))]);
    }

    /// A walk that lists a term again, or after a larger one, is a corrupt dictionary: merged, it would pass a term
    /// twice with part of its count each time. One walk or several, the merge stops with a corruption.
    #[test]
    fn a_walk_out_of_order_is_corrupt() {
        let schema = schema();
        let body = schema.field("body").unwrap().id();
        let seg = segment(&schema, &["x"]);
        let walk = |terms: &[&str]| TermWalk {
            terms: Box::new(Listed {
                terms: terms.iter().map(|t| (t.as_bytes().to_vec(), 1)).collect(),
                at: 0,
            }),
            walked: Walked::Segment(&seg),
            field: body,
        };
        let corrupt = |walks: Vec<TermWalk<'_>>| {
            let r = each_merged_term(walks, &mut |_, _| Ok(()));
            assert!(r.as_ref().is_err_and(ReadError::is_corruption), "{r:?}");
        };
        for bad in [&["b", "a"][..], &["a", "a"]] {
            corrupt(vec![walk(bad)]);
            corrupt(vec![walk(&["a", "c"]), walk(bad)]);
        }
        // Ascending walks that share terms are fine.
        let r = each_merged_term(vec![walk(&["a", "b"]), walk(&["a", "c"])], &mut |_, _| {
            Ok(())
        });
        assert!(r.is_ok());
    }

    /// A single view as a term source passes its own counts: a memtable's live `df`, a segment's `df` with the
    /// segment named.
    #[test]
    fn a_single_view_passes_its_walk_counts() {
        let schema = schema();
        let body = schema.field("body").unwrap().id();
        let mem = memtable(&schema, &["run runs", "run", "red"]);
        let seg = segment(&schema, &["run", "run"]);
        let walk = |source: &dyn TermSource| {
            let mut passed = Vec::new();
            source
                .each_term(
                    body,
                    Bound::Unbounded,
                    Bound::Unbounded,
                    &Expansion::all(),
                    &mut |term, counts| {
                        if let Some(m) = counts.memtable() {
                            assert!(m.view().is_same_view(&mem));
                        }
                        passed.push((
                            String::from_utf8(term.to_vec()).unwrap(),
                            counts.memtable().map(|m| m.df()),
                            counts.segments().map(|s| (s.df(), s.segments().len())),
                        ));
                        Ok(())
                    },
                )
                .unwrap();
            passed
        };
        let in_memtable = |t: &str, df| (t.to_owned(), Some(df), Some((0, 0)));
        assert_eq!(
            walk(&mem),
            [
                in_memtable("red", 1),
                in_memtable("run", 2),
                in_memtable("runs", 1)
            ]
        );
        let steps = Steps::new(&Never);
        assert_eq!(walk(&Interruptible::new(&mem, &steps)), walk(&mem));
        let view = SegmentView::new(&seg, None);
        assert_eq!(walk(&view), [("run".to_owned(), None, Some((2, 1)))]);
        assert_eq!(walk(&Interruptible::new(&view, &steps)), walk(&view));
    }
}
