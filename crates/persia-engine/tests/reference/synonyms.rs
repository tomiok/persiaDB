//! Reference synonym expansion (SPEC §6.3.2 `synonyms`, §7.6 L1, §7.1.3), written from the SPEC text alone: each
//! group term analyzed into tokens, the longest term matching at each start of an analyzed query, what a match reports,
//! the compiler's reading of whole clauses over several fields, its bound on the alternatives, and the cascade's cut.
//!
//! The analysis is a parameter: the differential suite analyzes by whitespace, where no term analyzes to nothing and
//! no stopword leaves a gap, so those rules are checked here, by hand.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::ops::Range;

/// A token: its text and its position.
pub type Token = (String, u32);

/// How a term or a query word analyzes (SPEC §6.3.4): its tokens, positions from 0 at the first word, and the
/// positions it took (removed tokens keep theirs).
pub type Analyze<'a> = dyn Fn(&str) -> (Vec<Token>, u32) + 'a;

/// The cap on what a match reports (SPEC §7.6 L1: 50 per matched term).
pub const CAP: usize = 50;

/// A term after analysis: its token texts and each token's position after the first's. Terms equal in both are one
/// term (SPEC §6.3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shape {
    texts: Vec<String>,
    gaps: Vec<u32>,
}

impl Shape {
    fn of(tokens: &[Token]) -> Option<Self> {
        let first = tokens.first()?.1;
        Some(Self {
            texts: tokens.iter().map(|t| t.0.clone()).collect(),
            gaps: tokens.iter().map(|t| t.1 - first).collect(),
        })
    }

    fn len(&self) -> usize {
        self.texts.len()
    }
}

/// A collection's synonym groups as one field's analysis sees them.
#[derive(Debug)]
pub struct Synonyms {
    /// Each distinct term after analysis.
    shapes: Vec<Shape>,
    /// Each group: its terms (index into `shapes`), each with its place in the group and as the group writes it there,
    /// in the group's order, each once.
    groups: Vec<Vec<(usize, usize, String)>>,
}

/// An alternative a match reports: its place (the group it is reported from, and its index there) and the term as
/// written.
pub type Alternative = ((usize, usize), String);

/// A match in a token sequence: the tokens it covers and what it reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub tokens: Range<usize>,
    pub alternatives: Vec<Alternative>,
}

impl Synonyms {
    /// SPEC §6.3.2: each term of every group analyzed; a term of no token ignored; terms equal after analysis one term,
    /// at its first place in each group that gives it.
    pub fn new(groups: &[Vec<String>], analyze: &Analyze<'_>) -> Self {
        let mut shapes: Vec<Shape> = Vec::new();
        let mut out = Vec::new();
        for group in groups {
            let mut terms: Vec<(usize, usize, String)> = Vec::new();
            for (place, written) in group.iter().enumerate() {
                let Some(shape) = Shape::of(&analyze(written).0) else {
                    continue;
                };
                let id = shapes.iter().position(|s| *s == shape).unwrap_or_else(|| {
                    shapes.push(shape);
                    shapes.len() - 1
                });
                if !terms.iter().any(|t| t.0 == id) {
                    terms.push((id, place, written.clone()));
                }
            }
            out.push(terms);
        }
        Self {
            shapes,
            groups: out,
        }
    }

    /// What term `id` reports (SPEC §6.3.2): in group order, then term order, the other terms of every group that
    /// holds it, each once, at most `cap`; each term at its place in the group it is reported from.
    fn alternatives(&self, id: usize, cap: usize) -> Vec<Alternative> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for (g, group) in self.groups.iter().enumerate() {
            if !group.iter().any(|t| t.0 == id) {
                continue;
            }
            for (other, place, written) in group {
                if *other != id && seen.insert(*other) && out.len() < cap {
                    out.push(((g, *place), written.clone()));
                }
            }
        }
        out
    }

    /// SPEC §6.3.2's scan of `tokens` (positions as the query's analysis numbers them), left to right: at each token
    /// that may start a match (`start`), the longest term whose texts and position gaps equal the tokens', that ends on
    /// a token that may end one (`end`) and has something to report; its tokens are then skipped. A term whose groups
    /// hold it alone reports nothing and never matches.
    pub fn scan(
        &self,
        tokens: &[Token],
        start: &dyn Fn(usize) -> bool,
        end: &dyn Fn(usize) -> bool,
        cap: usize,
    ) -> Vec<Match> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let best = (0..self.shapes.len())
                .filter(|&id| {
                    let shape = &self.shapes[id];
                    let Some(run) = tokens.get(i..i + shape.len()) else {
                        return false;
                    };
                    start(i)
                        && end(i + shape.len() - 1)
                        && Shape::of(run).as_ref() == Some(shape)
                        && !self.alternatives(id, 1).is_empty()
                })
                .max_by_key(|&id| (self.shapes[id].len(), std::cmp::Reverse(id)));
            match best {
                Some(id) => {
                    let len = self.shapes[id].len();
                    out.push(Match {
                        tokens: i..i + len,
                        alternatives: self.alternatives(id, cap),
                    });
                    i += len;
                }
                None => i += 1,
            }
        }
        out
    }
}

