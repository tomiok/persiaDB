//! Reference `prefix` and `fuzzy` expansion (SPEC §7.1), written from the SPEC text alone: the terms a field holds in
//! the documents the statistics count, those a query accepts, the `max_expansions` of highest collection `df` kept
//! (ties by byte order), and the optimal string alignment distance by its textbook table, over Unicode scalar values.

#![allow(dead_code, clippy::cast_precision_loss)]

use std::collections::BTreeSet;

use super::bm25f::Corpus;

/// A character of a term as SPEC §7.1 counts them: a Unicode scalar value, or a byte that is not part of valid UTF-8,
/// which no query character equals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Char {
    Scalar(char),
    Invalid,
}

/// The characters of `bytes`: valid UTF-8 by scalar value, every other byte one [`Char::Invalid`].
pub fn chars(bytes: &[u8]) -> Vec<Char> {
    let mut out = Vec::new();
    for chunk in bytes.utf8_chunks() {
        out.extend(chunk.valid().chars().map(Char::Scalar));
        out.extend(chunk.invalid().iter().map(|_| Char::Invalid));
    }
    out
}

/// The optimal string alignment distance of `term` from `query` (SPEC §7.1): an insertion, a deletion, a substitution
/// or a swap of two adjacent characters each count 1, and no character is edited twice. A byte of `term` that is not
/// valid UTF-8 equals no character of `query`.
#[allow(clippy::needless_range_loop, clippy::many_single_char_names)] // The textbook form.
pub fn osa(query: &str, term: &[u8]) -> usize {
    let a: Vec<char> = query.chars().collect();
    let b = chars(term);
    let same = |i: usize, j: usize| b[j] == Char::Scalar(a[i]);
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
            let cost = usize::from(!same(i - 1, j - 1));
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && same(i - 1, j - 2) && same(i - 2, j - 1) {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[n][m]
}

/// `(distance + 1 − d) / (distance + 1)`: the weight of a `fuzzy` term `d` edits from the query term (SPEC §7.1).
pub fn edit_weight(distance: u8, d: usize) -> f64 {
    let span = f64::from(distance) + 1.0;
    (span - d as f64) / span
}

impl Corpus {
    /// Every term of `field` in the documents the statistics count (SPEC §7.1: the views' terms, a segment's deleted
    /// documents included), in byte order.
    pub fn terms(&self, field: usize) -> BTreeSet<&str> {
        self.counted
            .iter()
            .flat_map(|d| d[field].iter().map(String::as_str))
            .collect()
    }

    /// The terms of `field` that `accepts` takes, at most `max` of them: the highest collection `df` first, ties by
    /// byte order (SPEC §7.1). Each with its `df`, in byte order.
    pub fn keep(
        &self,
        field: usize,
        accepts: impl Fn(&str) -> bool,
        max: usize,
    ) -> Vec<(String, u64)> {
        let mut all: Vec<(String, u64)> = self
            .terms(field)
            .into_iter()
            .filter(|t| accepts(t))
            .map(|t| (t.to_owned(), self.df(field, t)))
            .collect();
        all.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
        });
        all.truncate(max);
        all.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        all
    }

    /// What a `prefix` keeps: the terms starting with the bytes of `prefix`.
    pub fn prefix_terms(&self, field: usize, prefix: &str, max: usize) -> Vec<(String, u64)> {
        self.keep(field, |t| t.as_bytes().starts_with(prefix.as_bytes()), max)
    }

    /// What a `fuzzy` keeps: the terms within `distance` of `term`, each with its `df` and its distance.
    pub fn fuzzy_terms(
        &self,
        field: usize,
        term: &str,
        distance: u8,
        max: usize,
    ) -> Vec<(String, u64, usize)> {
        self.keep(
            field,
            |t| osa(term, t.as_bytes()) <= usize::from(distance),
            max,
        )
        .into_iter()
        .map(|(t, df)| {
            let d = osa(term, t.as_bytes());
            (t, df, d)
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::bm25f::{Corpus, Field};
    use super::{edit_weight, osa};

    fn corpus(docs: &[&str]) -> Corpus {
        let field = Field {
            k1: 1.2,
            b: 0.75,
            keyword: false,
            positions: true,
        };
        Corpus {
            fields: vec![field],
            counted: docs
                .iter()
                .map(|d| vec![d.split_whitespace().map(str::to_owned).collect()])
                .collect(),
        }
    }

    #[test]
    fn osa_by_hand() {
        assert_eq!(osa("cart", b"cart"), 0);
        // A swap of two adjacent characters is one edit (Levenshtein counts two).
        assert_eq!(osa("crat", b"cart"), 1);
        // No character is edited twice: `ca` to `abc` is 3, not the 2 of an unrestricted Damerau distance.
        assert_eq!(osa("ca", b"abc"), 3);
        assert_eq!(osa("elephant", b"elegant"), 2);
        assert_eq!(osa("elehpant", b"elephnat"), 2);
        // Scalar values, not bytes: `é` is one character in two bytes.
        assert_eq!(osa("cafe", "café".as_bytes()), 1);
        assert_eq!(osa("café", b"caf"), 1);
    }

    #[test]
    fn an_invalid_byte_is_a_character_no_query_character_equals() {
        // `\xff` is one character: a substitution away from `t`, never equal to it.
        assert_eq!(osa("cat", b"ca\xff"), 1);
        assert_eq!(osa("cat", b"cat\xff"), 1);
        // A truncated two-byte sequence is two invalid bytes, so two characters.
        assert_eq!(osa("cat", b"ca\xc3"), 1);
        assert_eq!(osa("ca", b"ca\xe2\x82"), 2);
        // Two invalid bytes do not swap into a match either.
        assert_eq!(osa("ab", b"\xff\xfe"), 2);
    }

    #[test]
    fn edit_weights_by_hand() {
        assert_eq!(edit_weight(1, 0), 1.0);
        assert_eq!(edit_weight(1, 1), 0.5);
        assert_eq!(edit_weight(2, 1), 2.0 / 3.0);
        assert_eq!(edit_weight(2, 2), 1.0 / 3.0);
    }

    #[test]
    fn the_cap_keeps_the_highest_df_and_ties_go_to_the_lower_bytes() {
        // `cat` 3, `cart` 2, `card` 2, `cats` 1.
        let c = corpus(&["cat cart card", "cat cart card", "cat cats", "dog"]);
        let terms = |max| c.prefix_terms(0, "ca", max);
        assert_eq!(terms(1), [("cat".to_owned(), 3)]);
        // `card` and `cart` tie at 2: the lower bytes, `card`, is kept first. The result is in byte order.
        assert_eq!(terms(2), [("card".to_owned(), 2), ("cat".to_owned(), 3)]);
        assert_eq!(terms(3).len(), 3);
        assert_eq!(terms(50).len(), 4);
        assert_eq!(c.prefix_terms(0, "x", 50), []);
        // The prefix is bytes: `car` starts `card` and `cart`, not `cat`.
        assert_eq!(c.prefix_terms(0, "car", 50).len(), 2);
    }

    #[test]
    fn a_fuzzy_keeps_its_terms_within_the_distance_with_their_edits() {
        let c = corpus(&["cart cart card", "cat crate"]);
        // Within 1 of `crat`: `cart` (a swap), `cat` (a deletion) and `crate` (an insertion); `card` is 2.
        let kept = c.fuzzy_terms(0, "crat", 1, 50);
        let terms: Vec<(&str, usize)> = kept.iter().map(|(t, _, d)| (t.as_str(), *d)).collect();
        assert_eq!(terms, [("cart", 1), ("cat", 1), ("crate", 1)]);
        // Capped at 1: each is in one document, so the lowest bytes, `cart`.
        assert_eq!(c.fuzzy_terms(0, "crat", 1, 1)[0].0, "cart");
        assert_eq!(c.fuzzy_terms(0, "crat", 2, 50).len(), 4);
    }

    #[test]
    fn deleted_documents_count_in_the_df_that_keeps_a_term() {
        // The second document is deleted: it still counts (SPEC §7.2), so `cart` (2) outranks `cat` (1).
        let c = corpus(&["cart cat", "cart"]);
        assert_eq!(c.prefix_terms(0, "ca", 1), [("cart".to_owned(), 2)]);
    }
}
