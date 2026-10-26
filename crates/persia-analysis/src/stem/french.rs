//! The Snowball French stemmer, as `algorithms/french.sbl` of Snowball v3.1.1.

use super::CharStemmer;
use super::word::{Word, r1_r2};
use crate::Scratch;

/// The Snowball French stemmer (SPEC §6.3.2): `continuellement` → `continuel`, `l'ambassadeur` → `ambassadeur`,
/// `généralement` → `général`.
///
/// Expects lowercase text with its accents (run it after [`LowercaseFilter`](crate::LowercaseFilter) and before
/// [`FoldingFilter`](crate::FoldingFilter)). It removes an elided `c'`, `d'`, `j'`, `l'`, `m'`, `n'`, `s'`, `t'` or
/// `qu'` before U+0027 at the start of the word.
///
/// ```
/// use persia_analysis::{FrenchStemmer, Stemmer};
///
/// let stems: Vec<_> = ["continuellement", "l'ambassadeur", "généralement", "chevaux"]
///     .iter()
///     .map(|w| FrenchStemmer.stem(w).into_owned())
///     .collect();
/// assert_eq!(stems, ["continuel", "ambassadeur", "général", "cheval"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrenchStemmer;

impl CharStemmer for FrenchStemmer {
    fn stem_chars(&self, word: &str, scratch: &mut Scratch) {
        let Scratch { chars, work, .. } = scratch;
        elide(word, work);
        prelude(work, chars);
        let mut w = Word::from_chars(chars);
        let pv = rv(w.chars());
        let (p1, p2) = r1_r2(w.chars(), is_vowel);
        let r = Regions { pv, p1, p2 };
        let suffix_removed = match standard_suffix(&mut w, r) {
            Some(true) => true,
            // `false`: the step rewrote the word but asks for the verb steps (`-ment` after a vowel).
            Some(false) | None => i_verb_suffix(&mut w, pv) || verb_suffix(&mut w, r),
        };
        if suffix_removed {
            let last = w.len().checked_sub(1);
            match w.before(w.len()) {
                Some('Y') => replace_last(&mut w, last, 'i'),
                Some('ç') => replace_last(&mut w, last, 'c'),
                _ => {}
            }
        } else {
            residual_suffix(&mut w, r);
        }
        un_double(&mut w);
        un_accent(&mut w);
        postlude(w.chars(), work);
        std::mem::swap(chars, work);
    }
}

#[derive(Debug, Clone, Copy)]
struct Regions {
    pv: usize,
    p1: usize,
    p2: usize,
}

fn is_vowel(c: char) -> bool {
    matches!(
        c,
        'a' | 'e'
            | 'i'
            | 'o'
            | 'u'
            | 'y'
            | 'â'
            | 'à'
            | 'ë'
            | 'é'
            | 'ê'
            | 'è'
            | 'ï'
            | 'î'
            | 'ô'
            | 'û'
            | 'ù'
    )
}

fn replace_last(w: &mut Word<'_>, last: Option<usize>, c: char) {
    if let Some(slot) = last.and_then(|i| w.chars_mut().get_mut(i)) {
        *slot = c;
    }
}

/// The word without a leading elided `c'`, `d'`, `j'`, `l'`, `m'`, `n'`, `s'`, `t'` or `qu'`, when something
/// follows it.
fn elide(word: &str, chars: &mut Vec<char>) {
    chars.clear();
    chars.extend(word.chars());
    let prefix = match chars.as_slice() {
        ['c' | 'd' | 'j' | 'l' | 'm' | 'n' | 's' | 't', '\'', _, ..] => 2,
        ['q', 'u', '\'', _, ..] => 3,
        _ => 0,
    };
    chars.drain(..prefix);
}

