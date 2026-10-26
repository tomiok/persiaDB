//! The Snowball Italian stemmer, as `algorithms/italian.sbl` of Snowball v3.1.1.

use std::borrow::Cow;

use super::Stemmer;
use super::word::{Word, finish, r1_r2, rv};

/// The Snowball Italian stemmer (SPEC §6.3.2): `abbandonata` → `abbandon`, `nell'estate` → `estat`,
/// `velocemente` → `veloc`.
///
/// Expects lowercase text with its accents (run it after [`LowercaseFilter`](crate::LowercaseFilter) and before
/// [`FoldingFilter`](crate::FoldingFilter)). It removes the elided articles and prepositions before U+0027
/// (`l'`, `dell'`, `un'`, ...), and reads acute accents as grave ones (`é` as `è`), which it keeps.
///
/// ```
/// use persia_analysis::{ItalianStemmer, Stemmer};
///
/// let stems: Vec<_> = ["abbandonata", "nell'estate", "velocemente", "città"]
///     .iter()
///     .map(|w| ItalianStemmer.stem(w).into_owned())
///     .collect();
/// assert_eq!(stems, ["abbandon", "estat", "veloc", "citt"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItalianStemmer;

impl Stemmer for ItalianStemmer {
    fn stem<'a>(&self, word: &'a str) -> Cow<'a, str> {
        let mut w = Word::from_chars(prelude(&elide(word)));
        let pv = regions_rv(w.chars());
        let (p1, p2) = r1_r2(w.chars(), is_vowel);
        attached_pronoun(&mut w, pv);
        if !standard_suffix(&mut w, pv, p1, p2) {
            verb_suffix(&mut w, pv);
        }
        vowel_suffix(&mut w, pv);
        for c in w.chars_mut() {
            *c = match *c {
                'I' => 'i',
                'U' => 'u',
                other => other,
            };
        }
        finish(word, w.chars())
    }
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'à' | 'è' | 'ì' | 'ò' | 'ù')
}

/// The word without a leading elided article or preposition, when something follows it.
fn elide(word: &str) -> Vec<char> {
    const ELISIONS: &[&str] = &[
        "d'", "l'", "m'", "s'", "t'", "v'", "all'", "dall'", "dell'", "gl'", "nell'", "quell'",
        "quest'", "sull'", "tutt'", "un'",
    ];
    let chars: Vec<char> = word.chars().collect();
    // At most one entry is a prefix: each ends at the first apostrophe.
    let elided = ELISIONS
        .iter()
        .map(|e| e.chars().count())
        .zip(ELISIONS)
        .find(|&(len, e)| len < chars.len() && chars.iter().copied().take(len).eq(e.chars()));
    match elided {
        Some((len, _)) => chars.get(len..).unwrap_or_default().to_vec(),
        None => chars,
    }
}

/// Acute accents written as grave ones, `qu` as `qU`, and `u` and `i` between vowels marked as consonants (`U`,
/// `I`).
fn prelude(chars: &[char]) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut after_q = false;
    for &c in chars {
        out.push(match c {
            'á' => 'à',
            'é' => 'è',
            'í' => 'ì',
            'ó' => 'ò',
            'ú' => 'ù',
            'u' if after_q => 'U',
            other => other,
        });
        after_q = c == 'q';
    }
    // Snowball's `repeat goto (v [('u'] v <- 'U') or ('i'] v <- 'I'))`: `goto` leaves the cursor where a match starts, so
    // the search goes on from the next letter, and the vowel after a marked letter can start the next match.
    let mut i = 0;
    while let (Some(&a), Some(&b), Some(&c)) = (
        out.get(i),
        out.get(i.saturating_add(1)),
        out.get(i.saturating_add(2)),
    ) {
        if is_vowel(a)
            && matches!(b, 'u' | 'i')
            && is_vowel(c)
            && let Some(b) = out.get_mut(i.saturating_add(1))
        {
            *b = b.to_ascii_uppercase();
        }
        i = i.saturating_add(1);
    }
    out
}

