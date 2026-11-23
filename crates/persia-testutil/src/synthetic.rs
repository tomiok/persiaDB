//! Synthetic corpora and the typo and term-drop queries a relaxation quality suite searches them with (ROADMAP 8.4.1,
//! SPEC §7.6), each with its ground truth.
//!
//! Everything is drawn from a [`Rng`] seeded by the caller, so a seed names a corpus and a suite: a failing test
//! prints its seed, and the same seed rebuilds the same queries on any platform (integer arithmetic only).
//!
//! A query is built from a *target* document: a few of its words (the *intended* query, which the target matches with
//! every word required), then changed so that no document matches it as typed:
//! - a **typo**: one intended word replaced by a word one edit away (a substitution, insertion, deletion or swap of two
//!   adjacent letters, the edits L4 `fuzzy` undoes) that no document holds, keeping at least 4 letters;
//! - a **term drop**: one word added that no document holds beside the intended ones, so L2 `drop_terms` must drop it;
//! - both.
//!
//! Its ground truth is the documents that hold every intended word: what the query meant. A synthetic corpus's words
//! are lowercase ASCII letters, and [`Corpus::from_texts`] keeps lowercase ASCII letters and digits; the `standard` and
//! `whitespace` analyzers keep such words, one space apart, as they are, so the engine's terms are the generator's
//! words.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::RangeInclusive;

/// `SplitMix64`: a small, fast generator whose output depends on the seed alone.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `0..n`; 0 when `n` is 0. Slightly biased toward low values for `n` far from a power of two, which a
    /// test generator does not mind.
    pub fn below(&mut self, n: usize) -> usize {
        let n64 = u64::try_from(n).unwrap_or(u64::MAX);
        if n64 == 0 {
            return 0;
        }
        usize::try_from(self.next_u64() % n64).unwrap_or(0)
    }

    /// A value in `range` (inclusive); its start when it is empty.
    pub fn within(&mut self, range: &RangeInclusive<usize>) -> usize {
        let (lo, hi) = (*range.start(), *range.end());
        if hi <= lo {
            return lo;
        }
        lo.saturating_add(self.below(hi.saturating_sub(lo).saturating_add(1)))
    }

    /// `true` with probability `percent` / 100.
    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    /// An element of `items`, `None` when it is empty.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        items.get(self.below(items.len()))
    }

    /// A seed for another generator, so that one stream's length does not shift another's draws.
    pub fn fork(&mut self) -> u64 {
        self.next_u64()
    }
}

/// A document of a [`Corpus`]: its id and its words, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusDoc {
    /// The document's id.
    pub id: String,
    /// Its words, lowercase ASCII letters (and digits, of [`Corpus::from_texts`]), in text order.
    pub words: Vec<String>,
}

impl CorpusDoc {
    /// The words as one text, one space apart: what a test writes in the document's text field.
    pub fn text(&self) -> String {
        self.words.join(" ")
    }
}

/// What [`Corpus::synthetic`] builds.
#[derive(Debug, Clone)]
pub struct CorpusOptions {
    /// How many documents.
    pub docs: usize,
    /// How many distinct words the vocabulary has.
    pub vocabulary: usize,
    /// How many words a document has.
    pub words: RangeInclusive<usize>,
}

impl Default for CorpusOptions {
    /// 200 documents of 5 to 20 words over 400 words: a corpus a test indexes in well under a second.
    fn default() -> Self {
        Self {
            docs: 200,
            vocabulary: 400,
            words: 5..=20,
        }
    }
}

/// A corpus: documents of words, and which documents hold each word.
#[derive(Debug, Clone)]
pub struct Corpus {
    docs: Vec<CorpusDoc>,
    /// By word, the indexes into `docs` of the documents holding it.
    postings: BTreeMap<String, BTreeSet<usize>>,
}

/// Two texts given to [`Corpus::from_texts`] with the same id (the id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateId(pub String);

impl fmt::Display for DuplicateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "two corpus documents have the id {:?}", self.0)
    }
}

impl std::error::Error for DuplicateId {}

