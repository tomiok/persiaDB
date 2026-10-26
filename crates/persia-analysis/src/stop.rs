//! Stopword filter and its embedded lists (SPEC §6.3.2).
//!
//! The lists are closed-class words, lowercase NFC with their accents, sorted by bytes: articles, prepositions,
//! conjunctions and negation in all of them; English is the classic 33-word list, while the others also hold
//! personal pronouns, possessives, demonstratives and the commonest forms of the auxiliaries (`ser`, `estar`,
//! `haber`; `être`, `avoir`; `sein`, `haben`, `werden`; `essere`, `avere`), and leave out words just as often used as
//! content words (`bajo`, `ante`, `era`; `or`, `car`, `été`; `kann`, `will`; `stato`, `più`, `sei`). They are part of the definition
//! of every analyzer that uses them: changing a list changes those analyzers' versions (SPEC §6.3).

use std::collections::HashSet;

use crate::token::TokenFilter;
use crate::token_stream::TokenStream;

/// English stopwords.
pub const ENGLISH_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

/// Spanish stopwords.
pub const SPANISH_STOPWORDS: &[&str] = &[
    "a", "al", "como", "con", "contra", "cuando", "de", "del", "desde", "donde", "durante", "e",
    "el", "ella", "ellas", "ellos", "en", "entre", "es", "esa", "esas", "ese", "eso", "esos",
    "esta", "estas", "este", "esto", "estos", "está", "fue", "ha", "han", "hasta", "hay", "la",
    "las", "le", "les", "lo", "los", "me", "mi", "mis", "muy", "más", "mí", "ni", "no", "nos",
    "nosotros", "o", "os", "para", "pero", "por", "porque", "que", "se", "ser", "si", "sin",
    "sino", "sobre", "son", "su", "sus", "sí", "también", "te", "ti", "tras", "tu", "tus", "tú",
    "u", "un", "una", "unas", "uno", "unos", "usted", "ustedes", "vosotros", "y", "ya", "yo", "él",
];

/// Portuguese stopwords.
pub const PORTUGUESE_STOPWORDS: &[&str] = &[
    "a", "ao", "aos", "as", "com", "como", "da", "das", "de", "do", "dos", "e", "ela", "elas",
    "ele", "eles", "em", "entre", "essa", "essas", "esse", "esses", "esta", "estas", "este",
    "estes", "está", "eu", "foi", "há", "isso", "isto", "já", "lhe", "lhes", "mais", "mas", "me",
    "meu", "minha", "muito", "na", "nas", "nem", "no", "nos", "num", "numa", "não", "nós", "o",
    "os", "ou", "para", "pela", "pelas", "pelo", "pelos", "por", "porque", "quando", "que", "se",
    "sem", "ser", "seu", "seus", "sua", "suas", "são", "também", "te", "tu", "um", "uma", "umas",
    "uns", "você", "vocês", "à", "às", "é",
];

/// French stopwords.
pub const FRENCH_STOPWORDS: &[&str] = &[
    "a", "ai", "au", "aux", "avait", "avec", "avez", "avons", "ce", "ceci", "cela", "celle",
    "celles", "celui", "ces", "cet", "cette", "ceux", "chez", "comme", "contre", "dans", "de",
    "depuis", "des", "donc", "dont", "du", "elle", "elles", "en", "entre", "es", "est", "et",
    "eux", "il", "ils", "je", "la", "le", "les", "leur", "leurs", "lorsque", "lui", "ma", "mais",
    "me", "mes", "moi", "mon", "ne", "ni", "non", "nos", "notre", "nous", "on", "ont", "ou", "où",
    "par", "pas", "pour", "puisque", "quand", "que", "qui", "sa", "sans", "se", "ses", "si",
    "sommes", "son", "sont", "sous", "suis", "sur", "ta", "te", "tes", "toi", "ton", "tu", "un",
    "une", "vers", "vos", "votre", "vous", "y", "à", "ça", "étaient", "était", "êtes",
];

/// German stopwords.
pub const GERMAN_STOPWORDS: &[&str] = &[
    "aber", "als", "am", "an", "auf", "aus", "bei", "beim", "bin", "bis", "bist", "das", "dass",
    "dein", "deine", "deinem", "deinen", "deiner", "dem", "den", "denn", "der", "des", "dich",
    "die", "diese", "diesem", "diesen", "dieser", "dieses", "dir", "du", "durch", "ein", "eine",
    "einem", "einen", "einer", "eines", "er", "es", "euch", "euer", "eure", "für", "gegen", "habe",
    "haben", "hat", "hatte", "ich", "ihm", "ihn", "ihnen", "ihr", "ihre", "ihrem", "ihren",
    "ihrer", "im", "in", "ins", "ist", "kein", "keine", "keinem", "keinen", "keiner", "man",
    "mein", "meine", "meinem", "meinen", "meiner", "mich", "mir", "mit", "nach", "nicht", "ob",
    "oder", "ohne", "sein", "seine", "seinem", "seinen", "seiner", "seit", "sich", "sie", "sind",
    "sondern", "um", "und", "uns", "unser", "unsere", "unserem", "unseren", "unserer", "unter",
    "vom", "von", "vor", "war", "waren", "was", "weil", "wenn", "wer", "werden", "wie", "wir",
    "wird", "wo", "wurde", "wurden", "zu", "zum", "zur", "über",
];

