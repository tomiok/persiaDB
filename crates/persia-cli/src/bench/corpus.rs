//! The synthetic corpus and query mix of `persia bench` (ROADMAP 10.4.3): pure functions of a seed, so the same seed
//! gives the same documents and the same queries on every machine and build.
//!
//! Nothing here reads a clock or the operating system's randomness. A document is a function of `(seed, index)` and
//! a query of `(seed, index, corpus size)`, so neither needs the others in memory: the corpus is generated in batches
//! as it is indexed, and a query re-derives the document it is drawn from.
//!
//! Words are made of consonant-vowel syllables (`bako`, `mitu`), which the default analyzer keeps as one token each.
//! Their frequencies are skewed (the smaller of two uniform draws), so a few words are common and most are rare, as
//! in text. This is a workload for a quick local measurement, not a model of any real corpus.

use std::fmt::Write;

/// Words in the vocabulary.
pub const VOCABULARY: u64 = 5000;
/// Distinct values of the `category` keyword field.
pub const CATEGORIES: u64 = 16;
/// Words in a title, at least and at most.
const TITLE_WORDS: (u64, u64) = (3, 8);
/// Words in a body, at least and at most.
const BODY_WORDS: (u64, u64) = (20, 80);
/// Prices are below this.
const PRICES: u64 = 100_000;

const CONSONANTS: &[u8] = b"bdfgklmnprstvz";
const VOWELS: &[u8] = b"aeiou";

/// What separates the query stream of a seed from its document stream.
const QUERY_STREAM: u64 = 0x5155_4552_595f_5354;

/// `SplitMix64`: a small, well-mixed generator with a 64-bit state. Its arithmetic wraps by definition.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// The generator of stream `stream` of `seed`: distinct streams of one seed are unrelated sequences.
    pub fn new(seed: u64, stream: u64) -> Self {
        let mut rng = Self(seed);
        let mixed = rng.next_u64() ^ stream.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        Self(mixed)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `n`; 0 when `n` is 0. The high bits of a 128-bit product: no modulo bias worth the name for
    /// the small `n` used here.
    pub fn below(&mut self, n: u64) -> u64 {
        let product = u128::from(self.next_u64()).wrapping_mul(u128::from(n)) >> 64;
        u64::try_from(product).unwrap_or(0)
    }

    /// A number in `low..=high` (`low` when the range is empty).
    fn within(&mut self, (low, high): (u64, u64)) -> u64 {
        let span = high.saturating_sub(low).saturating_add(1);
        low.saturating_add(self.below(span))
    }

    /// The index of a vocabulary word, small indexes more often: the smaller of two uniform draws.
    fn word_index(&mut self) -> u64 {
        self.below(VOCABULARY).min(self.below(VOCABULARY))
    }
}

/// The syllables of the longest word: a `u64` has at most 11 digits in base 70 (`70^11 > 2^64`).
const MAX_SYLLABLES: usize = 11;

/// Word `index` of the vocabulary: its digits in base 70 as consonant-vowel syllables, the lowest first, at least
/// two. Distinct indexes give distinct words.
pub fn word(index: u64) -> String {
    let consonants = u64::try_from(CONSONANTS.len()).unwrap_or(1);
    let vowels = u64::try_from(VOWELS.len()).unwrap_or(1);
    let base = consonants.saturating_mul(vowels);
    let mut out = String::new();
    let mut rest = index;
    // Bounded by the digits a `u64` can have, so no arithmetic slip can make a word grow without end.
    for _ in 0..MAX_SYLLABLES {
        let digit = rest.checked_rem(base).unwrap_or(0);
        let consonant = digit.checked_div(vowels).unwrap_or(0);
        let vowel = digit.checked_rem(vowels).unwrap_or(0);
        out.push(letter(CONSONANTS, consonant));
        out.push(letter(VOWELS, vowel));
        rest = rest.checked_div(base).unwrap_or(0);
        if rest == 0 && out.len() >= 4 {
            break;
        }
    }
    out
}

