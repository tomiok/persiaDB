//! `prefix` and `fuzzy` expansion over the collection's terms, once per plan (SPEC §7.1, ROADMAP 7.4.12): every
//! view's terms are walked, and the `max_expansions` with the highest collection `df` are kept, ties by bytes.
//!
//! The prefix and the fuzzy term are raw bytes. This walk does not run the field analyzer: a prefix `Run` does not
//! meet a term the analyzer stored as `running`. Scoring of the terms kept (one shared `idf`, and a fuzzy
//! edit-distance weight) is in `plan`. M8 typo tolerance must analyze before it builds these queries.

use core::cmp::Reverse;
use core::ops::Bound;
use std::collections::BTreeSet;
use std::sync::Arc;

use fst::Automaton;

use crate::schema::FieldId;
use crate::segment::ReadError;
use crate::stats::StatisticsProvider;
use crate::view::{SearchView, TermEnum};

/// The views a plan runs on, as a `prefix` or `fuzzy` expands over their terms (SPEC §7.1): a single view, or a
/// collection's segments and memtable ([`crate::db::CollectionViews`]).
pub trait TermSource {
    /// Calls `each` with every term of the `text` or `keyword` field `field` between `lower` and `upper` (byte
    /// order) that `expansion` accepts, view by view: a term comes once per view that lists it
    /// ([`SearchView::terms`]).
    ///
    /// # Errors
    /// As [`SearchView::terms`]; what `each` returns.
    fn each_term(
        &self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        expansion: &Expansion,
        each: &mut dyn FnMut(&[u8]) -> Result<(), ReadError>,
    ) -> Result<(), ReadError>;
}

impl<V: SearchView> TermSource for V {
    fn each_term(
        &self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        expansion: &Expansion,
        each: &mut dyn FnMut(&[u8]) -> Result<(), ReadError>,
    ) -> Result<(), ReadError> {
        let mut terms = self.terms(field, lower, upper, expansion.clone())?;
        while let Some((term, _)) = terms.next()? {
            each(term)?;
        }
        Ok(())
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
    pub(super) fn all() -> Self {
        Self { fuzzy: None }
    }

    /// The terms within `distance` of `term` (SPEC §7.1: optimal string alignment over Unicode scalar values);
    /// `distance` is 1 or 2.
    pub(super) fn fuzzy(term: &str, distance: u8) -> Self {
        Self {
            fuzzy: Some(Arc::new(Fuzzy {
                query: term.chars().map(u32::from).collect(),
                distance,
            })),
        }
    }

    /// The optimal string alignment distance of `term` when this is a fuzzy expansion that accepts it (SPEC §7.1).
    /// `None` for a prefix, or for a term farther than the distance. The query bytes are not analyzed.
    pub(super) fn distance(&self, term: &[u8]) -> Option<u8> {
        let fuzzy = self.fuzzy.as_ref()?;
        let mut state = fuzzy.start();
        for &byte in term {
            state = fuzzy.accept(&state, byte);
        }
        let mut end = state;
        fuzzy.flush(&mut end);
        let d = end.row.last().copied()?;
        (d <= fuzzy.distance).then_some(d)
    }
}

/// A `fuzzy` query's characters and distance.
#[derive(Debug)]
struct Fuzzy {
    query: Vec<u32>,
    distance: u8,
}

/// What a term byte that is not part of valid UTF-8 counts as: a character no query character equals.
const INVALID: u32 = u32::MAX;

/// Where a walk over a term's bytes is, against a `fuzzy` query: the edit distances of the term's characters so far
/// to each prefix of the query, capped at `distance + 1`, the row before (for a swap), and a UTF-8 sequence not
/// finished yet.
#[derive(Debug, Clone)]
pub struct FuzzyState {
    row: Vec<u8>,
    prev: Vec<u8>,
    last: Option<u32>,
    /// The code point so far, its bytes, and the bytes it still needs.
    pending: (u32, u8, u8),
}

impl Fuzzy {
    fn cap(&self) -> u8 {
        self.distance.saturating_add(1)
    }

    /// The row after the term character `c` (Wagner–Fischer, with the optimal string alignment swap).
    fn step(&self, state: &mut FuzzyState, c: u32) {
        let cap = self.cap();
        let mut row = Vec::with_capacity(state.row.len());
        row.push(
            state
                .row
                .first()
                .map_or(cap, |&r| r.saturating_add(1))
                .min(cap),
        );
        for (i, &q) in self.query.iter().enumerate() {
            let at = |r: &[u8], j: usize| r.get(j).copied().unwrap_or(cap);
            let substitute = at(&state.row, i).saturating_add(u8::from(q != c));
            let insert = at(&state.row, i + 1).saturating_add(1);
            let delete = at(&row, i).saturating_add(1);
            let mut d = substitute.min(insert).min(delete);
            // A swap of the query's characters `i - 1` and `i` with the term's last two.
            if let Some(before) = i.checked_sub(1)
                && state.last == Some(q)
                && self.query.get(before) == Some(&c)
            {
                d = d.min(at(&state.prev, before).saturating_add(1));
            }
            row.push(d.min(cap));
        }
        state.prev = core::mem::replace(&mut state.row, row);
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

    fn accept(&self, state: &FuzzyState, byte: u8) -> FuzzyState {
        let mut state = state.clone();
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

    fn is_match(&self, state: &FuzzyState) -> bool {
        let mut end = state.clone();
        self.flush(&mut end);
        end.row.last().is_some_and(|&d| d <= self.distance)
    }

    /// No row entry within the distance: every later one is at least the row's smallest (a swap adds 1 to the row
    /// before, whose diagonal bounds this row), so no extension of the term matches.
    fn can_match(&self, state: &FuzzyState) -> bool {
        state.row.iter().any(|&d| d <= self.distance)
    }

    fn start(&self) -> FuzzyState {
        let cap = self.cap();
        let row = (0..=self.query.len())
            .map(|i| u8::try_from(i).unwrap_or(cap).min(cap))
            .collect::<Vec<_>>();
        FuzzyState {
            prev: vec![cap; row.len()],
            row,
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
            (Some(f), Some(s)) => Some(f.accept(s, byte)),
            _ => None,
        }
    }
}

/// The terms of `field` in range that `expansion` takes from `terms`, at most `cap`: the highest collection `df`
/// (`stats`) first, ties by bytes; returned in byte order with their `df`s (SPEC §7.1).
///
/// # Errors
/// As [`TermSource::each_term`]; a `df` that cannot be read.
pub(super) fn expand(
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
    terms.each_term(field, range.0, range.1, expansion, &mut |term| {
        let df = stats.df(field, term)?;
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
    Ok(out)
}

#[cfg(test)]
mod tests {
    use fst::Automaton;

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