impl Corpus {
    /// A corpus of `texts` (`(id, text)`), each text split into its runs of ASCII letters and digits, lowercased. Of a
    /// text whose words are separated by spaces or by punctuation between whole words, these are the terms a
    /// `standard` analyzer keeps; within a word UAX#29 joins more than this split does (`x_y`, `can't`, `3.14` are one
    /// term each), so a test whose ground truth must be the engine's gives no such text.
    ///
    /// # Errors
    ///
    /// [`DuplicateId`] when two texts have the same id: a document's id is its identity in the ground truth (and an
    /// engine replaces a document by id), so two documents of one id would make [`Corpus::matching`] ambiguous.
    pub fn from_texts<I, S, T>(texts: I) -> Result<Self, DuplicateId>
    where
        I: IntoIterator<Item = (S, T)>,
        S: Into<String>,
        T: AsRef<str>,
    {
        let mut ids = BTreeSet::new();
        let mut docs = Vec::new();
        for (id, text) in texts {
            let id: String = id.into();
            if !ids.insert(id.clone()) {
                return Err(DuplicateId(id));
            }
            docs.push(CorpusDoc {
                id,
                words: text
                    .as_ref()
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|w| !w.is_empty())
                    .map(str::to_ascii_lowercase)
                    .collect(),
            });
        }
        Ok(Self::of(docs))
    }

    /// A synthetic corpus drawn from `seed`: a vocabulary of pronounceable words of 3 to 10 letters, and documents whose
    /// words are drawn skewed toward the vocabulary's first words (so a few are common and most are rare, as in text).
    /// Ids are `d0`, `d1`...
    pub fn synthetic(seed: u64, options: &CorpusOptions) -> Self {
        let mut rng = Rng::new(seed);
        let vocabulary = vocabulary(&mut rng, options.vocabulary);
        let docs = (0..options.docs)
            .map(|i| {
                let len = rng.within(&options.words);
                let words = (0..len)
                    .filter_map(|_| {
                        // The smaller of two draws: word `k` is drawn about twice as often as word `2k`.
                        let (a, b) = (rng.below(vocabulary.len()), rng.below(vocabulary.len()));
                        vocabulary.get(a.min(b)).cloned()
                    })
                    .collect();
                CorpusDoc {
                    id: format!("d{i}"),
                    words,
                }
            })
            .collect();
        Self::of(docs)
    }

    fn of(docs: Vec<CorpusDoc>) -> Self {
        let mut postings: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
        for (i, doc) in docs.iter().enumerate() {
            for word in &doc.words {
                postings.entry(word.clone()).or_default().insert(i);
            }
        }
        Self { docs, postings }
    }

    /// The documents, in order.
    pub fn docs(&self) -> &[CorpusDoc] {
        &self.docs
    }

    /// Whether some document holds `word`.
    pub fn holds(&self, word: &str) -> bool {
        self.postings.contains_key(word)
    }

    /// The ids of the documents holding every one of `words`, in document order; every document for none.
    pub fn matching<S: AsRef<str>>(&self, words: &[S]) -> Vec<String> {
        let mut docs: Option<BTreeSet<usize>> = None;
        for word in words {
            let holding = self
                .postings
                .get(word.as_ref())
                .cloned()
                .unwrap_or_default();
            docs = Some(match docs {
                None => holding,
                Some(d) => d.intersection(&holding).copied().collect(),
            });
        }
        let all = || (0..self.docs.len()).collect::<BTreeSet<usize>>();
        docs.unwrap_or_else(all)
            .into_iter()
            .filter_map(|i| self.docs.get(i).map(|d| d.id.clone()))
            .collect()
    }
}

/// How a [`SyntheticQuery`] was changed from its intended words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueryKind {
    /// One word misspelled.
    Typo,
    /// One word added that no document holds beside the others.
    Drop,
    /// Both.
    TypoAndDrop,
}

impl QueryKind {
    /// Every kind, in order.
    pub const ALL: [Self; 3] = [Self::Typo, Self::Drop, Self::TypoAndDrop];

    /// The kind's name: `typo`, `drop`, `typo+drop`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Typo => "typo",
            Self::Drop => "drop",
            Self::TypoAndDrop => "typo+drop",
        }
    }
}

impl fmt::Display for QueryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A query of the suite and its ground truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticQuery {
    /// The seed this query was drawn from: [`QueryGenerator::query`] with it builds it again.
    pub seed: u64,
    /// How it was changed.
    pub kind: QueryKind,
    /// The text to search, one space between words.
    pub text: String,
    /// The words it was built from, in order: what the user meant.
    pub intended: Vec<String>,
    /// The document it was built from.
    pub target: String,
    /// The ground truth: every document holding all of `intended`, in corpus order (`target` among them).
    pub relevant: Vec<String>,
    /// The misspelled word as typed and the intended word it replaced, for a typo.
    pub typo: Option<(String, String)>,
    /// The added word, for a term drop.
    pub added: Option<String>,
}

impl SyntheticQuery {
    /// Whether `id` is a document the query meant (in [`Self::relevant`]).
    pub fn is_relevant(&self, id: &str) -> bool {
        self.relevant.iter().any(|r| r == id)
    }
}