fn letter(alphabet: &[u8], index: u64) -> char {
    usize::try_from(index)
        .ok()
        .and_then(|i| alphabet.get(i))
        .map_or('a', |b| char::from(*b))
}

/// One synthetic document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticDoc {
    /// `doc-` and the index, zero-padded: ids sort in index order.
    pub id: String,
    /// The words of the `title` field.
    pub title: Vec<String>,
    /// The words of the `body` field.
    pub body: Vec<String>,
    /// The `category` keyword.
    pub category: String,
    /// The `price` number.
    pub price: i64,
}

/// Document `index` of the corpus of `seed`.
pub fn document(seed: u64, index: u64) -> SyntheticDoc {
    let mut rng = Rng::new(seed, index);
    let words = |rng: &mut Rng, range| -> Vec<String> {
        (0..rng.within(range))
            .map(|_| word(rng.word_index()))
            .collect()
    };
    let title = words(&mut rng, TITLE_WORDS);
    let body = words(&mut rng, BODY_WORDS);
    let category = category(rng.below(CATEGORIES));
    let price = i64::try_from(rng.below(PRICES)).unwrap_or(0);
    SyntheticDoc {
        id: format!("doc-{index:08}"),
        title,
        body,
        category,
        price,
    }
}

fn category(index: u64) -> String {
    format!("cat-{index:02}")
}

/// The kinds of query in the mix, in the order they rotate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum QueryKind {
    /// One word of the vocabulary.
    Term,
    /// Two words of one document's body, both required.
    Terms,
    /// Two adjacent words of one document's body, as a phrase.
    Phrase,
    /// One word of a document's body, filtered to that document's category.
    Filter,
    /// A word of a document's body with one letter changed: what relaxation is for.
    Typo,
}

impl QueryKind {
    /// Every kind, in rotation order: query `i` is of kind `ALL[i % 5]`.
    pub const ALL: [Self; 5] = [
        Self::Term,
        Self::Terms,
        Self::Phrase,
        Self::Filter,
        Self::Typo,
    ];

    /// The kind's name in the report.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Term => "term",
            Self::Terms => "terms",
            Self::Phrase => "phrase",
            Self::Filter => "filter",
            Self::Typo => "typo",
        }
    }
}

/// One query of the mix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticQuery {
    /// Its kind.
    pub kind: QueryKind,
    /// The user text (SPEC §7.1.1).
    pub text: String,
    /// The `category` value to filter by, for [`QueryKind::Filter`].
    pub category: Option<String>,
}

/// Query `index` of the mix of `seed` over a corpus of `docs` documents. The kinds rotate, so `n` queries hold
/// each kind `n / 5` times, give or take one.
pub fn query(seed: u64, index: u64, docs: u64) -> SyntheticQuery {
    let kinds = u64::try_from(QueryKind::ALL.len()).unwrap_or(1);
    let kind = usize::try_from(index.checked_rem(kinds).unwrap_or(0))
        .ok()
        .and_then(|i| QueryKind::ALL.get(i).copied())
        .unwrap_or(QueryKind::Term);
    let mut rng = Rng::new(seed ^ QUERY_STREAM, index);
    // The document the query is drawn from, so most queries match something.
    let source = document(seed, rng.below(docs));
    let at = rng.below(
        u64::try_from(source.body.len())
            .unwrap_or(0)
            .saturating_sub(1),
    );
    let body_word = |offset: u64| -> String {
        usize::try_from(at.saturating_add(offset))
            .ok()
            .and_then(|i| source.body.get(i))
            .cloned()
            .unwrap_or_else(|| word(0))
    };
    let (text, category) = match kind {
        QueryKind::Term => (word(rng.word_index()), None),
        QueryKind::Terms => (format!("{} {}", body_word(0), body_word(1)), None),
        QueryKind::Phrase => (format!("\"{} {}\"", body_word(0), body_word(1)), None),
        QueryKind::Filter => (body_word(0), Some(source.category.clone())),
        QueryKind::Typo => (misspelled(&body_word(0), &mut rng), None),
    };
    SyntheticQuery {
        kind,
        text,
        category,
    }
}