/// `RV` as in Spanish, except that it starts after `divan` in words beginning so (`divano` must not stem like
/// `diva`).
fn regions_rv(chars: &[char]) -> usize {
    let starts_divan = chars.iter().copied().take(5).eq("divan".chars());
    let vowel_first = chars.first().copied().is_some_and(is_vowel);
    let general = rv(chars, is_vowel);
    // The vowel-initial rule wins when it finds a position; `divan` comes before the consonant-initial rule.
    if !vowel_first && starts_divan {
        5
    } else {
        general
    }
}

/// An enclitic pronoun after a gerund or an infinitive in RV: `-ando`/`-endo` lose the pronoun, `-ar`/`-er`/`-ir`
/// get `e` for it (`mangiarla` → `mangiare`).
fn attached_pronoun(w: &mut Word, pv: usize) {
    const PRONOUNS: &[&str] = &[
        "ci", "gli", "la", "le", "li", "lo", "mi", "ne", "si", "ti", "vi", "sene", "gliela",
        "gliele", "glieli", "glielo", "gliene", "mela", "mele", "meli", "melo", "mene", "tela",
        "tele", "teli", "telo", "tene", "cela", "cele", "celi", "celo", "cene", "vela", "vele",
        "veli", "velo", "vene",
    ];
    const FORMS: &[&str] = &["ando", "endo", "ar", "er", "ir"];
    let Some((_, pronoun)) = w.longest_suffix(w.len(), PRONOUNS, 0) else {
        return;
    };
    let Some((form, start)) = w.longest_suffix(pronoun, FORMS, 0) else {
        return;
    };
    if start < pv {
        return;
    }
    match form {
        "ando" | "endo" => w.truncate(pronoun),
        _ => w.replace_tail(pronoun, "e"),
    }
}

#[derive(Debug, Clone, Copy)]
enum Standard {
    /// Delete in R2.
    Delete,
    /// Delete in R2, then a preceding `ic` in R2.
    DeleteIc,
    /// Replace with `log` in R2.
    Log,
    /// Replace with `u` in R2.
    U,
    /// Replace with `ente` in R2.
    Ente,
    /// Delete in RV.
    DeleteRv,
    /// `-amente`: delete in R1, then a preceding `iv` (and `at` before it), `os`, `ic` or `abil` in R2.
    Amente,
    /// `-ità`: delete in R2, then a preceding `abil`, `ic` or `iv` in R2.
    Ita,
    /// `-ivo` and its forms: delete in R2, then a preceding `at` in R2, and `ic` before that in R2.
    Iv,
}

const STANDARD: &[(&str, Standard)] = &[
    ("anza", Standard::Delete),
    ("anze", Standard::Delete),
    ("ico", Standard::Delete),
    ("ici", Standard::Delete),
    ("ica", Standard::Delete),
    ("ice", Standard::Delete),
    ("iche", Standard::Delete),
    ("ichi", Standard::Delete),
    ("ismo", Standard::Delete),
    ("ismi", Standard::Delete),
    ("abile", Standard::Delete),
    ("abili", Standard::Delete),
    ("ibile", Standard::Delete),
    ("ibili", Standard::Delete),
    ("ista", Standard::Delete),
    ("iste", Standard::Delete),
    ("isti", Standard::Delete),
    ("istà", Standard::Delete),
    ("istè", Standard::Delete),
    ("istì", Standard::Delete),
    ("oso", Standard::Delete),
    ("osi", Standard::Delete),
    ("osa", Standard::Delete),
    ("ose", Standard::Delete),
    ("mente", Standard::Delete),
    ("atrice", Standard::Delete),
    ("atrici", Standard::Delete),
    ("ante", Standard::Delete),
    ("anti", Standard::Delete),
    ("azione", Standard::DeleteIc),
    ("azioni", Standard::DeleteIc),
    ("atore", Standard::DeleteIc),
    ("atori", Standard::DeleteIc),
    ("logia", Standard::Log),
    ("logie", Standard::Log),
    ("uzione", Standard::U),
    ("uzioni", Standard::U),
    ("usione", Standard::U),
    ("usioni", Standard::U),
    ("enza", Standard::Ente),
    ("enze", Standard::Ente),
    ("amento", Standard::DeleteRv),
    ("amenti", Standard::DeleteRv),
    ("imento", Standard::DeleteRv),
    ("imenti", Standard::DeleteRv),
    ("amente", Standard::Amente),
    ("ità", Standard::Ita),
    ("ivo", Standard::Iv),
    ("ivi", Standard::Iv),
    ("iva", Standard::Iv),
    ("ive", Standard::Iv),
];

