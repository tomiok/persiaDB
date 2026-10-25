//! The Snowball Spanish stemmer, as `algorithms/spanish.sbl` of Snowball 3.

use std::borrow::Cow;

use super::Stemmer;
use super::word::{Word, finish, r1_r2, rv};

/// The Snowball Spanish stemmer (SPEC §6.3.2): `canciones` → `cancion`, `rápidamente` → `rapid`,
/// `diciéndole` → `dic`.
///
/// Expects lowercase text with its accents (run it after [`LowercaseFilter`](crate::LowercaseFilter) and before
/// [`FoldingFilter`](crate::FoldingFilter)); it removes the acute accents of `á`, `é`, `í`, `ó` and `ú`, and keeps
/// `ñ` and `ü`.
///
/// ```
/// use persia_analysis::{SpanishStemmer, Stemmer};
///
/// let stems: Vec<_> = ["canciones", "rápidamente", "diciéndole", "niños"]
///     .iter()
///     .map(|w| SpanishStemmer.stem(w).into_owned())
///     .collect();
/// assert_eq!(stems, ["cancion", "rapid", "dic", "niñ"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpanishStemmer;

impl Stemmer for SpanishStemmer {
    fn stem<'a>(&self, word: &'a str) -> Cow<'a, str> {
        let mut w = Word::new(word);
        let pv = rv(w.chars(), is_vowel);
        let (p1, p2) = r1_r2(w.chars(), is_vowel);
        attached_pronoun(&mut w, pv);
        if !standard_suffix(&mut w, p1, p2) && !y_verb_suffix(&mut w, pv) {
            verb_suffix(&mut w, pv);
        }
        residual_suffix(&mut w, pv);
        for c in w.chars_mut() {
            *c = match *c {
                'á' => 'a',
                'é' => 'e',
                'í' => 'i',
                'ó' => 'o',
                'ú' => 'u',
                other => other,
            };
        }
        finish(word, w.chars())
    }
}

fn is_vowel(c: char) -> bool {
    matches!(
        c,
        'a' | 'e' | 'i' | 'o' | 'u' | 'á' | 'é' | 'í' | 'ó' | 'ú' | 'ü'
    )
}

/// Enclitic pronouns after a gerund or an infinitive in RV: `diciéndole` → `diciendo`, `comerlo` → `comer`.
fn attached_pronoun(w: &mut Word, pv: usize) {
    const PRONOUNS: &[&str] = &[
        "me", "se", "sela", "selo", "selas", "selos", "la", "le", "lo", "las", "les", "los", "nos",
    ];
    const FORMS: &[&str] = &[
        "iéndo", "ándo", "ár", "ér", "ír", "ando", "iendo", "ar", "er", "ir", "yendo",
    ];
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
        "iéndo" => w.replace_tail(start, "iendo"),
        "ándo" => w.replace_tail(start, "ando"),
        "ár" => w.replace_tail(start, "ar"),
        "ér" => w.replace_tail(start, "er"),
        "ír" => w.replace_tail(start, "ir"),
        "yendo" => {
            if w.before(start) == Some('u') {
                w.truncate(pronoun);
            }
        }
        _ => w.truncate(pronoun),
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
    /// `-amente`: delete in R1, then a preceding `iv` (and `at` before it), `os`, `ic` or `ad` in R2.
    Amente,
    /// `-mente`: delete in R2, then a preceding `ante`, `able` or `ible` in R2.
    Mente,
    /// `-idad`: delete in R2, then a preceding `abil`, `ic` or `iv` in R2.
    Idad,
    /// `-iva`, `-ivo`: delete in R2, then a preceding `at` in R2.
    Iv,
}

