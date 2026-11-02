//! Reference BM25F (SPEC §7.2) over plain documents: each document is its tokens per field, as the analyzer would
//! give them. Statistics are counted from the documents, field lengths quantized by the engine's norm codec (checked
//! here against the properties SPEC §5.3.4 gives it, not copied), and the score evaluated as the SPEC writes the
//! formula.

#![allow(dead_code, clippy::cast_precision_loss, clippy::many_single_char_names)]

/// One field of the reference: its BM25F parameters; `keyword` fields have no norms (`b = 0`).
#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub k1: f64,
    pub b: f64,
    pub keyword: bool,
}

/// A document: its tokens (a `keyword` field: its values) per field, by field index.
pub type Doc = Vec<Vec<String>>;

/// The documents the statistics count: every document of every segment, deleted or not, and the memtable's live
/// documents (SPEC §7.2).
#[derive(Debug, Default)]
pub struct Corpus {
    pub fields: Vec<Field>,
    pub counted: Vec<Doc>,
}

/// One field of a query word: its index, its term and its weight `w_f`.
#[derive(Debug, Clone)]
pub struct Clause {
    pub field: usize,
    pub term: String,
    pub weight: f64,
}

impl Corpus {
    pub fn n(&self) -> u64 {
        self.counted.len() as u64
    }

    pub fn df(&self, field: usize, term: &str) -> u64 {
        self.counted
            .iter()
            .filter(|d| d[field].iter().any(|t| t == term))
            .count() as u64
    }

    pub fn avglen(&self, field: usize) -> f64 {
        let with: Vec<&Doc> = self
            .counted
            .iter()
            .filter(|d| !d[field].is_empty())
            .collect();
        if with.is_empty() {
            return 1.0;
        }
        let sum: usize = with.iter().map(|d| d[field].len()).sum();
        sum as f64 / with.len() as f64
    }

    /// The score of `doc` for the word `clauses` (SPEC §7.2): `df` is the largest of the clauses', `k1` the weighted
    /// mean of the fields'.
    pub fn score(&self, clauses: &[Clause], doc: &Doc) -> f64 {
        let n = self.n();
        let df = clauses
            .iter()
            .map(|c| self.df(c.field, &c.term))
            .max()
            .unwrap_or(0)
            .min(n);
        let idf = (1.0 + (n as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
        let wsum: f64 = clauses.iter().map(|c| c.weight).sum();
        let k1 = clauses
            .iter()
            .map(|c| c.weight * self.fields[c.field].k1)
            .sum::<f64>()
            / wsum;
        let mut tf_tilde = 0.0;
        for c in clauses {
            let field = self.fields[c.field];
            let tokens = &doc[c.field];
            let tf = tokens.iter().filter(|t| **t == c.term).count();
            if tf == 0 {
                continue;
            }
            let b = if field.keyword { 0.0 } else { field.b };
            let len = quantized(tokens.len() as u64) as f64;
            tf_tilde += c.weight * tf as f64 / (1.0 - b + b * len / self.avglen(c.field));
        }
        if tf_tilde == 0.0 {
            return 0.0;
        }
        idf * tf_tilde * (k1 + 1.0) / (k1 + tf_tilde)
    }
}

/// The length a field of `n` tokens is scored with. The engine's norm codec decides it (SPEC §5.3.4,
/// `persia_engine::segment::scored_length`); the reference does not copy the codec, it checks the properties the SPEC
/// gives it (the tests below), so the scores here and in the engine use the same lengths without the reference
/// repeating a bug of the codec.
pub fn quantized(n: u64) -> u64 {
    u64::from(persia_engine::segment::scored_length(
        u32::try_from(n).unwrap_or(u32::MAX),
    ))
}

/// SPEC §5.3.4 by hand: lengths below 40 are exact; 41 − 24 = 17 = `0b1_0001` keeps `0b1000 << 1` = 16, so 40;
/// 100 − 24 = 76 = `0b100_1100` keeps `0b1001 << 3` = 72, so 96.
#[test]
fn quantization_by_hand() {
    assert_eq!(quantized(23), 23);
    assert_eq!(quantized(39), 39);
    assert_eq!(quantized(41), 40);
    assert_eq!(quantized(100), 96);
}

/// The properties below, for every length up to 2^17, where real fields are: random lengths up to 2^31 would rarely
/// land on one.
#[test]
fn quantization_keeps_the_spec_properties_for_every_short_length() {
    let mut previous = 0;
    for n in 0..1u64 << 17 {
        let q = quantized(n);
        assert!(q <= n, "{n} scores as {q}");
        if n < 40 {
            assert_eq!(q, n);
        }
        assert!(
            (n - q) * 8 < n.max(1),
            "{n} scores as {q}: 12.5 % or more low"
        );
        assert!(q >= previous, "{n} scores shorter than {}", n - 1);
        previous = q;
    }
}

proptest::proptest! {
    /// SPEC §5.3.4: the scored length never exceeds the field's, is exact below 40, is less than 12.5 % low, and
    /// keeps the order of lengths.
    #[test]
    fn quantization_keeps_the_spec_properties(a in 0u64..1 << 31, b in 0u64..1 << 31) {
        for n in [a, b] {
            let q = quantized(n);
            proptest::prop_assert!(q <= n, "{n} scores as {q}");
            if n < 40 {
                proptest::prop_assert_eq!(q, n);
            }
            proptest::prop_assert!((n - q) * 8 < n.max(1), "{n} scores as {q}: 12.5 % or more low");
        }
        let (lo, hi) = (a.min(b), a.max(b));
        proptest::prop_assert!(quantized(lo) <= quantized(hi), "{lo} scores longer than {hi}");
    }
}
