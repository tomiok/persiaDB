//! A reusable token buffer: the zero-allocation fast path of analysis (SPEC §6.3.1, ROADMAP 4.4.4).

use std::fmt;
use std::ops::{Deref, DerefMut};

use crate::token::Token;

/// The tokens of one analyzed value, in a buffer that keeps its allocations across values.
///
/// Tokenizers append to it and token filters rewrite it ([`Tokenizer`](crate::Tokenizer),
/// [`TokenFilter`](crate::TokenFilter)). The strings of tokens it drops ([`clear`](Self::clear),
/// [`retain_mut`](Self::retain_mut)) and of texts it replaces ([`rewrite`](Self::rewrite)) are kept for the next
/// tokens, so analyzing value after value with one stream allocates only while it grows: once it has held as many
/// tokens, as long, as a value needs, tokenizing and the filters that rewrite in place or through `rewrite` do not
/// allocate. Dereferences to the token slice.
///
/// ```
/// use persia_analysis::{StandardTokenizer, TokenStream, Tokenizer};
///
/// let mut tokens = TokenStream::new();
/// for value in ["first value", "second value"] {
///     tokens.clear();
///     StandardTokenizer.tokenize(value, &mut tokens);
///     assert_eq!(tokens.len(), 2);
/// }
/// assert_eq!(tokens[0].text, "second");
/// ```
#[derive(Default)]
pub struct TokenStream {
    tokens: Vec<Token>,
    /// The other token buffer of [`Self::rebuild`], empty between calls.
    previous: Vec<Token>,
    /// Emptied strings with their capacity, for the next tokens and rewrites.
    spare: Vec<String>,
}

impl TokenStream {
    /// An empty stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a token with a copy of `text`, reusing a spare string.
    pub fn push(&mut self, text: &str, start: usize, end: usize, position: u32) {
        let mut owned = self.spare_string();
        owned.push_str(text);
        self.tokens.push(Token {
            text: owned,
            start,
            end,
            position,
        });
    }

    /// Appends `token` as is.
    pub fn push_token(&mut self, token: Token) {
        self.tokens.push(token);
    }

    /// Removes every token, keeping the strings for reuse.
    pub fn clear(&mut self) {
        self.spare.extend(self.tokens.drain(..).map(|token| {
            let mut text = token.text;
            text.clear();
            text
        }));
    }

    /// Keeps the tokens for which `keep` returns `true`, in order; the strings of the others are kept for reuse.
    pub fn retain(&mut self, mut keep: impl FnMut(&Token) -> bool) {
        self.retain_mut(|token| keep(token));
    }

    /// Keeps the tokens for which `keep` returns `true`, in order; the strings of the others are kept for reuse.
    pub fn retain_mut(&mut self, mut keep: impl FnMut(&mut Token) -> bool) {
        let mut kept = 0;
        for i in 0..self.tokens.len() {
            let Some(token) = self.tokens.get_mut(i) else {
                break;
            };
            if keep(token) {
                self.tokens.swap(kept, i);
                kept = kept.saturating_add(1);
            }
        }
        for token in self.tokens.drain(kept..) {
            let mut text = token.text;
            text.clear();
            self.spare.push(text);
        }
    }

    /// Rewrites the text of every token through `write`, which gets the current text and an empty spare string and
    /// returns whether it wrote a replacement there; the replaced string is kept for reuse. Tokens left empty are
    /// removed.
    pub fn rewrite(&mut self, mut write: impl FnMut(&str, &mut String) -> bool) {
        let mut scratch = self.spare_string();
        let mut emptied = false;
        for token in &mut self.tokens {
            if write(&token.text, &mut scratch) {
                std::mem::swap(&mut token.text, &mut scratch);
                emptied |= token.text.is_empty();
            }
            scratch.clear();
        }
        self.spare.push(scratch);
        if emptied {
            self.retain_mut(|token| !token.text.is_empty());
        }
    }

