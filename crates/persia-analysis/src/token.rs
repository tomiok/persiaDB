//! The token model shared by tokenizers and token filters (SPEC §6.3.1).

use crate::token_stream::TokenStream;

/// One token of analyzed text.
///
/// `start..end` is the token's byte span in the text given to the tokenizer: both ends are UTF-8 character
/// boundaries, so it can be used to highlight the source (an [`Analyzer`](crate::Analyzer) maps spans back to the
/// value given to it, before its char filters, SPEC §6.3.1). A tokenizer sets `text` to exactly that span; token
/// filters may later rewrite `text` (lowercase it, truncate it) but keep the span, so it points at the original
/// characters. Through char filters the text need not match its span (a replacement's text spans the replaced
/// characters), and a token made only of inserted text has an empty span at the insertion point. `position` counts tokens from 0 in one call to [`Tokenizer::tokenize`]; phrase and
/// proximity queries compare positions, so a filter that removes a token leaves a gap rather than renumbering.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Token {
    /// The token's text.
    pub text: String,
    /// Byte offset of the first byte of the token in the source text.
    pub start: usize,
    /// Byte offset one past the last byte of the token in the source text.
    pub end: usize,
    /// The token's position in the token stream, from 0.
    pub position: u32,
}

/// Splits text into tokens (SPEC §6.3.1).
///
/// Object safe, so analyzers can hold any tokenizer behind a pointer; tokens are appended to a caller-owned
/// [`TokenStream`] so its buffers are reused across fields and documents.
pub trait Tokenizer: Send + Sync {
    /// Appends the tokens of `text` to `out`, in order, with positions numbered from 0 for this call.
    ///
    /// Tokens of one call have strictly increasing `start` offsets and do not overlap. A tokenizer never panics:
    /// a text with more than `u32::MAX` tokens (impossible within the document size limit, SPEC §6.4) is cut at
    /// the last position that fits.
    fn tokenize(&self, text: &str, out: &mut TokenStream);
}

/// Appends the tokens produced by `spans` (byte offset, token text) to `out`, numbering positions from 0 and
/// stopping at the first position that does not fit a `u32`.
pub(crate) fn push_spans<'a>(spans: impl Iterator<Item = (usize, &'a str)>, out: &mut TokenStream) {
    for ((start, text), position) in spans.zip(0..=u32::MAX) {
        let Some(end) = start.checked_add(text.len()) else {
            return;
        };
        out.push(text, start, end, position);
    }
}

/// Rewrites, removes or adds tokens in place (SPEC §6.3.2).
///
/// A filter never changes a token's span; a token whose text it empties is removed, and removing a token leaves a
/// gap in positions. Object safe, so an analyzer can chain any filters behind pointers. Filters that remove tokens
/// through [`TokenStream::retain_mut`] and rewrite texts in place or through [`TokenStream::rewrite`] reuse the
/// stream's buffers, so they allocate nothing for it once it has grown (ROADMAP 4.4.4).
pub trait TokenFilter: Send + Sync {
    /// Applies the filter to `tokens`, the output of a tokenizer or of the previous filter.
    fn apply(&self, tokens: &mut TokenStream);

    /// Whether the filter adds tokens stacked on the position of the token they came from (edge n-grams). An
    /// [`Analyzer`](crate::Analyzer) leaves such filters out of query-time analysis (SPEC §6.3.4).
    fn stacks_tokens(&self) -> bool {
        false
    }
}
