//! The Snowball Portuguese stemmer, as `algorithms/portuguese.sbl` of Snowball v3.1.1.

use std::borrow::Cow;

use super::Stemmer;
use super::word::{Word, finish, r1_r2, rv};

/// The Snowball Portuguese stemmer (SPEC §6.3.2): `canções` → `cançõ`, `felizmente` → `feliz`,
/// `brasileiras` → `brasileir`.
///
/// Expects lowercase text with its accents (run it after [`LowercaseFilter`](crate::LowercaseFilter) and before
/// [`FoldingFilter`](crate::FoldingFilter)); a final `ç` becomes `c`, and other accents are kept. It writes `ã` and
/// `õ` as `a~` and `o~` while stemming, as Snowball does, so a literal `a~` in the input comes out as `ã`.
///
/// ```
/// use persia_analysis::{PortugueseStemmer, Stemmer};
///
/// let stems: Vec<_> = ["canções", "felizmente", "brasileiras", "falávamos"]
///     .iter()
///     .map(|w| PortugueseStemmer.stem(w).into_owned())
///     .collect();
/// assert_eq!(stems, ["cançõ", "feliz", "brasileir", "fal"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortugueseStemmer;

impl Stemmer for PortugueseStemmer {
    fn stem<'a>(&self, word: &'a str) -> Cow<'a, str> {
        // `ã` and `õ` are written as a vowel and `~`, a non-vowel, while stemming.
        let mut chars = Vec::with_capacity(word.len());
        for c in word.chars() {
            match c {
                'ã' => chars.extend(['a', '~']),
                'õ' => chars.extend(['o', '~']),
                other => chars.push(other),
            }
        }
        let mut w = Word::from_chars(chars);
        let pv = rv(w.chars(), is_vowel);
        let (p1, p2) = r1_r2(w.chars(), is_vowel);
        if standard_suffix(&mut w, p1, p2, pv) || verb_suffix(&mut w, pv) {
            if let Some(i) = w
                .ends_with("i")
                .filter(|&i| i >= pv && w.before(i) == Some('c'))
            {
                w.truncate(i);
            }
        } else {
            residual_suffix(&mut w, pv);
        }
        residual_form(&mut w, pv);
        let mut out = Vec::with_capacity(w.len());
        for &c in w.chars() {
            match (out.last(), c) {
                (Some('a'), '~') => {
                    out.pop();
                    out.push('ã');
                }
                (Some('o'), '~') => {
                    out.pop();
                    out.push('õ');
                }
                _ => out.push(c),
            }
        }
        finish(word, &out)
    }
}

fn is_vowel(c: char) -> bool {
    matches!(
        c,
        'a' | 'e' | 'i' | 'o' | 'u' | 'á' | 'é' | 'í' | 'ó' | 'ú' | 'â' | 'ê' | 'ô'
    )
}

#[derive(Debug, Clone, Copy)]
enum Standard {
    /// Delete in R2.
    Delete,
    /// Replace with `log` in R2.
    Log,
    /// Replace with `u` in R2.
    U,
    /// Replace with `ente` in R2.
    Ente,
    /// `-amente`: delete in R1, then a preceding `iv` (and `at` before it), `os`, `ic` or `ad` in R2.
    Amente,
    /// `-mente`: delete in R2, then a preceding `ante`, `avel` or `ível` in R2.
    Mente,
    /// `-idade`: delete in R2, then a preceding `abil`, `ic` or `iv` in R2.
    Idade,
    /// `-iva`, `-ivo`: delete in R2, then a preceding `at` in R2.
    Iv,
    /// `-eira`: replace `-ira` with `ir` in RV.
    Ira,
}