    /// Rebuilds the stream from its tokens: `f` gets each token, in order, and the emptied stream to push its
    /// replacements to (none, itself, or several). The token buffers swap, so this does not allocate either once
    /// the stream has grown.
    pub fn rebuild(&mut self, mut f: impl FnMut(Token, &mut Self)) {
        let mut input = std::mem::take(&mut self.previous);
        std::mem::swap(&mut input, &mut self.tokens);
        for token in input.drain(..) {
            f(token, self);
        }
        self.previous = input;
    }

    /// Keeps `text`'s allocation for later tokens.
    pub fn recycle(&mut self, mut text: String) {
        text.clear();
        self.spare.push(text);
    }

    /// The tokens, as a vector.
    pub fn into_vec(self) -> Vec<Token> {
        self.tokens
    }

    fn spare_string(&mut self) -> String {
        self.spare.pop().unwrap_or_default()
    }
}

impl Deref for TokenStream {
    type Target = [Token];

    fn deref(&self) -> &[Token] {
        &self.tokens
    }
}

impl DerefMut for TokenStream {
    fn deref_mut(&mut self) -> &mut [Token] {
        &mut self.tokens
    }
}

impl From<Vec<Token>> for TokenStream {
    fn from(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            ..Self::default()
        }
    }
}

impl From<TokenStream> for Vec<Token> {
    fn from(stream: TokenStream) -> Self {
        stream.tokens
    }
}

impl IntoIterator for TokenStream {
    type Item = Token;
    type IntoIter = std::vec::IntoIter<Token>;

    fn into_iter(self) -> Self::IntoIter {
        self.tokens.into_iter()
    }
}

impl<'a> IntoIterator for &'a TokenStream {
    type Item = &'a Token;
    type IntoIter = std::slice::Iter<'a, Token>;

    fn into_iter(self) -> Self::IntoIter {
        self.tokens.iter()
    }
}

impl<'a> IntoIterator for &'a mut TokenStream {
    type Item = &'a mut Token;
    type IntoIter = std::slice::IterMut<'a, Token>;

    fn into_iter(self) -> Self::IntoIter {
        self.tokens.iter_mut()
    }
}

/// Streams are equal when their tokens are: spare strings are not content.
impl PartialEq for TokenStream {
    fn eq(&self, other: &Self) -> bool {
        self.tokens == other.tokens
    }
}

impl PartialEq<[Token]> for TokenStream {
    fn eq(&self, other: &[Token]) -> bool {
        self.tokens == other
    }
}

impl<const N: usize> PartialEq<[Token; N]> for TokenStream {
    fn eq(&self, other: &[Token; N]) -> bool {
        self.tokens == other
    }
}

impl PartialEq<Vec<Token>> for TokenStream {
    fn eq(&self, other: &Vec<Token>) -> bool {
        &self.tokens == other
    }
}

impl Eq for TokenStream {}

impl Clone for TokenStream {
    fn clone(&self) -> Self {
        Self::from(self.tokens.clone())
    }
}

