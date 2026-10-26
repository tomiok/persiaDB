//! The Snowball English stemmer ("Porter2"), as `algorithms/english.sbl` of Snowball v3.1.1.

use std::borrow::Cow;

use super::CharStemmer;
use super::word::{Word, after_vowel_and_consonant};
use crate::Scratch;

/// The Snowball English stemmer, Porter2 (SPEC §6.3.2): `running` → `run`, `generalizations` → `general`,
/// `skies` → `sky`.
///
/// Expects lowercase text (run it after [`LowercaseFilter`](crate::LowercaseFilter)). Only U+0027 `'` counts as an
/// apostrophe; characters other than ASCII letters count as consonants.
///
/// ```
/// use persia_analysis::{EnglishStemmer, Stemmer};
///
/// let stems: Vec<_> = ["running", "happily", "generously", "dying"]
///     .iter()
///     .map(|w| EnglishStemmer.stem(w).into_owned())
///     .collect();
/// assert_eq!(stems, ["run", "happili", "generous", "die"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnglishStemmer;

impl CharStemmer for EnglishStemmer {
    fn shortcut<'a>(&self, word: &'a str) -> Option<Cow<'a, str>> {
        if let Some(stem) = exception(word) {
            // An invariant entry (`news`, `sky`) borrows the word itself, as `Stemmer::stem` promises.
            return Some(Cow::Borrowed(if stem == word { word } else { stem }));
        }
        // Words of fewer than 3 characters are left alone.
        word.chars().nth(2).is_none().then_some(Cow::Borrowed(word))
    }

    fn stem_chars(&self, word: &str, scratch: &mut Scratch) {
        let mut w = Word::new(&mut scratch.chars, word);
        let y_found = prelude(&mut w);
        let (p1, p2) = regions(w.chars());
        step_1a(&mut w);
        step_1b(&mut w, p1);
        step_1c(&mut w);
        step_2(&mut w, p1);
        step_3(&mut w, p1, p2);
        step_4(&mut w, p2);
        step_5(&mut w, p1, p2);
        if y_found {
            for c in w.chars_mut() {
                if *c == 'Y' {
                    *c = 'y';
                }
            }
        }
    }
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}

/// Words stemmed by a table: irregular forms, `-ly` words the steps would overstem, and invariant words.
fn exception(word: &str) -> Option<&'static str> {
    Some(match word {
        "skis" => "ski",
        "skies" | "sky" => "sky",
        "idly" => "idl",
        "gently" => "gentl",
        "ugly" => "ugli",
        "early" => "earli",
        "only" => "onli",
        "singly" => "singl",
        "news" => "news",
        "howe" => "howe",
        "atlas" => "atlas",
        "cosmos" => "cosmos",
        "bias" => "bias",
        "andes" => "andes",
        _ => return None,
    })
}

/// Drops a leading apostrophe and marks consonant `y`s (initial, or after a vowel) as `Y`. Returns whether it
/// marked any.
fn prelude(w: &mut Word<'_>) -> bool {
    if w.chars().first() == Some(&'\'') {
        w.remove_first();
    }
    let mut y_found = false;
    let mut previous: Option<char> = None;
    for c in w.chars_mut() {
        if *c == 'y' && previous.is_none_or(is_vowel) {
            *c = 'Y';
            y_found = true;
        }
        previous = Some(*c);
    }
    y_found
}

/// `R1` and `R2`; `R1` starts after one of a few prefixes the default rule would place too early or too late.
fn regions(chars: &[char]) -> (usize, usize) {
    const PREFIXES: &[&str] = &[
        "gener", "commun", "arsen", "past", "univers", "later", "emerg", "organ", "inter",
    ];
    let len = chars.len();
    let prefix = PREFIXES.iter().find_map(|p| {
        let n = p.chars().count();
        chars
            .get(..n)
            .is_some_and(|head| head.iter().copied().eq(p.chars()))
            .then_some(n)
    });
    match prefix.or_else(|| after_vowel_and_consonant(chars, 0, is_vowel)) {
        Some(p1) => (
            p1,
            after_vowel_and_consonant(chars, p1, is_vowel).unwrap_or(len),
        ),
        None => (len, len),
    }
}

/// Whether the word up to `end` ends in a short syllable: non-vowel, vowel, non-vowel other than `w`, `x`, `Y`;
/// or vowel, non-vowel at the start of the word; or `past`.
fn ends_short(w: &Word<'_>, end: usize) -> bool {
    let at = |back: usize| {
        end.checked_sub(back)
            .and_then(|i| w.chars().get(i))
            .copied()
    };
    let last_three = match (at(3), at(2), at(1)) {
        (Some(a), Some(b), Some(c)) => {
            !is_vowel(a) && is_vowel(b) && !is_vowel(c) && !matches!(c, 'w' | 'x' | 'Y')
        }
        _ => false,
    };
    let two_letter_word =
        end == 2 && matches!((at(2), at(1)), (Some(a), Some(b)) if is_vowel(a) && !is_vowel(b));
    last_three || two_letter_word || w.suffix_start(end, "past").is_some()
}

fn has_vowel(w: &Word<'_>, end: usize) -> bool {
    w.chars()
        .get(..end)
        .is_some_and(|head| head.iter().copied().any(is_vowel))
}

/// Possessives and plurals.
fn step_1a(w: &mut Word<'_>) {
    if let Some((_, start)) = w.longest_suffix(w.len(), &["'", "'s", "'s'"], 0) {
        w.truncate(start);
    }
    match w.longest_suffix(w.len(), &["sses", "ied", "ies", "s", "us", "ss"], 0) {
        Some(("sses", start)) => w.replace_tail(start, "ss"),
        Some(("ied" | "ies", start)) => w.replace_tail(start, if start >= 2 { "i" } else { "ie" }),
        // `s` goes when a vowel precedes the letter before it: `gaps` → `gap`, but `gas` stays.
        Some(("s", start)) if start.checked_sub(1).is_some_and(|end| has_vowel(w, end)) => {
            w.truncate(start);
        }
        _ => {}
    }
}

