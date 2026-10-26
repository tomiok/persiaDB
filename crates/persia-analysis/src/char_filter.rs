//! Char filters and the offset map that sends token spans back to the original field value (SPEC §6.3, §6.3.1).

use std::fmt;

/// Rewrites the text of a field value before it is tokenized (SPEC §6.3).
///
/// A filter consumes its input from the start, piece by piece, through a [`CharFilterOutput`]: each piece is kept
/// as is or replaced by other text. The output records which input bytes every replacement came from, so the
/// analyzer maps token spans back to the original value (SPEC §6.3.1). Input the filter does not consume is kept.
/// The map holds one entry per replacement: a filter that replaces every character on its own builds a map as long
/// as the text, and one that replaces a run in one call gives every token inside the run the run's whole span.
/// Object safe, so an analyzer can chain any char filters behind pointers.
///
/// ```
/// use persia_analysis::{TokenStream, Analyzer, CharFilter, CharFilterOutput, WhitespaceTokenizer};
///
/// /// Spells out `&` as ` and `.
/// struct Ampersand;
///
/// impl CharFilter for Ampersand {
///     fn filter(&self, out: &mut CharFilterOutput<'_>) {
///         while let Some(at) = out.remaining().find('&') {
///             out.keep(at);
///             out.replace(1, " and ");
///         }
///     }
/// }
///
/// let analyzer = Analyzer::builder(WhitespaceTokenizer).char_filter(Ampersand).build();
/// let mut tokens = TokenStream::new();
/// analyzer.analyze("salt&pepper", &mut tokens);
/// let spans: Vec<(&str, usize, usize)> = tokens.iter().map(|t| (t.text.as_str(), t.start, t.end)).collect();
/// // `and` comes from the `&` at byte 4: its span is that byte of the original value.
/// assert_eq!(spans, [("salt", 0, 4), ("and", 4, 5), ("pepper", 5, 11)]);
/// ```
pub trait CharFilter: Send + Sync {
    /// Consumes the input of `out` (or a prefix of it: the rest is kept), writing the filtered text. Never panics.
    fn filter(&self, out: &mut CharFilterOutput<'_>);
}

/// The output of one [`CharFilter`] over one input: the filtered text, built by consuming the input from its start,
/// and the offset map from the filtered text back to the input.
///
/// Lengths are in bytes of the input. A length that ends inside a character is extended to the end of that
/// character, and one that runs past the input is cut at its end, so every piece is whole characters and the filter
/// never panics.
pub struct CharFilterOutput<'a> {
    input: &'a str,
    /// Bytes of `input` consumed so far.
    consumed: usize,
    /// The filtered text; filled only from the first replacement on, as until then it equals the consumed input.
    text: String,
    map: OffsetMap,
}

impl<'a> CharFilterOutput<'a> {
    pub(crate) fn new(input: &'a str) -> Self {
        Self {
            input,
            consumed: 0,
            text: String::new(),
            map: OffsetMap::default(),
        }
    }

    /// The whole input of the filter.
    pub fn input(&self) -> &'a str {
        self.input
    }

    /// The input not consumed yet.
    pub fn remaining(&self) -> &'a str {
        self.input.get(self.consumed..).unwrap_or_default()
    }

    /// Keeps the next `len` bytes of the input unchanged.
    pub fn keep(&mut self, len: usize) {
        let end = self.piece_end(len);
        if self.changed() {
            self.text
                .push_str(self.input.get(self.consumed..end).unwrap_or_default());
        }
        self.consumed = end;
    }

    /// Replaces the next `len` bytes of the input with `replacement`. A `len` of 0 inserts `replacement`; an empty
    /// `replacement` deletes the bytes.
    pub fn replace(&mut self, len: usize, replacement: &str) {
        let end = self.piece_end(len);
        let original = self.input.get(self.consumed..end).unwrap_or_default();
        if original == replacement {
            self.keep(len);
            return;
        }
        if !self.changed() {
            self.text
                .push_str(self.input.get(..self.consumed).unwrap_or_default());
        }
        let filtered_start = self.text.len();
        self.text.push_str(replacement);
        self.map.replacements.push(Replacement {
            filtered_start,
            filtered_end: self.text.len(),
            original_start: self.consumed,
            original_end: end,
        });
        self.consumed = end;
    }

    /// Whether a replacement was made.
    fn changed(&self) -> bool {
        !self.map.replacements.is_empty()
    }

    /// The input offset `len` bytes on, at a character boundary and within the input.
    fn piece_end(&self, len: usize) -> usize {
        let end = self.consumed.saturating_add(len).min(self.input.len());
        self.input.ceil_char_boundary(end)
    }

    /// The filtered text and its offset map, or `None` if the filter left the input unchanged.
    pub(crate) fn finish(mut self) -> Option<(String, OffsetMap)> {
        if !self.changed() {
            return None;
        }
        let rest = self.input.len().saturating_sub(self.consumed);
        self.keep(rest);
        Some((self.text, self.map))
    }
}

impl fmt::Debug for CharFilterOutput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CharFilterOutput")
            .field("consumed", &self.consumed)
            .field("input_len", &self.input.len())
            .field("replacements", &self.map.replacements.len())
            .finish_non_exhaustive()
    }
}