/// What [`QueryGenerator`] draws.
#[derive(Debug, Clone)]
pub struct QueryOptions {
    /// How many intended words a query has.
    pub words: RangeInclusive<usize>,
    /// The relative weights of [`QueryKind::Typo`], [`QueryKind::Drop`] and [`QueryKind::TypoAndDrop`].
    pub weights: [usize; 3],
}

impl Default for QueryOptions {
    /// 2 or 3 intended words; typos twice as often as drops, and both as often as drops.
    fn default() -> Self {
        Self {
            words: 2..=3,
            weights: [2, 1, 1],
        }
    }
}

/// The fewest letters a misspelled word keeps: L4 corrects terms of 4 characters or more (SPEC §7.6).
pub const MIN_TYPO_LEN: usize = 4;

/// Draws [`SyntheticQuery`]s from a corpus.
#[derive(Debug)]
pub struct QueryGenerator<'c> {
    corpus: &'c Corpus,
    options: QueryOptions,
}

/// How many times a draw is tried before the generator gives up on a target or a word.
const TRIES: usize = 64;

impl<'c> QueryGenerator<'c> {
    /// A generator over `corpus`.
    pub fn new(corpus: &'c Corpus, options: QueryOptions) -> Self {
        Self { corpus, options }
    }

    /// `n` queries drawn from `seed`, each from a seed of its own ([`SyntheticQuery::seed`]); fewer when the corpus
    /// has too few documents a query can be built from (none of enough distinct words, or none with a word to
    /// misspell).
    pub fn generate(&self, seed: u64, n: usize) -> Vec<SyntheticQuery> {
        let mut rng = Rng::new(seed);
        let mut out = Vec::with_capacity(n);
        // A seed that builds no query is passed over; past `TRIES` misses per query, the corpus cannot give one.
        let mut misses = 0;
        while out.len() < n && misses < TRIES.saturating_mul(n.max(1)) {
            match self.query(rng.fork()) {
                Some(q) => out.push(q),
                None => misses += 1,
            }
        }
        out
    }

    /// The query `seed` draws, `None` when no try built one (no target of enough words, no word to misspell, or no
    /// word to add).
    pub fn query(&self, seed: u64) -> Option<SyntheticQuery> {
        let mut rng = Rng::new(seed);
        let [typo, drop, both] = self.options.weights;
        let total = typo.saturating_add(drop).saturating_add(both);
        let roll = rng.below(total);
        let kind = if roll < typo {
            QueryKind::Typo
        } else if roll < typo.saturating_add(drop) {
            QueryKind::Drop
        } else {
            QueryKind::TypoAndDrop
        };
        let misspells = kind != QueryKind::Drop;
        for _ in 0..TRIES {
            let doc = rng.pick(self.corpus.docs())?;
            let Some(intended) = self.intended(&mut rng, doc, misspells) else {
                continue;
            };
            let mut words = intended.clone();
            let typo = if misspells {
                let Some((at, typed)) = self.misspell(&mut rng, &words) else {
                    continue;
                };
                let meant = words.get(at).cloned()?;
                words.get_mut(at)?.clone_from(&typed);
                Some((typed, meant))
            } else {
                None
            };
            let added = if kind == QueryKind::Typo {
                None
            } else {
                let Some(word) = self.added(&mut rng, &intended) else {
                    continue;
                };
                let at = rng.below(words.len().saturating_add(1));
                words.insert(at, word.clone());
                Some(word)
            };
            return Some(SyntheticQuery {
                seed,
                kind,
                text: words.join(" "),
                relevant: self.corpus.matching(&intended),
                intended,
                target: doc.id.clone(),
                typo,
                added,
            });
        }
        None
    }

    /// Distinct words of `doc`, in its order, as many as the options draw; with one of at least
    /// [`MIN_TYPO_LEN`] + 1 letters when the query `misspells` one (so a deletion keeps [`MIN_TYPO_LEN`]).
    fn intended(&self, rng: &mut Rng, doc: &CorpusDoc, misspells: bool) -> Option<Vec<String>> {
        let distinct: Vec<&String> = {
            let mut seen = BTreeSet::new();
            doc.words
                .iter()
                .filter(|w| seen.insert(w.as_str()))
                .collect()
        };
        let n = rng.within(&self.options.words);
        if n == 0 || distinct.len() < n {
            return None;
        }
        // `n` distinct positions, kept in text order.
        let mut chosen: BTreeSet<usize> = BTreeSet::new();
        while chosen.len() < n {
            chosen.insert(rng.below(distinct.len()));
        }
        let words: Vec<String> = chosen
            .into_iter()
            .filter_map(|i| distinct.get(i).map(|w| (*w).clone()))
            .collect();
        let long = |w: &String| w.len() > MIN_TYPO_LEN;
        (!misspells || words.iter().any(long)).then_some(words)
    }

