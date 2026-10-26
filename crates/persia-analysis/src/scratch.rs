//! Reusable work buffers for token filters (ROADMAP 4.4.12).

/// Buffers a token filter may use while rewriting a token, kept by a [`TokenStream`](crate::TokenStream) so they are
/// reused from token to token and value to value: the stemmers work on characters in them and folding builds its
/// runs in them, so neither allocates once the buffers have grown.
///
/// Opaque: only this crate's stemmers and filters read the buffers. A [`Stemmer`](crate::Stemmer) outside the crate
/// receives one through [`Stemmer::stem_into`](crate::Stemmer::stem_into) and may ignore it.
#[derive(Debug, Default)]
pub struct Scratch {
    /// A word as characters, where a stemmer leaves its result.
    pub(crate) chars: Vec<char>,
    /// A second character buffer, for the steps that rebuild a word (preludes, postludes).
    pub(crate) work: Vec<char>,
    /// A text buffer (the folding run).
    pub(crate) text: String,
}

/// Buffers above this many characters (or bytes, for `text`) are shrunk after use, so one huge token does not stay
/// allocated: 4 times the default token length limit.
const MAX_KEPT: usize = 4 * crate::DEFAULT_MAX_TOKEN_LEN.get();

impl Scratch {
    /// Empty buffers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Empties the buffers, shrinking any that grew past the retention bound.
    pub(crate) fn reset(&mut self) {
        self.chars.clear();
        self.work.clear();
        self.text.clear();
        self.chars.shrink_to(MAX_KEPT);
        self.work.shrink_to(MAX_KEPT);
        self.text.shrink_to(MAX_KEPT);
    }
}