/// One replaced piece: `filtered_start..filtered_end` of the filtered text came from
/// `original_start..original_end` of the filter's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Replacement {
    filtered_start: usize,
    filtered_end: usize,
    original_start: usize,
    original_end: usize,
}

/// Maps offsets of a char filter's output to offsets of its input (SPEC §6.3.1).
///
/// Only the replaced pieces are recorded, in order; the text between them is the input itself, shifted by the
/// length difference of the replacements before it. An offset inside a replacement maps to the start of the
/// replaced input for a span start and to its end for a span end, so a span covers every input character its text
/// came from; at a deletion, a start maps past the deleted bytes and an end before them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OffsetMap {
    replacements: Vec<Replacement>,
}

impl OffsetMap {
    /// The input offset where a span starting at filtered offset `offset` starts.
    pub(crate) fn start(&self, offset: usize) -> usize {
        let index = self
            .replacements
            .partition_point(|r| r.filtered_start <= offset);
        self.map(
            index,
            offset,
            |r| offset < r.filtered_end,
            |r| r.original_start,
        )
    }

    /// The input offset where a span ending at filtered offset `offset` ends.
    pub(crate) fn end(&self, offset: usize) -> usize {
        let index = self
            .replacements
            .partition_point(|r| r.filtered_start < offset);
        self.map(
            index,
            offset,
            |r| offset <= r.filtered_end,
            |r| r.original_end,
        )
    }

    /// `offset` mapped through the replacement before `index`: to `inside(r)` when `within(r)`, otherwise shifted
    /// from the end of `r` (or unchanged before the first replacement).
    fn map(
        &self,
        index: usize,
        offset: usize,
        within: impl Fn(&Replacement) -> bool,
        inside: impl Fn(&Replacement) -> usize,
    ) -> usize {
        let Some(r) = index.checked_sub(1).and_then(|i| self.replacements.get(i)) else {
            return offset;
        };
        if within(r) {
            inside(r)
        } else {
            r.original_end
                .saturating_add(offset.saturating_sub(r.filtered_end))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &str, f: impl FnOnce(&mut CharFilterOutput<'_>)) -> Option<(String, OffsetMap)> {
        let mut out = CharFilterOutput::new(input);
        f(&mut out);
        out.finish()
    }

    #[test]
    fn untouched_input_is_unchanged() {
        assert_eq!(run("abc", |_| {}), None);
        assert_eq!(run("abc", |out| out.keep(2)), None);
        // Replacing a piece with itself is keeping it.
        assert_eq!(run("abc", |out| out.replace(1, "a")), None);
    }

    #[test]
    fn replacements_build_the_text_and_the_rest_is_kept() {
        let (text, map) = run("a-b-c", |out| {
            out.keep(1);
            out.replace(1, " minus ");
            out.keep(1);
            out.replace(1, "");
        })
        .unwrap();
        assert_eq!(text, "a minus bc");
        assert_eq!(map.replacements.len(), 2);
    }

    #[test]
    fn lengths_round_to_whole_characters_and_stop_at_the_end() {
        // `é` is two bytes: a length of 1 takes all of it.
        let (text, map) = run("éa", |out| out.replace(1, "e")).unwrap();
        assert_eq!(text, "ea");
        assert_eq!(map.start(0), 0);
        assert_eq!(map.end(1), 2);
        assert_eq!(map.start(1), 2);
        let (text, _) = run("ab", |out| {
            out.keep(usize::MAX);
            out.replace(5, "x");
        })
        .unwrap();
        // Nothing is left to replace: the replacement is an insertion at the end.
        assert_eq!(text, "abx");
    }

    #[test]
    fn offsets_map_through_replacements() {
        // "a&b" -> "a and b": `&` at 1..2 became " and " at 1..6.
        let (_, map) = run("a&b", |out| {
            out.keep(1);
            out.replace(1, " and ");
        })
        .unwrap();
        // Before the replacement: unchanged.
        assert_eq!((map.start(0), map.end(1)), (0, 1));
        // `and` at 2..5 lies inside it: the whole `&`.
        assert_eq!((map.start(2), map.end(5)), (1, 2));
        // At its edges.
        assert_eq!((map.start(1), map.end(6)), (1, 2));
        // After it: shifted back by the 4 added bytes.
        assert_eq!((map.start(6), map.end(7)), (2, 3));
    }

    #[test]
    fn deletions_tighten_spans() {
        // "ab--cd" -> "abcd": the start of `cd` maps past the deletion, the end of `ab` before it.
        let (_, map) = run("ab--cd", |out| {
            out.keep(2);
            out.replace(2, "");
        })
        .unwrap();
        assert_eq!((map.start(0), map.end(2)), (0, 2));
        assert_eq!((map.start(2), map.end(4)), (4, 6));
    }

    #[test]
    fn insertions_map_to_their_position() {
        let (text, map) = run("ab", |out| {
            out.keep(1);
            out.replace(0, "XY");
        })
        .unwrap();
        assert_eq!(text, "aXYb");
        // Inserted text has no original characters: a span inside it is empty, at the insertion point.
        assert_eq!((map.start(1), map.end(3)), (1, 1));
        assert_eq!((map.start(0), map.end(4)), (0, 2));
    }
}