/// `word` with the vowel of one syllable replaced by another vowel. The result may be another word of the
/// vocabulary: then the query is an exact one, as some real typos are.
fn misspelled(word: &str, rng: &mut Rng) -> String {
    let syllables = u64::try_from(word.len().checked_div(2).unwrap_or(0)).unwrap_or(0);
    let target = rng.below(syllables).saturating_mul(2).saturating_add(1);
    let shift = rng.within((1, 4));
    word.bytes()
        .enumerate()
        .map(|(i, b)| {
            if u64::try_from(i).ok() != Some(target) {
                return char::from(b);
            }
            let current = VOWELS.iter().position(|v| *v == b).unwrap_or(0);
            let next = u64::try_from(current).unwrap_or(0).saturating_add(shift);
            letter(VOWELS, next.checked_rem(5).unwrap_or(0))
        })
        .collect()
}

/// FNV-1a over a stream of fields: a fingerprint of the generated corpus or query mix, so two runs can be seen to
/// have measured the same work. Not a checksum of anything stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint(u64);

impl Default for Fingerprint {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fingerprint {
    fn bytes(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    /// Adds one field; a separator follows it, so `("ab", "c")` and `("a", "bc")` differ.
    fn field(&mut self, text: &str) {
        self.bytes(text.as_bytes());
        self.bytes(&[0xff]);
    }

    /// Adds a document.
    pub fn document(&mut self, doc: &SyntheticDoc) {
        self.field(&doc.id);
        self.field(&doc.title.join(" "));
        self.field(&doc.body.join(" "));
        self.field(&doc.category);
        self.field(&doc.price.to_string());
    }

    /// Adds a query.
    pub fn query(&mut self, query: &SyntheticQuery) {
        self.field(query.kind.name());
        self.field(&query.text);
        self.field(query.category.as_deref().unwrap_or(""));
    }

    /// 16 lowercase hex digits.
    pub fn hex(self) -> String {
        let mut out = String::new();
        // Writing to a `String` cannot fail.
        if write!(out, "{:016x}", self.0).is_err() {
            out.clear();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[test]
    fn the_generator_is_splitmix64() {
        // The reference sequence of SplitMix64 from state 0 (Vigna's `splitmix64.c`).
        let mut rng = Rng(0);
        assert_eq!(rng.next_u64(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(rng.next_u64(), 0x6e78_9e6a_a1b9_65f4);
        assert_eq!(rng.next_u64(), 0x06c4_5d18_8009_454f);
    }

    #[test]
    fn streams_of_one_seed_and_seeds_of_one_stream_differ() {
        let first = |seed, stream| Rng::new(seed, stream).next_u64();
        assert_eq!(first(7, 3), first(7, 3));
        assert_ne!(first(7, 3), first(7, 4));
        assert_ne!(first(7, 3), first(8, 3));
        // Stream 0 is not the bare seed's sequence shifted by one.
        assert_ne!(first(7, 0), first(7, 1));
    }

    #[test]
    fn below_stays_below_and_reaches_both_ends() {
        let mut rng = Rng::new(1, 0);
        let drawn: BTreeSet<u64> = (0..2000).map(|_| rng.below(7)).collect();
        assert_eq!(drawn, (0..7).collect());
        assert_eq!(rng.below(0), 0);
        assert_eq!(rng.below(1), 0);
        let mut rng = Rng::new(2, 0);
        let drawn: BTreeSet<u64> = (0..2000).map(|_| rng.within((3, 8))).collect();
        assert_eq!(drawn, (3..=8).collect());
        assert_eq!(rng.within((5, 5)), 5);
    }

    #[test]
    fn words_are_distinct_syllable_strings() {
        assert_eq!(word(0), "baba");
        assert_eq!(word(1), "beba");
        assert_eq!(word(5), "daba");
        assert_eq!(word(69), "zuba");
        assert_eq!(word(70), "babe");
        assert_eq!(word(4899), "zuzu");
        assert_eq!(word(4900), "bababe");
        // The largest index has all 11 digits, and is still its own word.
        assert_eq!(word(u64::MAX).len(), 2 * MAX_SYLLABLES);
        assert_ne!(word(u64::MAX), word(u64::MAX - 1));
        assert_ne!(word(u64::MAX), word(u64::MAX / 70));
        let words: BTreeSet<String> = (0..VOCABULARY).map(word).collect();
        assert_eq!(u64::try_from(words.len()).unwrap(), VOCABULARY);
        for w in &words {
            assert!(w.len() >= 4 && w.len() % 2 == 0, "{w}");
            for (i, b) in w.bytes().enumerate() {
                let alphabet = if i % 2 == 0 { CONSONANTS } else { VOWELS };
                assert!(alphabet.contains(&b), "{w}");
            }
        }
    }

    #[test]
    fn a_document_is_a_function_of_its_seed_and_index() {
        let doc = document(42, 17);
        assert_eq!(doc, document(42, 17));
        assert_ne!(doc, document(42, 18));
        assert_ne!(doc.body, document(43, 17).body);
        assert_eq!(doc.id, "doc-00000017");
        for index in 0..200 {
            let doc = document(42, index);
            assert!((3..=8).contains(&doc.title.len()), "{doc:?}");
            assert!((20..=80).contains(&doc.body.len()), "{doc:?}");
            assert!((0..100_000).contains(&doc.price), "{doc:?}");
            let n: u64 = doc.category.strip_prefix("cat-").unwrap().parse().unwrap();
            assert!(n < CATEGORIES && doc.category.len() == 6, "{doc:?}");
        }
    }

    /// A fixed document: a change of the generator (and so of what every seed means) fails here first.
    #[test]
    fn the_corpus_of_a_seed_is_pinned() {
        let doc = document(42, 0);
        assert_eq!(doc.id, "doc-00000000");
        assert_eq!(doc.title.join(" "), "semu tebu tibu zedi tara");
        assert_eq!((doc.category.as_str(), doc.price), ("cat-02", 43_302));
        let first = query(42, 3, 100);
        assert_eq!(
            (first.text.as_str(), first.category.as_deref()),
            ("fefe", Some("cat-02"))
        );
        let mut print = Fingerprint::default();
        for index in 0..100 {
            print.document(&document(42, index));
        }
        let mut again = Fingerprint::default();
        for index in 0..100 {
            again.document(&document(42, index));
        }
        assert_eq!(print, again);
        assert_eq!(print.hex(), PINNED_CORPUS);
        let mut queries = Fingerprint::default();
        for index in 0..100 {
            queries.query(&query(42, index, 100));
        }
        assert_eq!(queries.hex(), PINNED_QUERIES);
    }

    const PINNED_CORPUS: &str = "ec2ceb787982b863";
    const PINNED_QUERIES: &str = "ce5ad2d8c903b087";

    #[test]
    fn word_frequencies_are_skewed_toward_small_indexes() {
        let mut rng = Rng::new(9, 0);
        let draws = 20_000;
        let low = (0..draws)
            .filter(|_| rng.word_index() < VOCABULARY / 4)
            .count();
        // The smaller of two uniform draws is in the lowest quarter 7 times in 16; a uniform draw 4 in 16.
        assert!((8000..9500).contains(&low), "{low}");
    }

    #[test]
    fn the_query_kinds_rotate_and_each_has_its_form() {
        let docs = 50;
        let mut counts = BTreeMap::new();
        for index in 0..100 {
            let q = query(42, index, docs);
            assert_eq!(q, query(42, index, docs));
            assert_eq!(q.kind, QueryKind::ALL[usize::try_from(index).unwrap() % 5]);
            *counts.entry(q.kind).or_insert(0) += 1;
            let words: Vec<&str> = q.text.trim_matches('"').split(' ').collect();
            match q.kind {
                QueryKind::Term | QueryKind::Typo | QueryKind::Filter => {
                    assert_eq!(words.len(), 1, "{q:?}");
                }
                QueryKind::Terms => assert!(words.len() == 2 && !q.text.contains('"'), "{q:?}"),
                QueryKind::Phrase => {
                    assert!(q.text.starts_with('"') && q.text.ends_with('"'), "{q:?}");
                    assert_eq!(words.len(), 2, "{q:?}");
                }
            }
            assert_eq!(q.category.is_some(), q.kind == QueryKind::Filter, "{q:?}");
        }
        assert_eq!(counts.values().copied().collect::<Vec<_>>(), [20; 5]);
        assert_ne!(query(42, 0, docs), query(43, 0, docs));
        // A seed that shares bits with the query stream's constant (all of them): the stream is the seed mixed with
        // it, not merged into it.
        assert_eq!(query(u64::MAX, 0, docs).text, "pigi");
    }

    /// Each query but a `term` is drawn from one document of the corpus, which therefore matches it (a typo's
    /// document matches the word it was made from).
    #[test]
    fn a_query_is_drawn_from_a_document_of_the_corpus() {
        let docs = 30;
        let corpus: Vec<SyntheticDoc> = (0..docs).map(|i| document(7, i)).collect();
        for index in 0..200 {
            let q = query(7, index, docs);
            let words: Vec<&str> = q.text.trim_matches('"').split(' ').collect();
            let holds = |doc: &SyntheticDoc| match q.kind {
                QueryKind::Term => true,
                QueryKind::Terms => words.iter().all(|w| doc.body.iter().any(|b| b == w)),
                QueryKind::Phrase => doc.body.windows(2).any(|pair| pair == words.as_slice()),
                QueryKind::Filter => {
                    doc.body.iter().any(|b| b == words[0])
                        && Some(&doc.category) == q.category.as_ref()
                }
                QueryKind::Typo => doc.body.iter().any(|b| {
                    b.len() == words[0].len()
                        && b.bytes()
                            .zip(words[0].bytes())
                            .filter(|(x, y)| x != y)
                            .count()
                            == 1
                }),
            };
            assert!(corpus.iter().any(holds), "{q:?}");
        }
    }

    #[test]
    fn a_misspelling_changes_exactly_one_vowel() {
        let mut rng = Rng::new(3, 0);
        for index in 0..500 {
            let original = word(index * 7);
            let typo = misspelled(&original, &mut rng);
            assert_eq!(typo.len(), original.len());
            let changed: Vec<usize> = (0..typo.len())
                .filter(|i| typo.as_bytes()[*i] != original.as_bytes()[*i])
                .collect();
            assert_eq!(changed.len(), 1, "{original} -> {typo}");
            assert_eq!(changed[0] % 2, 1, "{original} -> {typo}");
            assert!(
                VOWELS.contains(&typo.as_bytes()[changed[0]]),
                "{original} -> {typo}"
            );
        }
    }

    #[test]
    fn a_fingerprint_separates_fields_and_orders() {
        let print = |fields: &[&str]| {
            let mut f = Fingerprint::default();
            for field in fields {
                f.field(field);
            }
            f.hex()
        };
        // FNV-1a's offset basis, with nothing added.
        assert_eq!(Fingerprint::default().hex(), "cbf29ce484222325");
        assert_ne!(print(&["ab", "c"]), print(&["a", "bc"]));
        assert_ne!(print(&["a", "b"]), print(&["b", "a"]));
        assert_eq!(print(&["a", "b"]), print(&["a", "b"]));
        assert_eq!(print(&["a"]).len(), 16);
        // FNV-1a of the one byte `a` (the published test vector).
        let mut f = Fingerprint::default();
        f.bytes(b"a");
        assert_eq!(f.hex(), "af63dc4c8601ec8c");
    }
}