const STANDARD: &[(&str, Standard)] = &[
    ("anza", Standard::Delete),
    ("anzas", Standard::Delete),
    ("ico", Standard::Delete),
    ("ica", Standard::Delete),
    ("icos", Standard::Delete),
    ("icas", Standard::Delete),
    ("ismo", Standard::Delete),
    ("ismos", Standard::Delete),
    ("able", Standard::Delete),
    ("ables", Standard::Delete),
    ("ible", Standard::Delete),
    ("ibles", Standard::Delete),
    ("ista", Standard::Delete),
    ("istas", Standard::Delete),
    ("oso", Standard::Delete),
    ("osa", Standard::Delete),
    ("osos", Standard::Delete),
    ("osas", Standard::Delete),
    ("amiento", Standard::Delete),
    ("amientos", Standard::Delete),
    ("imiento", Standard::Delete),
    ("imientos", Standard::Delete),
    ("adora", Standard::DeleteIc),
    ("ador", Standard::DeleteIc),
    ("ación", Standard::DeleteIc),
    ("adoras", Standard::DeleteIc),
    ("adores", Standard::DeleteIc),
    ("aciones", Standard::DeleteIc),
    ("ante", Standard::DeleteIc),
    ("antes", Standard::DeleteIc),
    ("ancia", Standard::DeleteIc),
    ("ancias", Standard::DeleteIc),
    // A common misspelling of `-ación`.
    ("acion", Standard::DeleteIc),
    ("logía", Standard::Log),
    ("logías", Standard::Log),
    ("ución", Standard::U),
    ("uciones", Standard::U),
    // A common misspelling of `-ución`.
    ("ucion", Standard::U),
    ("encia", Standard::Ente),
    ("encias", Standard::Ente),
    ("amente", Standard::Amente),
    ("mente", Standard::Mente),
    ("idad", Standard::Idad),
    ("idades", Standard::Idad),
    ("iva", Standard::Iv),
    ("ivo", Standard::Iv),
    ("ivas", Standard::Iv),
    ("ivos", Standard::Iv),
];

/// Removes `suffix` if the word ends with it in R2.
fn delete_in_r2(w: &mut Word, suffix: &str, p2: usize) {
    if let Some(start) = w.ends_with(suffix).filter(|&s| s >= p2) {
        w.truncate(start);
    }
}

/// Removes the longest of `suffixes` the word ends with, if it is in R2; returns which one it removed.
fn delete_longest_in_r2<'a>(w: &mut Word, suffixes: &[&'a str], p2: usize) -> Option<&'a str> {
    let (suffix, start) = w.longest_suffix(w.len(), suffixes, 0)?;
    (start >= p2).then(|| {
        w.truncate(start);
        suffix
    })
}

/// Noun and adjective suffixes; returns whether one was removed.
fn standard_suffix(w: &mut Word, p1: usize, p2: usize) -> bool {
    let Some((rule, start)) = w.longest_rule(STANDARD, 0) else {
        return false;
    };
    let region = if matches!(rule, Standard::Amente) {
        p1
    } else {
        p2
    };
    if start < region {
        return false;
    }
    match rule {
        Standard::Delete => w.truncate(start),
        Standard::DeleteIc => {
            w.truncate(start);
            delete_in_r2(w, "ic", p2);
        }
        Standard::Log => w.replace_tail(start, "log"),
        Standard::U => w.replace_tail(start, "u"),
        Standard::Ente => w.replace_tail(start, "ente"),
        Standard::Amente => {
            w.truncate(start);
            if delete_longest_in_r2(w, &["iv", "os", "ic", "ad"], p2) == Some("iv") {
                delete_in_r2(w, "at", p2);
            }
        }
        Standard::Mente => {
            w.truncate(start);
            delete_longest_in_r2(w, &["ante", "able", "ible"], p2);
        }
        Standard::Idad => {
            w.truncate(start);
            delete_longest_in_r2(w, &["abil", "ic", "iv"], p2);
        }
        Standard::Iv => {
            w.truncate(start);
            delete_in_r2(w, "at", p2);
        }
    }
    true
}

/// Verb forms beginning with `y` in RV after a `u`; returns whether one was removed.
fn y_verb_suffix(w: &mut Word, pv: usize) -> bool {
    const SUFFIXES: &[&str] = &[
        "ya", "ye", "yan", "yen", "yeron", "yendo", "yo", "yó", "yas", "yes", "yais", "yamos",
    ];
    let Some((_, start)) = w.longest_suffix(w.len(), SUFFIXES, pv) else {
        return false;
    };
    if w.before(start) != Some('u') {
        return false;
    }
    w.truncate(start);
    true
}

#[derive(Debug, Clone, Copy)]
enum Verb {
    /// Delete, with a preceding `u` when a `g` comes before it (`-guen` → `-g`).
    AfterGu,
    Delete,
}