/// Deletes the `suffix` ending the word if it starts at or after `from`; whether it did.
fn delete_from(w: &mut Word, suffix: &str, from: usize) -> bool {
    match w.ends_with(suffix) {
        Some(start) if start >= from => {
            w.truncate(start);
            true
        }
        _ => false,
    }
}

/// The longest standard suffix, if its rule's region holds; `false` (so verb suffixes are tried) otherwise.
fn standard_suffix(w: &mut Word, pv: usize, p1: usize, p2: usize) -> bool {
    let Some((rule, start)) = w.longest_rule(STANDARD, 0) else {
        return false;
    };
    let region = match rule {
        Standard::DeleteRv => pv,
        Standard::Amente => p1,
        _ => p2,
    };
    if start < region {
        return false;
    }
    match rule {
        Standard::Delete | Standard::DeleteRv => w.truncate(start),
        Standard::DeleteIc => {
            w.truncate(start);
            delete_from(w, "ic", p2);
        }
        Standard::Log => w.replace_tail(start, "log"),
        Standard::U => w.replace_tail(start, "u"),
        Standard::Ente => w.replace_tail(start, "ente"),
        Standard::Amente => {
            w.truncate(start);
            if let Some((suffix, s)) = w.longest_suffix(w.len(), &["iv", "os", "ic", "abil"], 0)
                && s >= p2
            {
                w.truncate(s);
                if suffix == "iv" {
                    delete_from(w, "at", p2);
                }
            }
        }
        Standard::Ita => {
            w.truncate(start);
            if let Some((_, s)) = w.longest_suffix(w.len(), &["abil", "ic", "iv"], 0)
                && s >= p2
            {
                w.truncate(s);
            }
        }
        Standard::Iv => {
            w.truncate(start);
            if delete_from(w, "at", p2) {
                delete_from(w, "ic", p2);
            }
        }
    }
    true
}

/// The longest verb ending inside RV.
fn verb_suffix(w: &mut Word, pv: usize) {
    const VERB: &[&str] = &[
        "ammo", "ando", "ano", "are", "arono", "asse", "assero", "assi", "assimo", "ata", "ate",
        "ati", "ato", "ava", "avamo", "avano", "avate", "avi", "avo", "emmo", "enda", "ende",
        "endi", "endo", "erà", "erai", "eranno", "ere", "erebbe", "erebbero", "erei", "eremmo",
        "eremo", "ereste", "eresti", "erete", "erò", "erono", "essero", "ete", "eva", "evamo",
        "evano", "evate", "evi", "evo", "Yamo", "iamo", "immo", "irà", "irai", "iranno", "ire",
        "irebbe", "irebbero", "irei", "iremmo", "iremo", "ireste", "iresti", "irete", "irò",
        "irono", "isca", "iscano", "isce", "isci", "isco", "iscono", "issero", "ita", "ite", "iti",
        "ito", "iva", "ivamo", "ivano", "ivate", "ivi", "ivo", "ono", "uta", "ute", "uti", "uto",
        "ar", "ir",
    ];
    if let Some((_, start)) = w.longest_suffix(w.len(), VERB, pv) {
        w.truncate(start);
    }
}

/// A final vowel in RV, then an `i` before it in RV; an `h` after `c` or `g` in RV (`amiche` → `amic`).
fn vowel_suffix(w: &mut Word, pv: usize) {
    let len = w.len();
    let last = w.before(len);
    if let Some(start) = len.checked_sub(1)
        && last.is_some_and(|c| matches!(c, 'a' | 'e' | 'i' | 'o' | 'à' | 'è' | 'ì' | 'ò'))
        && start >= pv
    {
        w.truncate(start);
        if let Some(i) = w.ends_with("i")
            && i >= pv
        {
            w.truncate(i);
        }
    }
    if let Some(h) = w.ends_with("h")
        && w.before(h).is_some_and(|c| c == 'c' || c == 'g')
        && h.checked_sub(1).is_some_and(|cg| cg >= pv)
    {
        w.truncate(h);
    }
}
