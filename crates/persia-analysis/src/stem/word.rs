//! A word as characters, with the suffix tests and edits the Snowball algorithms are written in.
//!
//! Positions are character indices. The algorithms only edit the end of a word, so regions computed on the word
//! before stemming (`RV`, `R1`, `R2`) stay valid as it shrinks.

pub(super) struct Word {
    chars: Vec<char>,
}

impl Word {
    pub(super) fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
        }
    }

    pub(super) fn from_chars(chars: Vec<char>) -> Self {
        Self { chars }
    }

    pub(super) fn chars(&self) -> &[char] {
        &self.chars
    }

    pub(super) fn chars_mut(&mut self) -> &mut [char] {
        &mut self.chars
    }

    pub(super) fn len(&self) -> usize {
        self.chars.len()
    }

    /// The character just before position `at`, if any.
    pub(super) fn before(&self, at: usize) -> Option<char> {
        at.checked_sub(1).and_then(|i| self.chars.get(i)).copied()
    }

    /// Where `suffix` starts if the characters ending at `end` are `suffix`.
    pub(super) fn suffix_start(&self, end: usize, suffix: &str) -> Option<usize> {
        if end > self.chars.len() {
            return None;
        }
        let mut start = end;
        for c in suffix.chars().rev() {
            start = start.checked_sub(1)?;
            if self.chars.get(start) != Some(&c) {
                return None;
            }
        }
        Some(start)
    }

    /// Where `suffix` starts if the word ends with it.
    pub(super) fn ends_with(&self, suffix: &str) -> Option<usize> {
        self.suffix_start(self.chars.len(), suffix)
    }

    /// The longest entry of `suffixes` ending at `end` and starting at or after `min_start`, with its start
    /// (Snowball's `substring ... among`).
    pub(super) fn longest_suffix<'a>(
        &self,
        end: usize,
        suffixes: &[&'a str],
        min_start: usize,
    ) -> Option<(&'a str, usize)> {
        suffixes
            .iter()
            .filter_map(|&s| {
                self.suffix_start(end, s)
                    .filter(|&start| start >= min_start)
                    .map(|start| (s, start))
            })
            .min_by_key(|&(_, start)| start)
    }

    /// [`Self::longest_suffix`] over a table of suffixes and the rule each one selects, at the end of the word.
    pub(super) fn longest_rule<T: Copy>(
        &self,
        rules: &[(&str, T)],
        min_start: usize,
    ) -> Option<(T, usize)> {
        rules
            .iter()
            .filter_map(|&(s, rule)| {
                self.ends_with(s)
                    .filter(|&start| start >= min_start)
                    .map(|start| (rule, start))
            })
            .min_by_key(|&(_, start)| start)
    }

    /// Removes the first character, if any.
    pub(super) fn remove_first(&mut self) {
        if !self.chars.is_empty() {
            self.chars.remove(0);
        }
    }

    /// Removes everything from `start` on.
    pub(super) fn truncate(&mut self, start: usize) {
        self.chars.truncate(start);
    }

    /// Replaces everything from `start` on with `with`.
    pub(super) fn replace_tail(&mut self, start: usize, with: &str) {
        self.chars.truncate(start);
        self.chars.extend(with.chars());
    }

    pub(super) fn push(&mut self, c: char) {
        self.chars.push(c);
    }

    pub(super) fn pop(&mut self) {
        self.chars.pop();
    }
}

/// Snowball's `gopast v gopast non-v` from `from`: the position after the first non-vowel that follows a vowel.
pub(super) fn after_vowel_and_consonant(
    chars: &[char],
    from: usize,
    is_vowel: fn(char) -> bool,
) -> Option<usize> {
    let mut seen_vowel = false;
    for (i, &c) in chars.iter().enumerate().skip(from) {
        if !seen_vowel {
            seen_vowel = is_vowel(c);
        } else if !is_vowel(c) {
            return i.checked_add(1);
        }
    }
    None
}

/// `R1` and `R2` starts of the Snowball algorithms: each is the region after the first non-vowel following a vowel,
/// `R2` searched from `R1`; the word's length where there is none.
pub(super) fn r1_r2(chars: &[char], is_vowel: fn(char) -> bool) -> (usize, usize) {
    let len = chars.len();
    match after_vowel_and_consonant(chars, 0, is_vowel) {
        Some(p1) => (
            p1,
            after_vowel_and_consonant(chars, p1, is_vowel).unwrap_or(len),
        ),
        None => (len, len),
    }
}

/// The `RV` start of the Spanish and Portuguese algorithms: if the second letter is a consonant, the region after
/// the next vowel; if the first two letters are vowels, after the next consonant; otherwise after the third letter.
/// The word's length where there is none.
pub(super) fn rv(chars: &[char], is_vowel: fn(char) -> bool) -> usize {
    let len = chars.len();
    let (Some(&first), Some(&second)) = (chars.first(), chars.get(1)) else {
        return len;
    };
    let after_first = |want_vowel: bool| {
        chars
            .iter()
            .enumerate()
            .skip(2)
            .find(|&(_, &c)| is_vowel(c) == want_vowel)
            .and_then(|(i, _)| i.checked_add(1))
    };
    let found = if !is_vowel(second) {
        after_first(true)
    } else if is_vowel(first) {
        after_first(false)
    } else {
        // Consonant then vowel: after the third letter.
        (len >= 3).then_some(3)
    };
    found.unwrap_or(len)
}

/// The stemmed word, borrowing `original` when stemming left it unchanged.
pub(super) fn finish<'a>(original: &'a str, chars: &[char]) -> std::borrow::Cow<'a, str> {
    if original.chars().eq(chars.iter().copied()) {
        std::borrow::Cow::Borrowed(original)
    } else {
        std::borrow::Cow::Owned(chars.iter().collect())
    }
}
