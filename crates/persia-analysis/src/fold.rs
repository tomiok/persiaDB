//! Accent and diacritic folding (SPEC §6.3.2).

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::decompose_canonical;

use crate::token::{Token, TokenFilter};

/// Removes accents and diacritics from Latin, Greek and Cyrillic letters (SPEC §6.3.2): `é` → `e`, `ü` → `u`,
/// `ç` → `c`, `ß` → `ss`, `ø` → `o`.
///
/// Each character is decomposed canonically and loses its combining marks U+0300–U+036F; letters without a
/// decomposition fold by a fixed table, also inside a decomposition. The folded text is recomposed (NFC), so the marks of
/// other scripts and Hangul syllables are unchanged. Characters of the preserved set are kept as listed (case
/// matters), also inside a decomposition, and are not recomposed, so an analyzer can keep `año` apart from `ano`:
///
/// ```
/// use persia_analysis::{FoldingFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = Vec::new();
/// WhitespaceTokenizer.tokenize("pingüino año Straße", &mut tokens);
/// let mut preserving = tokens.clone();
/// FoldingFilter::default().apply(&mut tokens);
/// FoldingFilter::preserving(['ñ', 'Ñ']).apply(&mut preserving);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["pinguino", "ano", "Strasse"]);
/// assert_eq!(preserving[1].text, "año");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoldingFilter {
    /// Sorted and deduplicated.
    preserved: Vec<char>,
}

impl FoldingFilter {
    /// A filter that folds every character except those of `preserved`.
    pub fn preserving(preserved: impl IntoIterator<Item = char>) -> Self {
        let mut preserved: Vec<char> = preserved.into_iter().collect();
        preserved.sort_unstable();
        preserved.dedup();
        Self { preserved }
    }

    /// The preserved characters, sorted.
    pub fn preserved(&self) -> &[char] {
        &self.preserved
    }

    fn is_preserved(&self, c: char) -> bool {
        self.preserved.binary_search(&c).is_ok()
    }

    /// Folds `text`, or returns `None` when it is unchanged.
    fn fold(&self, text: &str) -> Option<String> {
        // ASCII has no decomposition and no folding table entry.
        if text.is_ascii() {
            return None;
        }
        let mut folded = String::with_capacity(text.len());
        // The folded run since the last preserved character, recomposed when one comes or at the end. Preserved
        // characters bypass recomposition, which could rewrite them (U+0344 is not NFC) and break idempotence.
        let mut run = String::new();
        let keep = |c: char, folded: &mut String, run: &mut String| {
            folded.extend(run.nfc());
            run.clear();
            folded.push(c);
        };
        for c in text.chars() {
            if self.is_preserved(c) {
                keep(c, &mut folded, &mut run);
                continue;
            }
            // The table and the preserved set apply to the parts too: `Ǣ` is `Æ` and a macron.
            decompose_canonical(c, |part| {
                if self.is_preserved(part) {
                    keep(part, &mut folded, &mut run);
                } else if let Some(letter) = fold_letter(part) {
                    run.push_str(letter);
                } else if !is_folded_mark(part) {
                    run.push(part);
                }
            });
        }
        folded.extend(run.nfc());
        (folded != text).then_some(folded)
    }
}

impl TokenFilter for FoldingFilter {
    fn apply(&self, tokens: &mut Vec<Token>) {
        tokens.retain_mut(|token| {
            if let Some(folded) = self.fold(&token.text) {
                token.text = folded;
            }
            // A token made only of combining marks folds to nothing.
            !token.text.is_empty()
        });
    }
}

/// Combining Diacritical Marks: the accents of Latin, Greek and Cyrillic letters.
fn is_folded_mark(c: char) -> bool {
    matches!(c, '\u{300}'..='\u{36f}')
}

/// Letters with no canonical decomposition that still carry a diacritic or are ligatures (SPEC §6.3.2).
fn fold_letter(c: char) -> Option<&'static str> {
    Some(match c {
        'ß' | 'ẞ' => "ss",
        'æ' => "ae",
        'Æ' => "AE",
        'œ' => "oe",
        'Œ' => "OE",
        'ø' => "o",
        'Ø' => "O",
        'đ' | 'ð' => "d",
        'Đ' | 'Ð' => "D",
        'ł' => "l",
        'Ł' => "L",
        'þ' => "th",
        'Þ' => "TH",
        'ħ' => "h",
        'Ħ' => "H",
        'ı' => "i",
        _ => return None,
    })
}
