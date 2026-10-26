//! Accent and diacritic folding (SPEC §6.3.2).

use unicode_normalization::char::decompose_canonical;
use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};

use crate::token::TokenFilter;
use crate::token_stream::TokenStream;

/// Removes accents and diacritics from Latin, Greek and Cyrillic letters (SPEC §6.3.2): `é` → `e`, `ü` → `u`,
/// `ç` → `c`, `ß` → `ss`, `ø` → `o`.
///
/// Each character is decomposed canonically and loses its combining marks U+0300–U+036F; letters without a
/// decomposition fold by a fixed table, also inside a decomposition. The folded text is recomposed (NFC), so the marks of
/// other scripts and Hangul syllables are unchanged. Characters of the preserved set are kept as listed (case
/// matters), also inside a decomposition, and are not recomposed, so an analyzer can keep `año` apart from `ano`:
///
/// ```
/// use persia_analysis::{TokenStream, FoldingFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = TokenStream::new();
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

    /// Writes the folded `text` to the empty `out` and returns `true`, or returns `false` when folding leaves
    /// `text` unchanged.
    fn fold_into(&self, text: &str, out: &mut String) -> bool {
        // ASCII has no decomposition and no folding table entry.
        if text.is_ascii() || self.folds_to_itself(text) {
            return false;
        }
        self.fold_all_into(text, out)
    }

    /// Whether folding provably leaves `text` unchanged, found without allocating (ROADMAP 4.4.9): `text` is NFC
    /// and no character outside the preserved set decomposes into a preserved character, a table letter or a folded
    /// mark. Then each run between preserved characters is recomposed from its own canonical decomposition, and a
    /// substring of an NFC text is NFC (its pairs and their order are the text's), so every run comes back as is.
    /// A `Maybe` from the quick check (some combining marks) takes the full path.
    fn folds_to_itself(&self, text: &str) -> bool {
        is_nfc_quick(text.chars()) == IsNormalized::Yes
            && text.chars().all(|c| {
                if c.is_ascii() || self.is_preserved(c) {
                    return true;
                }
                let mut inert = true;
                decompose_canonical(c, |part| {
                    inert &= !(self.is_preserved(part)
                        || fold_letter(part).is_some()
                        || is_folded_mark(part));
                });
                inert
            })
    }

    /// [`Self::fold_all_into`] into a new string, or `None` when unchanged.
    #[cfg(test)]
    fn fold_all(&self, text: &str) -> Option<String> {
        let mut folded = String::new();
        self.fold_all_into(text, &mut folded).then_some(folded)
    }

    /// [`Self::fold_into`] without the pre-scan.
    fn fold_all_into(&self, text: &str, folded: &mut String) -> bool {
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
                keep(c, folded, &mut run);
                continue;
            }
            // The table and the preserved set apply to the parts too: `Ǣ` is `Æ` and a macron.
            decompose_canonical(c, |part| {
                if self.is_preserved(part) {
                    keep(part, folded, &mut run);
                } else if let Some(letter) = fold_letter(part) {
                    run.push_str(letter);
                } else if !is_folded_mark(part) {
                    run.push(part);
                }
            });
        }
        folded.extend(run.nfc());
        folded != text
    }
}

impl TokenFilter for FoldingFilter {
    fn apply(&self, tokens: &mut TokenStream) {
        // A token made only of combining marks folds to nothing, and `rewrite` removes it.
        tokens.rewrite(|text, out| self.fold_into(text, out));
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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn filters() -> [FoldingFilter; 3] {
        [
            FoldingFilter::default(),
            FoldingFilter::preserving(['ñ']),
            FoldingFilter::preserving(['ñ', 'Ö', 'ü', 'ł', '\u{344}', 'か']),
        ]
    }

    #[test]
    fn the_pre_scan_takes_the_fast_path_only_when_nothing_changes() {
        let es = FoldingFilter::preserving(['ñ']);
        for text in ["日本語", "привет", "año", "가나다", "ελληνικη", "ŧƀɨ"]
        {
            assert!(es.folds_to_itself(text), "{text}");
            assert_eq!(es.fold_all(text), None, "{text}");
        }
        // Accents, table letters, decompositions with marks, preserved parts outside the set, non-NFC text.
        for text in ["é", "ß", "ǖ", "Ñ", "n\u{303}", "\u{1100}\u{1161}", "क़", "Ǣ"] {
            assert!(!es.folds_to_itself(text), "{text}");
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]

        /// The pre-scan never changes the result: whenever it skips folding, folding changes nothing.
        #[test]
        fn the_pre_scan_agrees_with_full_folding(
            text in prop_oneof![
                "[a-zñÑöÖüÜłŁæßéáà\u{300}-\u{36f}\u{344}\u{1100}-\u{1112}\u{1161}-\u{1175}가-갛क़कं़\u{93c}日本ăΣάかが\u{3099} ]{0,16}",
                any::<String>(),
            ],
        ) {
            for filter in filters() {
                if filter.folds_to_itself(&text) {
                    prop_assert_eq!(filter.fold_all(&text), None, "{:?}", filter.preserved());
                }
            }
        }
    }

    /// Every scalar value (the BMP only without `slow`: minutes in debug otherwise) alone and after a few starters:
    /// the pre-scan agrees with full folding.
    #[test]
    fn the_pre_scan_agrees_on_every_scalar_value() {
        let last = if cfg!(feature = "slow") {
            char::MAX
        } else {
            '\u{ffff}'
        };
        let mut text = String::new();
        for c in (0..=u32::from(last)).filter_map(char::from_u32) {
            for before in ["", "a", "ñ", "\u{1100}"] {
                text.clear();
                text.push_str(before);
                text.push(c);
                for filter in filters() {
                    if filter.folds_to_itself(&text) {
                        assert_eq!(
                            filter.fold_all(&text),
                            None,
                            "{text:?} {:?}",
                            filter.preserved()
                        );
                    }
                }
            }
        }
    }
}