const STANDARD: &[(&str, Standard)] = &[
    ("eza", Standard::Delete),
    ("ezas", Standard::Delete),
    ("ico", Standard::Delete),
    ("ica", Standard::Delete),
    ("icos", Standard::Delete),
    ("icas", Standard::Delete),
    ("ismo", Standard::Delete),
    ("ismos", Standard::Delete),
    ("ável", Standard::Delete),
    ("ível", Standard::Delete),
    ("ista", Standard::Delete),
    ("istas", Standard::Delete),
    ("oso", Standard::Delete),
    ("osa", Standard::Delete),
    ("osos", Standard::Delete),
    ("osas", Standard::Delete),
    ("amento", Standard::Delete),
    ("amentos", Standard::Delete),
    ("imento", Standard::Delete),
    ("imentos", Standard::Delete),
    ("adora", Standard::Delete),
    ("ador", Standard::Delete),
    ("aça~o", Standard::Delete),
    ("adoras", Standard::Delete),
    ("adores", Standard::Delete),
    ("aço~es", Standard::Delete),
    ("ante", Standard::Delete),
    ("antes", Standard::Delete),
    ("ância", Standard::Delete),
    ("logia", Standard::Log),
    ("logias", Standard::Log),
    ("uça~o", Standard::U),
    ("uço~es", Standard::U),
    ("ência", Standard::Ente),
    ("ências", Standard::Ente),
    ("amente", Standard::Amente),
    ("mente", Standard::Mente),
    ("idade", Standard::Idade),
    ("idades", Standard::Idade),
    ("iva", Standard::Iv),
    ("ivo", Standard::Iv),
    ("ivas", Standard::Iv),
    ("ivos", Standard::Iv),
    ("ira", Standard::Ira),
    ("iras", Standard::Ira),
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

/// Noun and adjective suffixes; returns whether one was removed or replaced.
fn standard_suffix(w: &mut Word, p1: usize, p2: usize, pv: usize) -> bool {
    let Some((rule, start)) = w.longest_rule(STANDARD, 0) else {
        return false;
    };
    let in_region = match rule {
        Standard::Amente => start >= p1,
        Standard::Ira => start >= pv && w.before(start) == Some('e'),
        _ => start >= p2,
    };
    if !in_region {
        return false;
    }
    match rule {
        Standard::Delete => w.truncate(start),
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
            delete_longest_in_r2(w, &["ante", "avel", "ível"], p2);
        }
        Standard::Idade => {
            w.truncate(start);
            delete_longest_in_r2(w, &["abil", "ic", "iv"], p2);
        }
        Standard::Iv => {
            w.truncate(start);
            delete_in_r2(w, "at", p2);
        }
        Standard::Ira => w.replace_tail(start, "ir"),
    }
    true
}

/// Verb suffixes in RV; returns whether one was removed.
fn verb_suffix(w: &mut Word, pv: usize) -> bool {
    const SUFFIXES: &[&str] = &[
        "ada", "ida", "ia", "aria", "eria", "iria", "ará", "ara", "erá", "era", "irá", "ava",
        "asse", "esse", "isse", "aste", "este", "iste", "ei", "arei", "erei", "irei", "am", "iam",
        "ariam", "eriam", "iriam", "aram", "eram", "iram", "avam", "em", "arem", "erem", "irem",
        "assem", "essem", "issem", "ado", "ido", "ando", "endo", "indo", "ara~o", "era~o", "ira~o",
        "ar", "er", "ir", "as", "adas", "idas", "ias", "arias", "erias", "irias", "arás", "aras",
        "erás", "eras", "irás", "avas", "es", "ardes", "erdes", "irdes", "ares", "eres", "ires",
        "asses", "esses", "isses", "astes", "estes", "istes", "is", "ais", "eis", "íeis", "aríeis",
        "eríeis", "iríeis", "áreis", "areis", "éreis", "ereis", "íreis", "ireis", "ásseis",
        "ésseis", "ísseis", "áveis", "ados", "idos", "ámos", "amos", "íamos", "aríamos", "eríamos",
        "iríamos", "áramos", "éramos", "íramos", "ávamos", "emos", "aremos", "eremos", "iremos",
        "ássemos", "êssemos", "íssemos", "imos", "armos", "ermos", "irmos", "eu", "iu", "ou",
        "ira", "iras",
    ];
    let Some((_, start)) = w.longest_suffix(w.len(), SUFFIXES, pv) else {
        return false;
    };
    w.truncate(start);
    true
}

/// A final `-os`, `a`, `i`, `o`, `á`, `í` or `ó` in RV.
fn residual_suffix(w: &mut Word, pv: usize) {
    if let Some((_, start)) = w.longest_suffix(w.len(), &["os", "a", "i", "o", "á", "í", "ó"], 0)
        && start >= pv
    {
        w.truncate(start);
    }
}

/// A final `e`, `é` or `ê` in RV, then a `u` after `g` or an `i` after `c` in RV; a final `ç` becomes `c`.
fn residual_form(w: &mut Word, pv: usize) {
    match w.longest_suffix(w.len(), &["e", "é", "ê", "ç"], 0) {
        Some(("ç", start)) => w.replace_tail(start, "c"),
        Some((_, start)) if start >= pv => {
            w.truncate(start);
            let after = |suffix: &str, before: char| {
                w.ends_with(suffix).filter(|&s| w.before(s) == Some(before))
            };
            if let Some(s) = after("u", 'g').or_else(|| after("i", 'c'))
                && s >= pv
            {
                w.truncate(s);
            }
        }
        _ => {}
    }
}
