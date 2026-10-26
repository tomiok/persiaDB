//! Whitespace and keyword tokenizers (SPEC §6.3.1).

use crate::token::{Tokenizer, push_spans};
use crate::token_stream::TokenStream;

/// Splits text at Unicode white space (`char::is_whitespace`) and keeps everything else as is, punctuation
/// included: the tokenizer of the `whitespace` analyzer (SPEC §6.3).
///
/// ```
/// use persia_analysis::{TokenStream, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = TokenStream::new();
/// WhitespaceTokenizer.tokenize(" C++\tand  Go! ", &mut tokens);
/// let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
/// assert_eq!(texts, ["C++", "and", "Go!"]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WhitespaceTokenizer;

impl Tokenizer for WhitespaceTokenizer {
    fn tokenize(&self, text: &str, out: &mut TokenStream) {
        push_spans(WhitespaceSpans { text, offset: 0 }, out);
    }
}

/// Words between runs of white space, with their byte offsets.
struct WhitespaceSpans<'a> {
    text: &'a str,
    /// Byte offset of `text` in the original text.
    offset: usize,
}

impl<'a> Iterator for WhitespaceSpans<'a> {
    type Item = (usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        let trimmed = self.text.trim_start();
        let start = self
            .offset
            .checked_add(self.text.len().checked_sub(trimmed.len())?)?;
        if trimmed.is_empty() {
            self.text = trimmed;
            return None;
        }
        let len = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let (word, rest) = trimmed.split_at_checked(len)?;
        self.text = rest;
        self.offset = start.checked_add(len)?;
        Some((start, word))
    }
}

/// Emits the whole text as one token, the tokenizer of the `keyword` analyzer (SPEC §6.3). An empty text gives
/// no token; white space is kept.
///
/// ```
/// use persia_analysis::{TokenStream, KeywordTokenizer, Tokenizer};
///
/// let mut tokens = TokenStream::new();
/// KeywordTokenizer.tokenize("New York", &mut tokens);
/// assert_eq!(tokens.len(), 1);
/// assert_eq!((tokens[0].text.as_str(), tokens[0].end), ("New York", 8));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeywordTokenizer;

impl Tokenizer for KeywordTokenizer {
    fn tokenize(&self, text: &str, out: &mut TokenStream) {
        if !text.is_empty() {
            push_spans(std::iter::once((0, text)), out);
        }
    }
}
