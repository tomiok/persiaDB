//! The Snowball German stemmer, as `algorithms/german.sbl` of Snowball v3.1.1.

use std::borrow::Cow;

use super::Stemmer;
use super::word::{Word, after_vowel_and_consonant, finish};

/// The Snowball German stemmer (SPEC §6.3.2): `häuser` → `haus`, `aufeinanderfolgenden` → `aufeinanderfolg`,
/// `straße` → `strass`.
///
/// Expects lowercase text with its umlauts (run it after [`LowercaseFilter`](crate::LowercaseFilter) and before
/// [`FoldingFilter`](crate::FoldingFilter)). It writes `ß` as `ss` and `ae`, `oe`, `ue` as `ä`, `ö`, `ü` (except in
/// `qu`), and removes the umlauts from the stem.
///
/// ```
/// use persia_analysis::{GermanStemmer, Stemmer};
///
/// let stems: Vec<_> = ["häuser", "aufeinanderfolgenden", "straße", "möglichkeiten"]
///     .iter()
///     .map(|w| GermanStemmer.stem(w).into_owned())
///     .collect();
/// assert_eq!(stems, ["haus", "aufeinanderfolg", "strass", "moglich"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GermanStemmer;

impl Stemmer for GermanStemmer {
    fn stem<'a>(&self, word: &'a str) -> Cow<'a, str> {
        let mut w = Word::from_chars(prelude(word));
        let (p1, p2) = regions(w.chars());
        standard_suffix(&mut w, p1, p2);
        for c in w.chars_mut() {
            *c = match *c {
                'Y' => 'y',
                'U' | 'ü' => 'u',
                'ä' => 'a',
                'ö' => 'o',
                other => other,
            };
        }
        finish(word, w.chars())
    }
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'ä' | 'ö' | 'ü')
}

/// Marks `u` and `y` between vowels as consonants (`U`, `Y`), then writes `ß` as `ss` and `ae`, `oe`, `ue` as umlauts,
/// leaving `qu` alone.
fn prelude(word: &str) -> Vec<char> {
    let mut chars: Vec<char> = word.chars().collect();
    // Snowball's `repeat goto (v [('u'] v <- 'U') or ('y'] v <- 'Y'))`: `goto` leaves the cursor where a match starts, and
    // the marked letter cannot match again there, so the search goes on from the next letter: the vowel after a
    // marked letter can start the next match (`jouaient` marks both `U` and `I`).
    let mut i = 0;
    while let (Some(&a), Some(&b), Some(&c)) = (
        chars.get(i),
        chars.get(i.saturating_add(1)),
        chars.get(i.saturating_add(2)),
    ) {
        if is_vowel(a)
            && matches!(b, 'u' | 'y')
            && is_vowel(c)
            && let Some(b) = chars.get_mut(i.saturating_add(1))
        {
            *b = if *b == 'u' { 'U' } else { 'Y' };
        }
        i = i.saturating_add(1);
    }
    let mut out = Vec::with_capacity(chars.len());
    let mut rest = chars.as_slice();
    while let Some((&c, tail)) = rest.split_first() {
        let next = tail.first().copied();
        match (c, next) {
            ('ß', _) => {
                out.extend(['s', 's']);
                rest = tail;
            }
            ('a' | 'o' | 'u' | 'q', Some(n))
                if (c != 'q' && n == 'e') || (c == 'q' && n == 'u') =>
            {
                out.push(match c {
                    'a' => 'ä',
                    'o' => 'ö',
                    'u' => 'ü',
                    _ => 'q',
                });
                if c == 'q' {
                    out.push('u');
                }
                rest = tail.get(1..).unwrap_or_default();
            }
            _ => {
                out.push(c);
                rest = tail;
            }
        }
    }
    out
}

/// `R1` (at least 3 characters in) and `R2`, the latter searched from where `R1` was before that adjustment.
fn regions(chars: &[char]) -> (usize, usize) {
    let len = chars.len();
    if len < 3 {
        return (len, len);
    }
    let Some(p1) = after_vowel_and_consonant(chars, 0, is_vowel) else {
        return (len, len);
    };
    let p2 = after_vowel_and_consonant(chars, p1, is_vowel).unwrap_or(len);
    (p1.max(3), p2)
}