/// Marks `u`, `i` and `y` that act as consonants (`U`, `I`, `Y`), and writes `ë`, `ï` as `He`, `Hi` (Snowball's
/// `repeat goto (...)`: `goto` leaves the cursor where a match starts, and nothing matches again at a
/// rewritten letter, so the search goes on from the next letter).
/// Rewrites `c` while it writes the result to `out`.
fn prelude(c: &mut Vec<char>, out: &mut Vec<char>) {
    out.clear();
    let mut i = 0;
    while let Some(&first) = c.get(i) {
        let mut cur = first;
        // Snowball retries at the same position after a match (each one rewrites a letter, so this ends): a vowel
        // whose next letter was just marked can still be an `ë` or `ï` to rewrite (`ëie` → `HeIe`).
        loop {
            let next = c.get(i.saturating_add(1)).copied();
            let after = c.get(i.saturating_add(2)).copied();
            let mark = |c: &mut Vec<char>, to: char| {
                if let Some(slot) = c.get_mut(i.saturating_add(1)) {
                    *slot = to;
                }
            };
            if is_vowel(cur) && matches!(next, Some('u' | 'i')) && after.is_some_and(is_vowel) {
                // A vowel, then `u` or `i` before a vowel.
                mark(c, if next == Some('u') { 'U' } else { 'I' });
            } else if is_vowel(cur) && next == Some('y') {
                mark(c, 'Y');
            } else if cur == 'ë' || cur == 'ï' {
                // `He` or `Hi`: the `H` goes out, and its letter is the next position.
                out.push('H');
                cur = if cur == 'ë' { 'e' } else { 'i' };
            } else if cur == 'y' && next.is_some_and(is_vowel) {
                cur = 'Y';
            } else if cur == 'q' && next == Some('u') {
                mark(c, 'U');
            } else {
                break;
            }
        }
        out.push(cur);
        i = i.saturating_add(1);
    }
}

/// `RV`: after the third letter if the first two are vowels, or after `par`, `col`, `tap` or `ni` + vowel at the
/// start; otherwise after the first vowel that is not the first letter.
fn rv(chars: &[char]) -> usize {
    let len = chars.len();
    let vowel = |i: usize| chars.get(i).copied().is_some_and(is_vowel);
    if vowel(0) && vowel(1) && len >= 3 {
        return 3;
    }
    let starts = |prefix: &str| {
        chars
            .iter()
            .copied()
            .take(prefix.chars().count())
            .eq(prefix.chars())
    };
    if starts("par") || starts("col") || starts("tap") {
        return 3;
    }
    if starts("ni") && vowel(2) {
        return 3;
    }
    chars
        .iter()
        .enumerate()
        .skip(1)
        .find(|&(_, &c)| is_vowel(c))
        .and_then(|(i, _)| i.checked_add(1))
        .unwrap_or(len)
}