    /// A word of `words` (of more than [`MIN_TYPO_LEN`] letters) and a misspelling of it: one edit, at least
    /// [`MIN_TYPO_LEN`] letters, held by no document. `None` when no try found one.
    fn misspell(&self, rng: &mut Rng, words: &[String]) -> Option<(usize, String)> {
        let candidates: Vec<usize> = (0..words.len())
            .filter(|&i| words.get(i).is_some_and(|w| w.len() > MIN_TYPO_LEN))
            .collect();
        for _ in 0..TRIES {
            let at = *rng.pick(&candidates)?;
            let word = words.get(at)?;
            let typed = edit(rng, word);
            if typed.len() >= MIN_TYPO_LEN && typed != *word && !self.corpus.holds(&typed) {
                return Some((at, typed));
            }
        }
        None
    }

    /// A word no document holds beside every word of `intended`: a vocabulary word when one is found, else a made-up
    /// one no document holds at all; `None` when [`TRIES`] draws of each found neither (a corpus so degenerate gives
    /// fewer queries, never a hang).
    fn added(&self, rng: &mut Rng, intended: &[String]) -> Option<String> {
        for _ in 0..TRIES {
            let Some(doc) = rng.pick(self.corpus.docs()) else {
                break;
            };
            let Some(word) = rng.pick(&doc.words) else {
                continue;
            };
            if intended.contains(word) {
                continue;
            }
            let mut all = intended.to_vec();
            all.push(word.clone());
            if self.corpus.matching(&all).is_empty() {
                return Some(word.clone());
            }
        }
        made_up(rng, |w| self.corpus.holds(w))
    }
}

/// A pronounceable word of 5 to 8 letters that `held` says no document holds; `None` after [`TRIES`] draws held.
fn made_up(rng: &mut Rng, held: impl Fn(&str) -> bool) -> Option<String> {
    (0..TRIES)
        .map(|_| pronounceable(rng, 5..=8))
        .find(|word| !held(word))
}

const CONSONANTS: &[u8] = b"bcdfghjklmnprstvz";
const VOWELS: &[u8] = b"aeiou";
const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";

/// A word of alternating consonants and vowels, of a length in `len`.
fn pronounceable(rng: &mut Rng, len: RangeInclusive<usize>) -> String {
    let n = rng.within(&len);
    let start_vowel = rng.chance(20);
    (0..n)
        .filter_map(|i| {
            let set = if (i % 2 == 0) ^ start_vowel {
                CONSONANTS
            } else {
                VOWELS
            };
            rng.pick(set).map(|&b| char::from(b))
        })
        .collect()
}

/// `n` distinct pronounceable words of 3 to 10 letters, most of 4 to 8.
fn vocabulary(rng: &mut Rng, n: usize) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut words = Vec::with_capacity(n);
    // Each length has 17^k·5^k words at least; 3 letters alone have over 400, so `n` up to tens of thousands fills.
    let mut misses = 0usize;
    while words.len() < n && misses < n.saturating_mul(TRIES).max(TRIES) {
        let len = if rng.chance(10) { 3..=10 } else { 4..=8 };
        let word = pronounceable(rng, len);
        if seen.insert(word.clone()) {
            words.push(word);
        } else {
            misses += 1;
        }
    }
    words
}

/// `word` with one edit: a letter substituted, inserted or deleted, or two adjacent different letters swapped (the
/// optimal string alignment edits, SPEC §7.1). Its letters are ASCII, so bytes are characters.
fn edit(rng: &mut Rng, word: &str) -> String {
    let mut bytes: Vec<u8> = word.bytes().collect();
    let len = bytes.len();
    let letter = |rng: &mut Rng| rng.pick(LETTERS).copied().unwrap_or(b'a');
    match rng.below(4) {
        0 if len > 0 => {
            let at = rng.below(len);
            let new = letter(rng);
            if let Some(b) = bytes.get_mut(at) {
                *b = new;
            }
        }
        1 => {
            let at = rng.below(len.saturating_add(1));
            let new = letter(rng);
            bytes.insert(at.min(len), new);
        }
        2 if len > 0 => {
            bytes.remove(rng.below(len));
        }
        _ if len >= 2 => {
            let at = rng.below(len.saturating_sub(1));
            bytes.swap(at, at.saturating_add(1).min(len.saturating_sub(1)));
        }
        _ => {}
    }
    String::from_utf8(bytes).unwrap_or_default()
}

#[cfg(test)]
mod tests;