/// The compiler's matches in one default field over a run of required words (SPEC §6.3.2): each word analyzed alone
/// and numbered after the words before it by the positions they took; a match starts at a word's first token and
/// ends with a word's last. Each match as the words it covers.
pub fn field_matches(
    synonyms: &Synonyms,
    words: &[&str],
    analyze: &Analyze<'_>,
) -> Vec<(Range<usize>, Vec<Alternative>)> {
    let mut tokens = Vec::new();
    // Per token: its word, and whether it is its word's first or last.
    let mut owner: Vec<(usize, bool, bool)> = Vec::new();
    let mut base = 0;
    for (w, word) in words.iter().enumerate() {
        let (word_tokens, took) = analyze(word);
        let n = word_tokens.len();
        for (k, (text, position)) in word_tokens.into_iter().enumerate() {
            tokens.push((text, base + position));
            owner.push((w, k == 0, k + 1 == n));
        }
        base += took;
    }
    synonyms
        .scan(&tokens, &|i| owner[i].1, &|i| owner[i].2, CAP)
        .into_iter()
        .map(|m| {
            let first = owner[m.tokens.start].0;
            let last = owner[m.tokens.end - 1].0;
            (first..last + 1, m.alternatives)
        })
        .collect()
}

/// The compiler's runs over the default fields' matches, in field order (SPEC §6.3.2): the first field's matches,
/// then each later field's that covers the same words as one kept (sharing its alternatives: the union, each place
/// once, in group order then term order) or overlaps none kept; one that overlaps a kept match otherwise is dropped.
/// In word order. The compiler then keeps at most [`CAP`] of each run's alternatives.
pub fn merge(
    per_field: Vec<Vec<(Range<usize>, Vec<Alternative>)>>,
) -> Vec<(Range<usize>, Vec<Alternative>)> {
    let mut kept: Vec<(Range<usize>, Vec<Alternative>)> = Vec::new();
    for matches in per_field {
        for (words, alternatives) in matches {
            if let Some(same) = kept.iter_mut().find(|k| k.0 == words) {
                for a in alternatives {
                    if !same.1.iter().any(|s| s.0 == a.0) {
                        same.1.push(a);
                    }
                }
                same.1.sort_by_key(|a| a.0);
            } else if !kept
                .iter()
                .any(|k| k.0.start < words.end && words.start < k.0.end)
            {
                kept.push((words, alternatives));
            }
        }
    }
    kept.sort_by_key(|k| k.0.start);
    kept
}

/// SPEC §7.1.3's sharing of `left` among lists of `lens` (at least 1 each when `left` allows): each the smaller of its
/// own and the highest shared count `c` whose sum fits, and what `c` leaves one each, in order, to the lists `c`
/// lowered.
pub fn shared(lens: &[usize], left: usize) -> Vec<usize> {
    let sum = |c: usize| lens.iter().map(|&l| l.min(c)).sum::<usize>();
    let mut c = lens.iter().copied().max().unwrap_or(0);
    while c > 0 && sum(c) > left {
        c -= 1;
    }
    let mut rest = left.saturating_sub(sum(c));
    lens.iter()
        .map(|&l| {
            if l > c && rest > 0 {
                rest -= 1;
                c + 1
            } else {
                l.min(c)
            }
        })
        .collect()
}

/// SPEC §6.3.2's bound on a compilation's alternatives: at most `max_terms − 2 · n` in all (`n` its query's terms),
/// shared among the runs (`lens`: the terms each run's fields report, at most [`CAP`]) as §7.1.3 shares expansions, and
/// one each when that is less than one per run. Each run's count.
pub fn bound(lens: &[usize], max_terms: usize, n: usize) -> Vec<usize> {
    let left = max_terms.saturating_sub(2 * n);
    if left < lens.len() {
        vec![1; lens.len()]
    } else {
        shared(lens, left)
    }
}

