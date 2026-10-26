//! Elision filter (SPEC §6.3.2).

use crate::token::TokenFilter;
use crate::token_stream::TokenStream;

/// French elided articles, pronouns, prepositions and conjunctions, without their apostrophe: the `french.sbl`
/// prefixes and the conjunctions ending in `qu` (`lorsqu'il`).
pub const FRENCH_ELISIONS: &[&str] = &[
    "c", "d", "j", "jusqu", "l", "lorsqu", "m", "n", "puisqu", "qu", "quelqu", "quoiqu", "s", "t",
];

/// Italian elided articles, pronouns and prepositions, without their apostrophe: the `italian.sbl` prefixes.
pub const ITALIAN_ELISIONS: &[&str] = &[
    "all", "d", "dall", "dell", "gl", "l", "m", "nell", "quell", "quest", "s", "sull", "t", "tutt",
    "un", "v",
];

/// Removes a leading elided word and its apostrophe (U+0027) from a token, when something follows it (SPEC
/// §6.3.2): `l'été` → `été`, `qu'il` → `il`.
///
/// The token's span and position are kept. It runs after [`LowercaseFilter`](crate::LowercaseFilter) and
/// [`ApostropheFilter`](crate::ApostropheFilter), so `L’été` is matched too, and before
/// [`StopwordFilter`](crate::StopwordFilter), so `c'est` is the stopword `est`.
///
/// ```
/// use persia_analysis::{ElisionFilter, TokenFilter, TokenStream, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = TokenStream::new();
/// WhitespaceTokenizer.tokenize("l'été qu'il l' aujourd'hui", &mut tokens);
/// ElisionFilter::french().apply(&mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["été", "il", "l'", "aujourd'hui"]);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElisionFilter {
    /// Sorted, without apostrophes.
    prefixes: Vec<Box<str>>,
}

impl ElisionFilter {
    /// A filter removing the given elided words (given without their apostrophe).
    pub fn new<W: Into<Box<str>>>(prefixes: impl IntoIterator<Item = W>) -> Self {
        let mut prefixes: Vec<Box<str>> = prefixes.into_iter().map(Into::into).collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        Self { prefixes }
    }

    /// The French elisions, [`FRENCH_ELISIONS`].
    pub fn french() -> Self {
        Self::new(FRENCH_ELISIONS.iter().copied())
    }

    /// The Italian elisions, [`ITALIAN_ELISIONS`].
    pub fn italian() -> Self {
        Self::new(ITALIAN_ELISIONS.iter().copied())
    }

    /// The length in bytes of the elided prefix of `text`, apostrophe included, if it has one.
    fn elided_len(&self, text: &str) -> Option<usize> {
        let (word, rest) = text.split_once('\'')?;
        if rest.is_empty() || self.prefixes.binary_search_by(|p| (**p).cmp(word)).is_err() {
            return None;
        }
        word.len().checked_add(1)
    }
}

impl TokenFilter for ElisionFilter {
    fn apply(&self, tokens: &mut TokenStream) {
        tokens.rewrite(|text, out| match self.elided_len(text) {
            Some(len) => {
                out.push_str(text.get(len..).unwrap_or_default());
                true
            }
            None => false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_are_sorted_unique_and_without_apostrophes() {
        for list in [FRENCH_ELISIONS, ITALIAN_ELISIONS] {
            assert!(list.windows(2).all(|w| w[0] < w[1]), "{list:?}");
            assert!(list.iter().all(|w| !w.is_empty() && !w.contains('\'')));
        }
        // Part of the `fr` and `it` analyzer definitions (SPEC §6.3.2): editing a list needs new versions.
        assert_eq!(
            FRENCH_ELISIONS.join(","),
            "c,d,j,jusqu,l,lorsqu,m,n,puisqu,qu,quelqu,quoiqu,s,t"
        );
        assert_eq!(
            ITALIAN_ELISIONS.join(","),
            "all,d,dall,dell,gl,l,m,nell,quell,quest,s,sull,t,tutt,un,v"
        );
    }

    #[test]
    fn only_a_listed_prefix_with_something_after_it_goes() {
        let fr = ElisionFilter::french();
        for (text, want) in [
            ("l'été", Some(2)),
            ("lorsqu'il", Some(7)),
            ("l'", None),
            ("x'y", None),
            ("aujourd'hui", None),
            ("l'l'x", Some(2)),
            ("été", None),
        ] {
            assert_eq!(fr.elided_len(text), want, "{text}");
        }
    }
}