fn is_s_ending(c: char) -> bool {
    matches!(
        c,
        'b' | 'd' | 'f' | 'g' | 'h' | 'k' | 'l' | 'm' | 'n' | 'r' | 't'
    )
}

fn is_st_ending(c: char) -> bool {
    c != 'r' && is_s_ending(c)
}

fn is_et_ending(c: char) -> bool {
    matches!(
        c,
        'd' | 'f' | 'g' | 'k' | 'l' | 'm' | 'n' | 'r' | 's' | 't' | 'U' | 'z' | 'ä'
    )
}

fn standard_suffix(w: &mut Word, p1: usize, p2: usize) {
    step_1(w, p1);
    step_2(w, p1);
    step_3(w, p1, p2);
    apostrophe(w);
}

fn step_1(w: &mut Word, p1: usize) {
    const SUFFIXES: &[&str] = &[
        "em", "ern", "er", "erin", "erinnen", "e", "en", "es", "s", "ln", "lns",
    ];
    let Some((suffix, start)) = w.longest_suffix(w.len(), SUFFIXES, 0) else {
        return;
    };
    if start < p1 {
        return;
    }
    match suffix {
        "em" => {
            if w.suffix_start(start, "syst").is_none() {
                w.truncate(start);
            }
        }
        "e" | "en" | "es" => {
            w.truncate(start);
            if let Some(s) = w.ends_with("s")
                && w.suffix_start(s, "nis").is_some()
            {
                w.truncate(s);
            }
        }
        "s" => {
            if w.before(start).is_some_and(is_s_ending) {
                w.truncate(start);
            }
        }
        "ln" | "lns" => w.replace_tail(start, "l"),
        // `ern`, `er`, `erin`, `erinnen`.
        _ => w.truncate(start),
    }
}

fn step_2(w: &mut Word, p1: usize) {
    const SUFFIXES: &[&str] = &["en", "er", "est", "st", "et"];
    const ET_EXCEPTIONS: &[&str] = &["geordn", "intern", "plan", "tick", "tr"];
    let Some((suffix, start)) = w.longest_suffix(w.len(), SUFFIXES, 0) else {
        return;
    };
    if start < p1 {
        return;
    }
    match suffix {
        "st" => {
            // A valid `st` ending, itself preceded by at least 3 letters.
            if w.before(start).is_some_and(is_st_ending) && start >= 4 {
                w.truncate(start);
            }
        }
        "et" => {
            if w.before(start).is_some_and(is_et_ending)
                && w.longest_suffix(start, ET_EXCEPTIONS, 0).is_none()
            {
                w.truncate(start);
            }
        }
        // `en`, `er`, `est`.
        _ => w.truncate(start),
    }
}

fn step_3(w: &mut Word, p1: usize, p2: usize) {
    const SUFFIXES: &[&str] = &["end", "ung", "ig", "ik", "isch", "lich", "heit", "keit"];
    let Some((suffix, start)) = w.longest_suffix(w.len(), SUFFIXES, 0) else {
        return;
    };
    if start < p2 {
        return;
    }
    match suffix {
        "end" | "ung" => {
            w.truncate(start);
            if let Some(ig) = w.ends_with("ig")
                && w.before(ig) != Some('e')
                && ig >= p2
            {
                w.truncate(ig);
            }
        }
        "ig" | "ik" | "isch" => {
            if w.before(start) != Some('e') {
                w.truncate(start);
            }
        }
        "lich" | "heit" => {
            w.truncate(start);
            if let Some((_, s)) = w.longest_suffix(w.len(), &["er", "en"], 0)
                && s >= p1
            {
                w.truncate(s);
            }
        }
        // `keit`.
        _ => {
            w.truncate(start);
            if let Some((_, s)) = w.longest_suffix(w.len(), &["lich", "ig"], 0)
                && s >= p2
            {
                w.truncate(s);
            }
        }
    }
}

/// `'s`, `'sch` or `'` after at least two characters (`Grimm'sch` → `Grimm`).
fn apostrophe(w: &mut Word) {
    if let Some((_, start)) = w.longest_suffix(w.len(), &["'s", "'sch", "'"], 0)
        && start >= 2
    {
        w.truncate(start);
    }
}