impl fmt::Debug for TokenStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(&self.tokens).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;

    fn texts(stream: &TokenStream) -> Vec<&str> {
        stream.iter().map(|t| t.text.as_str()).collect()
    }

    /// The addresses of every string the stream holds (token texts and spares) and its token capacity: when they do
    /// not change, analyzing allocated nothing for the stream.
    fn buffers(stream: &TokenStream) -> (Vec<(usize, usize)>, usize) {
        let mut strings: Vec<(usize, usize)> = stream
            .tokens
            .iter()
            .map(|t| &t.text)
            .chain(&stream.spare)
            .filter(|s| s.capacity() > 0)
            .map(|s| (s.as_ptr() as usize, s.capacity()))
            .collect();
        strings.sort_unstable();
        (
            strings,
            stream.tokens.capacity() + stream.previous.capacity(),
        )
    }

    /// ROADMAP 4.4.4: analyzing value after value with one stream reuses its buffers. Once a round over the values
    /// changes nothing (it converges within a few rounds, as strings only grow), later rounds create, grow or free no
    /// string and no token buffer, for every built-in analyzer and a chain of every rewriting filter.
    #[test]
    fn analysis_reuses_the_stream_buffers() {
        use crate::{
            Analyzer, ApostropheFilter, BUILTIN_ANALYZERS, EdgeNgramFilter, FoldingFilter,
            LowercaseFilter, NfkcFilter, StandardTokenizer, StopwordFilter, TokenLengthGuard,
            builtin_analyzer,
        };
        let values = [
            "The quick brown fox, don’t panic: ÁRBOL Ñandú ΟΔΟΣ ﬁne Straße 3.14",
            "Los años de las canciones brasileiras: ações não são",
            "",
            "one",
            "привет мир 日本語のテキスト 가나다 running connected",
        ];
        let mut analyzers: Vec<(String, Analyzer)> = BUILTIN_ANALYZERS
            .iter()
            .map(|a| {
                (
                    a.name.to_owned(),
                    builtin_analyzer(a.name, a.version).unwrap(),
                )
            })
            .collect();
        let ngrams =
            EdgeNgramFilter::new(NonZeroUsize::MIN, NonZeroUsize::new(4).unwrap()).unwrap();
        analyzers.push((
            "every filter".to_owned(),
            Analyzer::builder(StandardTokenizer)
                .filter(NfkcFilter)
                .filter(LowercaseFilter)
                .filter(ApostropheFilter)
                .filter(StopwordFilter::english())
                .filter(FoldingFilter::preserving(['ñ']))
                .filter(TokenLengthGuard::default())
                .filter(ngrams.preserve_original(true))
                .build(),
        ));
        for (name, analyzer) in &analyzers {
            let mut stream = TokenStream::new();
            let round = |stream: &mut TokenStream| {
                for value in values {
                    analyzer.analyze(value, stream);
                }
            };
            let mut converged = None;
            for i in 0..32 {
                let before = buffers(&stream);
                round(&mut stream);
                if buffers(&stream) == before {
                    converged = Some(i);
                    break;
                }
            }
            assert!(converged.is_some(), "{name}: buffers keep changing");
            let settled = buffers(&stream);
            for _ in 0..3 {
                for value in values {
                    analyzer.analyze(value, &mut stream);
                    let (strings, capacity) = buffers(&stream);
                    assert_eq!(
                        capacity, settled.1,
                        "{name}: token buffer grew on {value:?}"
                    );
                    assert!(
                        strings.iter().all(|s| settled.0.binary_search(s).is_ok()),
                        "{name}: a string was allocated or grown on {value:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn strings_are_reused() {
        let mut stream = TokenStream::new();
        stream.push("abcdef", 0, 6, 0);
        let capacity = stream[0].text.capacity();
        stream.clear();
        assert!(stream.is_empty());
        stream.push("x", 0, 1, 0);
        assert_eq!(stream[0].text, "x");
        assert_eq!(stream[0].text.capacity(), capacity);
    }

    #[test]
    fn retain_keeps_order_and_recycles() {
        let mut stream = TokenStream::new();
        for (i, t) in ["a", "bb", "c", "dd", "e"].iter().enumerate() {
            let i = u32::try_from(i).unwrap();
            stream.push(t, 0, 1, i);
        }
        stream.retain_mut(|t| t.text.len() == 1);
        assert_eq!(texts(&stream), ["a", "c", "e"]);
        assert_eq!(
            stream.iter().map(|t| t.position).collect::<Vec<_>>(),
            [0, 2, 4]
        );
        assert_eq!(stream.spare.len(), 2);
        assert!(stream.spare.iter().all(String::is_empty));
    }

    #[test]
    fn rewrite_swaps_and_removes_emptied_tokens() {
        let mut stream = TokenStream::from(vec![
            Token {
                text: "Ab".to_owned(),
                start: 0,
                end: 2,
                position: 0,
            },
            Token {
                text: "cd".to_owned(),
                start: 3,
                end: 5,
                position: 1,
            },
            Token {
                text: "--".to_owned(),
                start: 6,
                end: 8,
                position: 2,
            },
        ]);
        stream.rewrite(|text, out| {
            if text.starts_with('A') {
                out.push_str("ab");
                true
            } else if text == "--" {
                true // replaced by the empty string
            } else {
                false
            }
        });
        assert_eq!(texts(&stream), ["ab", "cd"]);
        assert!(stream.spare.iter().all(String::is_empty));
    }
}