/// Writes the consonant marks back as letters: `I`, `U`, `Y` as `i`, `u`, `y`, `He`, `Hi` as `ë`, `ï`, and drops
/// any other `H`.
fn postlude(chars: &[char], out: &mut Vec<char>) {
    out.clear();
    let mut rest = chars;
    while let Some((&c, tail)) = rest.split_first() {
        rest = tail;
        match (c, tail.first()) {
            ('I', _) => out.push('i'),
            ('U', _) => out.push('u'),
            ('Y', _) => out.push('y'),
            ('H', Some('e')) => {
                out.push('ë');
                rest = tail.get(1..).unwrap_or_default();
            }
            ('H', Some('i')) => {
                out.push('ï');
                rest = tail.get(1..).unwrap_or_default();
            }
            ('H', _) => {}
            (other, _) => out.push(other),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Standard {
    /// Delete in R2.
    Delete,
    /// Delete in R2, then a preceding `ic`: deleted in R2, else written `iqU`.
    DeleteIc,
    /// Replace with `log` in R2.
    Log,
    /// Replace with `u` in R2.
    U,
    /// Replace with `ent` in R2.
    Ent,
    /// `-ement`: delete in RV, then handle a preceding `iv`, `eus`, `abl`, `iqU`, `ièr` or `Ièr`.
    Ement,
    /// `-ité`: delete in R2, then handle a preceding `abil`, `ic` or `iv`.
    Ite,
    /// `-if`, `-ive`: delete in R2, then a preceding `at` in R2, and `ic` before it.
    If,
    /// `-eaux` → `eau`.
    Eaux,
    /// `-aux` → `al` in R1.
    Aux,
    /// `-oux` → `ou` after `b`, `h`, `j`, `l`, `n` or `p`.
    Oux,
    /// `-euse`: delete in R2, else `eux` in R1.
    Euse,
    /// `-issement`: delete in R1 after a non-vowel.
    Issement,
    /// `-amment` → `ant` in RV, then the verb steps.
    Amment,
    /// `-emment` → `ent` in RV, then the verb steps.
    Emment,
    /// `-ment` after a vowel in RV: delete, then the verb steps.
    Ment,
}

const STANDARD: &[(&str, Standard)] = &[
    ("ance", Standard::Delete),
    ("iqUe", Standard::Delete),
    ("isme", Standard::Delete),
    ("able", Standard::Delete),
    ("iste", Standard::Delete),
    ("eux", Standard::Delete),
    ("ances", Standard::Delete),
    ("iqUes", Standard::Delete),
    ("ismes", Standard::Delete),
    ("ables", Standard::Delete),
    ("istes", Standard::Delete),
    ("atrice", Standard::DeleteIc),
    ("ateur", Standard::DeleteIc),
    ("ation", Standard::DeleteIc),
    ("atrices", Standard::DeleteIc),
    ("ateurs", Standard::DeleteIc),
    ("ations", Standard::DeleteIc),
    ("logie", Standard::Log),
    ("logies", Standard::Log),
    ("usion", Standard::U),
    ("ution", Standard::U),
    ("usions", Standard::U),
    ("utions", Standard::U),
    ("ence", Standard::Ent),
    ("ences", Standard::Ent),
    ("ement", Standard::Ement),
    ("ements", Standard::Ement),
    ("ité", Standard::Ite),
    ("ités", Standard::Ite),
    ("if", Standard::If),
    ("ive", Standard::If),
    ("ifs", Standard::If),
    ("ives", Standard::If),
    ("eaux", Standard::Eaux),
    ("aux", Standard::Aux),
    ("oux", Standard::Oux),
    ("euse", Standard::Euse),
    ("euses", Standard::Euse),
    ("issement", Standard::Issement),
    ("issements", Standard::Issement),
    ("amment", Standard::Amment),
    ("emment", Standard::Emment),
    ("ment", Standard::Ment),
    ("ments", Standard::Ment),
];

/// The suffix `s` ending the word, when it starts at or after `from`: its start.
fn at_end(w: &Word<'_>, s: &str, from: usize) -> Option<usize> {
    w.ends_with(s).filter(|&start| start >= from)
}

/// A preceding `ic`: deleted in R2, else written `iqU`.
fn ic(w: &mut Word<'_>, p2: usize) {
    if let Some(start) = w.ends_with("ic") {
        if start >= p2 {
            w.truncate(start);
        } else {
            w.replace_tail(start, "iqU");
        }
    }
}

/// The longest standard suffix and its rule: `None` if none applies, `Some(true)` if it was removed, `Some(false)`
/// if the word was rewritten but the verb steps must still run (Snowball's `fail(...)`).
fn standard_suffix(w: &mut Word<'_>, r: Regions) -> Option<bool> {
    let (rule, start) = w.longest_rule(STANDARD, 0)?;
    let Regions { pv, p1, p2 } = r;
    let in_r2 = start >= p2;
    match rule {
        Standard::Delete if in_r2 => w.truncate(start),
        Standard::DeleteIc if in_r2 => {
            w.truncate(start);
            ic(w, p2);
        }
        Standard::Log if in_r2 => w.replace_tail(start, "log"),
        Standard::U if in_r2 => w.replace_tail(start, "u"),
        Standard::Ent if in_r2 => w.replace_tail(start, "ent"),
        Standard::Ement if start >= pv => {
            w.truncate(start);
            ement_tail(w, r);
        }
        Standard::Ite if in_r2 => {
            w.truncate(start);
            if let Some((s, at)) = w.longest_suffix(w.len(), &["abil", "ic", "iv"], 0) {
                match s {
                    "abil" if at >= p2 => w.truncate(at),
                    "abil" => w.replace_tail(at, "abl"),
                    "ic" => ic(w, p2),
                    _ if at >= p2 => w.truncate(at),
                    _ => {}
                }
            }
        }
        Standard::If if in_r2 => {
            w.truncate(start);
            if let Some(at) = at_end(w, "at", p2) {
                w.truncate(at);
                ic(w, p2);
            }
        }
        Standard::Eaux => w.replace_tail(start, "eau"),
        Standard::Aux if start >= p1 => w.replace_tail(start, "al"),
        Standard::Oux if w.before(start).is_some_and(|c| "bhjlnp".contains(c)) => {
            w.replace_tail(start, "ou");
        }
        Standard::Euse => {
            if in_r2 {
                w.truncate(start);
            } else if start >= p1 {
                w.replace_tail(start, "eux");
            } else {
                return None;
            }
        }
        Standard::Issement if start >= p1 && w.before(start).is_some_and(|c| !is_vowel(c)) => {
            w.truncate(start);
        }
        Standard::Amment if start >= pv => {
            w.replace_tail(start, "ant");
            return Some(false);
        }
        Standard::Emment if start >= pv => {
            w.replace_tail(start, "ent");
            return Some(false);
        }
        Standard::Ment
            if w.before(start).is_some_and(is_vowel)
                && start.checked_sub(1).is_some_and(|v| v >= pv) =>
        {
            w.truncate(start);
            return Some(false);
        }
        _ => return None,
    }
    Some(true)
}

/// After `-ement` is removed: `iv` (and `at` before it) in R2; `eus` deleted in R2 or written `eux` in R1; `abl`,
/// `iqU` in R2; `ièr`, `Ièr` written `i` in RV.
fn ement_tail(w: &mut Word<'_>, r: Regions) {
    let Regions { pv, p1, p2 } = r;
    let Some((s, at)) = w.longest_suffix(w.len(), &["iv", "eus", "abl", "iqU", "ièr", "Ièr"], 0)
    else {
        return;
    };
    match s {
        "iv" if at >= p2 => {
            w.truncate(at);
            if let Some(a) = at_end(w, "at", p2) {
                w.truncate(a);
            }
        }
        "eus" | "abl" | "iqU" if at >= p2 => w.truncate(at),
        "eus" if at >= p1 => w.replace_tail(at, "eux"),
        "ièr" | "Ièr" if at >= pv => w.replace_tail(at, "i"),
        _ => {}
    }
}

/// The longest `-ir` verb ending inside RV, after a non-vowel other than `H` also inside RV.
fn i_verb_suffix(w: &mut Word<'_>, pv: usize) -> bool {
    const SUFFIXES: &[&str] = &[
        "îmes", "ît", "îtes", "i", "ie", "ies", "ir", "ira", "irai", "iraIent", "irais", "irait",
        "iras", "irent", "irez", "iriez", "irions", "irons", "iront", "is", "issaIent", "issais",
        "issait", "issant", "issante", "issantes", "issants", "isse", "issent", "isses", "issez",
        "issiez", "issions", "issons", "it",
    ];
    let Some((_, start)) = w.longest_suffix(w.len(), SUFFIXES, pv) else {
        return false;
    };
    let before = start.checked_sub(1).filter(|&b| b >= pv);
    match before.and_then(|b| w.chars().get(b)) {
        Some(&c) if c != 'H' && !is_vowel(c) => {
            w.truncate(start);
            true
        }
        _ => false,
    }
}

/// The longest other verb ending inside RV.
fn verb_suffix(w: &mut Word<'_>, r: Regions) -> bool {
    const DELETE: &[&str] = &[
        "é", "ée", "ées", "és", "èrent", "er", "era", "erai", "eraIent", "erais", "erait", "eras",
        "erez", "eriez", "erions", "erons", "eront", "ez", "iez", "eais",
    ];
    const A: &[&str] = &[
        "âmes", "ât", "âtes", "a", "ai", "aIent", "ait", "ant", "ante", "antes", "ants", "as",
        "asse", "assent", "asses", "assiez", "assions",
    ];
    const AIS: &[&str] = &["ais", "aise", "aises"];
    let all = DELETE.iter().chain(A).chain(AIS).chain(&["ions"]);
    let Some((s, start)) = all
        .filter_map(|&s| at_end(w, s, r.pv).map(|start| (s, start)))
        .min_by_key(|&(_, start)| start)
    else {
        return false;
    };
    if s == "ions" {
        if start < r.p2 {
            return false;
        }
        w.truncate(start);
    } else if A.contains(&s) {
        // An `e` before the ending goes with it when it is in RV.
        let e = start
            .checked_sub(1)
            .filter(|&e| e >= r.pv && w.chars().get(e) == Some(&'e'));
        w.truncate(e.unwrap_or(start));
    } else if AIS.contains(&s) {
        let kept = w.suffix_start(start, "auv").is_some()
            || w.suffix_start(start, "épl").is_some()
            // `balais`, `palais`: `al` right after the first letter.
            || w.suffix_start(start, "al") == Some(1);
        if kept {
            return false;
        }
        w.truncate(start);
    } else {
        w.truncate(start);
    }
    true
}

/// When no other suffix was removed: a final `s` (unless after `a`, `i`, `o`, `u`, `è` or `s`, but after `Hi`), then
/// inside RV `-ion` after `s` or `t` in R2, `-ier`/`-ière` written `i`, or a final `e`.
fn residual_suffix(w: &mut Word<'_>, r: Regions) {
    if let Some(s) = w.ends_with("s") {
        let after_hi = w.suffix_start(s, "Hi").is_some();
        let removable = w
            .before(s)
            .is_some_and(|c| !matches!(c, 'a' | 'i' | 'o' | 'u' | 'è' | 's'));
        if after_hi || removable {
            w.truncate(s);
        }
    }
    let Some((s, start)) =
        w.longest_suffix(w.len(), &["ion", "ier", "ière", "Ier", "Ière", "e"], r.pv)
    else {
        return;
    };
    match s {
        "ion" => {
            let st = start
                .checked_sub(1)
                .filter(|&b| b >= r.pv)
                .and_then(|b| w.chars().get(b))
                .is_some_and(|&c| c == 's' || c == 't');
            if start >= r.p2 && st {
                w.truncate(start);
            }
        }
        "e" => w.truncate(start),
        _ => w.replace_tail(start, "i"),
    }
}

/// A final `enn`, `onn`, `ett`, `ell` or `eill` loses its last letter.
fn un_double(w: &mut Word<'_>) {
    if ["enn", "onn", "ett", "ell", "eill"]
        .iter()
        .any(|s| w.ends_with(s).is_some())
    {
        w.pop();
    }
}

/// An `é` or `è` before the final run of non-vowels becomes `e`.
fn un_accent(w: &mut Word<'_>) {
    let chars = w.chars();
    let Some(last_vowel) = chars.iter().rposition(|&c| is_vowel(c)) else {
        return;
    };
    if last_vowel
        .checked_add(1)
        .is_some_and(|after| after < chars.len())
        && matches!(chars.get(last_vowel), Some('é' | 'è'))
        && let Some(c) = w.chars_mut().get_mut(last_vowel)
    {
        *c = 'e';
    }
}