/// `-eed`, `-ed`, `-ing` and their `-ly` forms.
fn step_1b(w: &mut Word<'_>, p1: usize) {
    const ENDINGS: &[&str] = &[
        "at", "bl", "iz", "bb", "dd", "ff", "gg", "mm", "nn", "pp", "rr", "tt",
    ];
    let Some((suffix, start)) =
        w.longest_suffix(w.len(), &["eed", "eedly", "ed", "edly", "ing", "ingly"], 0)
    else {
        return;
    };
    match suffix {
        "eed" | "eedly" => {
            let exceptional = w
                .longest_suffix(start, &["proc", "exc", "succ"], 0)
                .is_some_and(|(_, s)| s == 0);
            if start >= p1 && !exceptional {
                w.replace_tail(start, "ee");
            }
            return;
        }
        "ing" => match w.longest_suffix(
            start,
            &["y", "inn", "out", "cann", "herr", "earr", "even"],
            0,
        ) {
            // dying → die, lying → lie.
            Some(("y", 1)) if w.chars().first().is_some_and(|&c| !is_vowel(c)) => {
                w.replace_tail(1, "ie");
                return;
            }
            // inning, outing, canning, herring, earring, evening.
            Some((exception, 0)) if exception != "y" => return,
            _ => {}
        },
        _ => {}
    }
    if !has_vowel(w, start) {
        return;
    }
    w.truncate(start);
    match w.longest_suffix(w.len(), ENDINGS, 0) {
        Some(("at" | "bl" | "iz", _)) => w.push('e'),
        // A double consonant loses a letter, unless the word is a, e or o before it (`add`, `egg`, `off`).
        Some((_, start)) => {
            let short = start == 1
                && w.chars()
                    .first()
                    .is_some_and(|c| matches!(c, 'a' | 'e' | 'o'));
            if !short {
                w.pop();
            }
        }
        None if p1 == w.len() && ends_short(w, w.len()) => w.push('e'),
        None => {}
    }
}

/// A final `y` after a consonant that is not the first letter becomes `i`.
fn step_1c(w: &mut Word<'_>) {
    // The consonant before the `y` must not be the first letter, so the `y` is at least the third.
    let Some(last) = w.len().checked_sub(1).filter(|&last| last >= 2) else {
        return;
    };
    if matches!(w.chars().get(last), Some('y' | 'Y'))
        && w.before(last).is_some_and(|c| !is_vowel(c))
    {
        w.replace_tail(last, "i");
    }
}

fn step_2(w: &mut Word<'_>, p1: usize) {
    const SUFFIXES: &[&str] = &[
        "tional", "enci", "anci", "abli", "entli", "izer", "ization", "ational", "ation", "ator",
        "alism", "aliti", "alli", "fulness", "ousli", "ousness", "iveness", "iviti", "biliti",
        "bli", "ogist", "ogi", "fulli", "lessli", "li",
    ];
    let Some((suffix, start)) = w.longest_suffix(w.len(), SUFFIXES, 0) else {
        return;
    };
    if start < p1 {
        return;
    }
    let with = match suffix {
        "tional" => "tion",
        "enci" => "ence",
        "anci" => "ance",
        "abli" => "able",
        "entli" => "ent",
        "izer" | "ization" => "ize",
        "ational" | "ation" | "ator" => "ate",
        "alism" | "aliti" | "alli" => "al",
        "fulness" | "fulli" => "ful",
        "ousli" | "ousness" => "ous",
        "iveness" | "iviti" => "ive",
        "biliti" | "bli" => "ble",
        "ogist" => "og",
        "ogi" if w.before(start) == Some('l') => "og",
        "lessli" => "less",
        "li" if w.before(start).is_some_and(|c| "cdeghkmnrt".contains(c)) => "",
        _ => return,
    };
    w.replace_tail(start, with);
}

fn step_3(w: &mut Word<'_>, p1: usize, p2: usize) {
    const SUFFIXES: &[&str] = &[
        "tional", "ational", "alize", "icate", "iciti", "ical", "ful", "ness", "ative",
    ];
    let Some((suffix, start)) = w.longest_suffix(w.len(), SUFFIXES, 0) else {
        return;
    };
    if start < p1 {
        return;
    }
    let with = match suffix {
        "tional" => "tion",
        "ational" => "ate",
        "alize" => "al",
        "icate" | "iciti" | "ical" => "ic",
        "ful" | "ness" => "",
        "ative" if start >= p2 => "",
        _ => return,
    };
    w.replace_tail(start, with);
}

fn step_4(w: &mut Word<'_>, p2: usize) {
    const SUFFIXES: &[&str] = &[
        "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ism",
        "ate", "iti", "ous", "ive", "ize", "ion",
    ];
    let Some((suffix, start)) = w.longest_suffix(w.len(), SUFFIXES, 0) else {
        return;
    };
    if start < p2 || (suffix == "ion" && !matches!(w.before(start), Some('s' | 't'))) {
        return;
    }
    w.truncate(start);
}

fn step_5(w: &mut Word<'_>, p1: usize, p2: usize) {
    match w.longest_suffix(w.len(), &["e", "l"], 0) {
        Some(("e", start)) => {
            if start >= p2 || (start >= p1 && !ends_short(w, start)) {
                w.truncate(start);
            }
        }
        Some(("l", start)) if start >= p2 && w.before(start) == Some('l') => w.truncate(start),
        _ => {}
    }
}