/// SPEC §7.1.3's cut of L1's step: each list (`lens`, in text order) keeps its first alternatives; every list is
/// lowered to the highest count that `fits`, none below 1, then, in text order, a list that count cut takes one more
/// where the step still fits. `None` (skipped) when one alternative each does not fit.
pub fn cut(lens: &[usize], fits: &dyn Fn(&[usize]) -> bool) -> Option<Vec<usize>> {
    let at = |c: usize| lens.iter().map(|&l| l.min(c)).collect::<Vec<_>>();
    let top = lens.iter().copied().max().unwrap_or(1);
    let c = (1..=top).rev().find(|&c| fits(&at(c)))?;
    let mut counts = at(c);
    for i in 0..lens.len() {
        if lens[i] > c {
            counts[i] += 1;
            if !fits(&counts) {
                counts[i] -= 1;
            }
        }
    }
    Some(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whitespace words at positions 0, 1, 2...; `-` a stopword the analysis removes, keeping its position; `~`
    /// analyzes to nothing and takes no position (a word the tokenizer finds no token in).
    fn analyze(text: &str) -> (Vec<Token>, u32) {
        let mut tokens = Vec::new();
        let mut position = 0;
        for word in text.split_whitespace() {
            match word {
                "~" => {}
                "-" => position += 1,
                w => {
                    tokens.push((w.to_owned(), position));
                    position += 1;
                }
            }
        }
        (tokens, position)
    }

    fn synonyms(groups: &[&[&str]]) -> Synonyms {
        let groups: Vec<Vec<String>> = groups
            .iter()
            .map(|g| g.iter().map(|&t| t.to_owned()).collect())
            .collect();
        Synonyms::new(&groups, &analyze)
    }

    /// The matches over `words`, each alternative as written.
    fn run(s: &Synonyms, words: &[&str]) -> Vec<(Range<usize>, Vec<String>)> {
        field_matches(s, words, &analyze)
            .into_iter()
            .map(|(r, a)| (r, a.into_iter().map(|a| a.1).collect()))
            .collect()
    }

    /// No match.
    const NONE: [(Range<usize>, Vec<String>); 0] = [];

    fn alts(list: &[&str]) -> Vec<String> {
        list.iter().map(|&a| a.to_owned()).collect()
    }

    #[test]
    fn a_multi_word_run_matches_as_one_and_the_longest_term_wins() {
        let s = synonyms(&[&["new york", "nyc"], &["new", "fresh"]]);
        assert_eq!(run(&s, &["new", "york", "pizza"]), [(0..2, alts(&["nyc"]))]);
        // Without `york`, the shorter term matches.
        assert_eq!(run(&s, &["new", "pizza"]), [(0..1, alts(&["fresh"]))]);
        // Words out of order are not the term.
        assert_eq!(run(&s, &["york", "new"]), [(1..2, alts(&["fresh"]))]);
    }

    #[test]
    fn overlapping_runs_take_the_first_start_and_skip_its_tokens() {
        let s = synonyms(&[&["a b", "x"], &["b c", "y"], &["c", "z"]]);
        // `a b` starts first and takes `b`; `b c` cannot match, but `c` alone then does.
        assert_eq!(
            run(&s, &["a", "b", "c"]),
            [(0..2, alts(&["x"])), (2..3, alts(&["z"]))]
        );
        assert_eq!(run(&s, &["b", "c"]), [(0..2, alts(&["y"]))]);
    }

    #[test]
    fn a_word_with_several_alternatives_reports_group_then_term_order_each_once_capped() {
        let s = synonyms(&[&["tv", "television", "telly"], &["box", "tv", "television"]]);
        assert_eq!(
            run(&s, &["tv"]),
            [(0..1, alts(&["television", "telly", "box"]))]
        );
        // A term is reported at its place in the group it expands from: `box` comes before `tv` there.
        assert_eq!(run(&s, &["box"]), [(0..1, alts(&["tv", "television"]))]);
        let many: Vec<String> = (0..60).map(|i| format!("w{i}")).collect();
        let s = Synonyms::new(&[many], &analyze);
        let m = &run(&s, &["w0"])[0].1;
        assert_eq!(m.len(), CAP);
        assert_eq!(m[0], "w1");
        assert_eq!(m[CAP - 1], "w50");
    }

    #[test]
    fn terms_equal_after_analysis_are_one_and_a_group_of_one_never_matches() {
        // `a  b` and `a b` analyze alike: the group shrinks to one term, which has nothing to expand to.
        let s = synonyms(&[&["a b", "a  b"], &["c", "c"]]);
        assert_eq!(run(&s, &["a", "b", "c"]), NONE);
        // The same term in another group still expands there, as the first writing.
        let s = synonyms(&[&["a b", "a  b", "q"]]);
        assert_eq!(run(&s, &["a", "b"]), [(0..2, alts(&["q"]))]);
        assert_eq!(run(&s, &["q"]), [(0..1, alts(&["a b"]))]);
    }

    #[test]
    fn a_term_that_analyzes_to_nothing_is_ignored() {
        // `~` yields no token: the group keeps `cat` alone and never matches; the other group is unaffected.
        let s = synonyms(&[&["cat", "~"], &["dog", "~", "hound"]]);
        assert_eq!(run(&s, &["cat"]), NONE);
        assert_eq!(run(&s, &["dog"]), [(0..1, alts(&["hound"]))]);
        assert_eq!(run(&s, &["hound"]), [(0..1, alts(&["dog"]))]);
    }

    #[test]
    fn a_stopword_gap_must_line_up() {
        let s = synonyms(&[&["state - art", "sota"]]);
        // `of` is removed keeping its position, so the gap lines up.
        assert_eq!(run(&s, &["state", "-", "art"]), [(0..3, alts(&["sota"]))]);
        // No gap, or a word with no token (which takes no position), does not line up.
        assert_eq!(run(&s, &["state", "art"]), NONE);
        assert_eq!(run(&s, &["state", "~", "art"]), NONE);
    }

    #[test]
    fn a_match_covers_whole_words_and_a_term_ending_inside_one_does_not_hide_a_shorter_one() {
        let s = synonyms(&[&["a b", "x"], &["a", "y"]]);
        // One word `a b` of two tokens: the match must end with a word's last token.
        assert_eq!(run(&s, &["a b"]), [(0..1, alts(&["x"]))]);
        let s = synonyms(&[&["a b c", "x"], &["a b", "y"]]);
        // `a b c` would end inside the word `c d`, so `a b` matches.
        assert_eq!(run(&s, &["a", "b", "c d"]), [(0..2, alts(&["y"]))]);
        // Nor may a match start inside a word.
        let s = synonyms(&[&["b", "x"]]);
        assert_eq!(run(&s, &["a b"]), NONE);
    }

    #[test]
    fn fields_matching_the_same_words_share_in_group_then_term_order_and_an_overlap_is_dropped() {
        let at = |g: usize, i: usize, t: &str| ((g, i), t.to_owned());
        let merged = merge(vec![
            vec![
                (0..2, vec![at(0, 1, "x"), at(2, 0, "y")]),
                (3..4, vec![at(0, 0, "p")]),
            ],
            vec![
                // `z` comes from an earlier group than `y`, so the union lists it before `y`.
                (0..2, vec![at(1, 3, "z"), at(2, 0, "y")]),
                (1..3, vec![at(0, 0, "q")]),
                (4..5, vec![at(0, 0, "r")]),
            ],
        ]);
        assert_eq!(
            merged,
            [
                (0..2, vec![at(0, 1, "x"), at(1, 3, "z"), at(2, 0, "y")]),
                (3..4, vec![at(0, 0, "p")]),
                (4..5, vec![at(0, 0, "r")])
            ]
        );
    }

    #[test]
    fn the_bound_shares_like_expansions_and_keeps_one_each_below_one_per_run() {
        // 2 · 3 terms of 20: 14 left over lists of 10, 2, 6, at a shared 6 (sum 14).
        assert_eq!(bound(&[10, 2, 6], 20, 3), [6, 2, 6]);
        // 15 left: one more for the first list the count lowered.
        assert_eq!(bound(&[10, 2, 9], 21, 3), [7, 2, 6]);
        // Fewer left than runs: one each.
        assert_eq!(bound(&[3, 1], 7, 3), [1, 1]);
        assert_eq!(bound(&[3, 4], 1000, 3), [3, 4]);
    }

    #[test]
    fn the_cut_lowers_the_longest_first_then_gives_one_more_in_text_order() {
        // The step fits when the counts sum to at most 7.
        let fits = |c: &[usize]| c.iter().sum::<usize>() <= 7;
        assert_eq!(cut(&[5, 1, 5], &fits), Some(vec![3, 1, 3]));
        // At a shared 2 (sum 6) one is left: the first list lowered takes it.
        let fits = |c: &[usize]| c.iter().sum::<usize>() <= 7;
        assert_eq!(cut(&[4, 4, 4], &fits), Some(vec![3, 2, 2]));
        // A list may take one more where an earlier one could not: weights 3, 1.
        let fits = |c: &[usize]| 3 * c[0] + c[1] <= 9;
        assert_eq!(cut(&[4, 4], &fits), Some(vec![2, 3]));
        // One each does not fit: skipped.
        assert_eq!(
            cut(&[2, 2], &|c: &[usize]| c.iter().sum::<usize>() < 2),
            None
        );
        assert_eq!(cut(&[2, 3], &|_| true), Some(vec![2, 3]));
    }
}