const VERB: &[(&str, Verb)] = &[
    ("en", Verb::AfterGu),
    ("es", Verb::AfterGu),
    ("éis", Verb::AfterGu),
    ("emos", Verb::AfterGu),
    ("arían", Verb::Delete),
    ("arías", Verb::Delete),
    ("arán", Verb::Delete),
    ("arás", Verb::Delete),
    ("aríais", Verb::Delete),
    ("aría", Verb::Delete),
    ("aréis", Verb::Delete),
    ("aríamos", Verb::Delete),
    ("aremos", Verb::Delete),
    ("ará", Verb::Delete),
    ("aré", Verb::Delete),
    ("erían", Verb::Delete),
    ("erías", Verb::Delete),
    ("erán", Verb::Delete),
    ("erás", Verb::Delete),
    ("eríais", Verb::Delete),
    ("ería", Verb::Delete),
    ("eréis", Verb::Delete),
    ("eríamos", Verb::Delete),
    ("eremos", Verb::Delete),
    ("erá", Verb::Delete),
    ("eré", Verb::Delete),
    ("irían", Verb::Delete),
    ("irías", Verb::Delete),
    ("irán", Verb::Delete),
    ("irás", Verb::Delete),
    ("iríais", Verb::Delete),
    ("iría", Verb::Delete),
    ("iréis", Verb::Delete),
    ("iríamos", Verb::Delete),
    ("iremos", Verb::Delete),
    ("irá", Verb::Delete),
    ("iré", Verb::Delete),
    ("aba", Verb::Delete),
    ("ada", Verb::Delete),
    ("ida", Verb::Delete),
    ("ía", Verb::Delete),
    ("ara", Verb::Delete),
    ("iera", Verb::Delete),
    ("ad", Verb::Delete),
    ("ed", Verb::Delete),
    ("id", Verb::Delete),
    ("ase", Verb::Delete),
    ("iese", Verb::Delete),
    ("aste", Verb::Delete),
    ("iste", Verb::Delete),
    ("an", Verb::Delete),
    ("aban", Verb::Delete),
    ("ían", Verb::Delete),
    ("aran", Verb::Delete),
    ("ieran", Verb::Delete),
    ("asen", Verb::Delete),
    ("iesen", Verb::Delete),
    ("aron", Verb::Delete),
    ("ieron", Verb::Delete),
    ("ado", Verb::Delete),
    ("ido", Verb::Delete),
    ("ando", Verb::Delete),
    ("iendo", Verb::Delete),
    ("ió", Verb::Delete),
    ("ar", Verb::Delete),
    ("er", Verb::Delete),
    ("ir", Verb::Delete),
    ("as", Verb::Delete),
    ("abas", Verb::Delete),
    ("adas", Verb::Delete),
    ("idas", Verb::Delete),
    ("ías", Verb::Delete),
    ("aras", Verb::Delete),
    ("ieras", Verb::Delete),
    ("ases", Verb::Delete),
    ("ieses", Verb::Delete),
    ("ís", Verb::Delete),
    ("áis", Verb::Delete),
    ("abais", Verb::Delete),
    ("íais", Verb::Delete),
    ("arais", Verb::Delete),
    ("ierais", Verb::Delete),
    ("aseis", Verb::Delete),
    ("ieseis", Verb::Delete),
    ("asteis", Verb::Delete),
    ("isteis", Verb::Delete),
    ("ados", Verb::Delete),
    ("idos", Verb::Delete),
    ("amos", Verb::Delete),
    ("ábamos", Verb::Delete),
    ("íamos", Verb::Delete),
    ("imos", Verb::Delete),
    ("áramos", Verb::Delete),
    ("iéramos", Verb::Delete),
    ("iésemos", Verb::Delete),
    ("ásemos", Verb::Delete),
];

/// Other verb suffixes in RV.
fn verb_suffix(w: &mut Word, pv: usize) {
    let Some((rule, start)) = w.longest_rule(VERB, pv) else {
        return;
    };
    let gu = start
        .checked_sub(1)
        .filter(|&u| w.before(start) == Some('u') && w.before(u) == Some('g'));
    match (rule, gu) {
        (Verb::AfterGu, Some(u)) => w.truncate(u),
        _ => w.truncate(start),
    }
}

/// A final vowel or `-os` in RV; after `-e`, also a `u` in RV after a `g`.
fn residual_suffix(w: &mut Word, pv: usize) {
    let Some((suffix, start)) =
        w.longest_suffix(w.len(), &["os", "a", "o", "á", "í", "ó", "e", "é"], 0)
    else {
        return;
    };
    if start < pv {
        return;
    }
    w.truncate(start);
    if matches!(suffix, "e" | "é")
        && let Some(u) = w
            .ends_with("u")
            .filter(|&u| u >= pv && w.before(u) == Some('g'))
    {
        w.truncate(u);
    }
}