/// Italian stopwords.
pub const ITALIAN_STOPWORDS: &[&str] = &[
    "a", "abbiamo", "ad", "agli", "ai", "al", "alla", "alle", "allo", "anche", "avete", "che",
    "chi", "ci", "col", "come", "con", "cui", "da", "dagli", "dai", "dal", "dalla", "dalle",
    "dallo", "degli", "dei", "del", "della", "delle", "dello", "di", "dove", "e", "ed", "egli",
    "essa", "esse", "essere", "essi", "esso", "fra", "fu", "gli", "ha", "hai", "hanno", "ho", "i",
    "il", "in", "io", "la", "le", "lei", "lo", "loro", "lui", "ma", "me", "mi", "mia", "mie",
    "miei", "mio", "ne", "negli", "nei", "nel", "nella", "nelle", "nello", "noi", "non", "nostra",
    "nostre", "nostri", "nostro", "né", "o", "per", "perché", "quando", "quegli", "quei", "quel",
    "quella", "quelle", "quelli", "quello", "questa", "queste", "questi", "questo", "se", "si",
    "siamo", "siete", "sono", "su", "sua", "sue", "sugli", "sui", "sul", "sulla", "sulle", "sullo",
    "suo", "suoi", "te", "ti", "tra", "tu", "tua", "tue", "tuo", "tuoi", "un", "una", "uno", "vi",
    "voi", "vostra", "vostre", "vostri", "vostro", "è",
];

/// Removes tokens whose text is exactly one of a list of words (SPEC §6.3.2), leaving a gap in positions.
///
/// The embedded lists are lowercase with their accents, so the filter runs after
/// [`LowercaseFilter`](crate::LowercaseFilter) and before [`FoldingFilter`](crate::FoldingFilter); a custom list is
/// matched as given.
///
/// ```
/// use persia_analysis::{TokenStream, StopwordFilter, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let mut tokens = TokenStream::new();
/// WhitespaceTokenizer.tokenize("la casa de papel", &mut tokens);
/// StopwordFilter::spanish().apply(&mut tokens);
/// let kept: Vec<(&str, u32)> = tokens.iter().map(|t| (t.text.as_str(), t.position)).collect();
/// assert_eq!(kept, [("casa", 1), ("papel", 3)]);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StopwordFilter {
    words: HashSet<Box<str>>,
}

impl StopwordFilter {
    /// A filter removing the given words.
    pub fn new<W: Into<Box<str>>>(words: impl IntoIterator<Item = W>) -> Self {
        Self {
            words: words.into_iter().map(Into::into).collect(),
        }
    }

    /// The embedded English list, [`ENGLISH_STOPWORDS`].
    pub fn english() -> Self {
        Self::new(ENGLISH_STOPWORDS.iter().copied())
    }

    /// The embedded Spanish list, [`SPANISH_STOPWORDS`].
    pub fn spanish() -> Self {
        Self::new(SPANISH_STOPWORDS.iter().copied())
    }

    /// The embedded Portuguese list, [`PORTUGUESE_STOPWORDS`].
    pub fn portuguese() -> Self {
        Self::new(PORTUGUESE_STOPWORDS.iter().copied())
    }

    /// The embedded French list, [`FRENCH_STOPWORDS`].
    pub fn french() -> Self {
        Self::new(FRENCH_STOPWORDS.iter().copied())
    }

    /// The embedded German list, [`GERMAN_STOPWORDS`].
    pub fn german() -> Self {
        Self::new(GERMAN_STOPWORDS.iter().copied())
    }

    /// The embedded Italian list, [`ITALIAN_STOPWORDS`].
    pub fn italian() -> Self {
        Self::new(ITALIAN_STOPWORDS.iter().copied())
    }

    /// Whether `text` is one of the filter's words.
    pub fn is_stopword(&self, text: &str) -> bool {
        self.words.contains(text)
    }
}

impl TokenFilter for StopwordFilter {
    fn apply(&self, tokens: &mut TokenStream) {
        tokens.retain(|token| !token.text.is_empty() && !self.is_stopword(&token.text));
    }
}
