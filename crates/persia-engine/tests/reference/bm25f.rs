//! Reference BM25F (SPEC §7.2) over plain documents: each document is its tokens per field, as the analyzer would
//! give them. Statistics are counted from the documents, field lengths quantized as SPEC §5.3.4 describes, and the
//! score evaluated as the SPEC writes the formula.

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

/// The length a field of `n` tokens is scored with: SPEC §5.3.4's encoding of `n`, decoded.
pub fn quantized(n: u64) -> u64 {
    if n < 24 {
        return n;
    }
    let x = n.min((1 << 31) - 1) - 24;
    let k = 64 - u64::from(x.leading_zeros());
    let c = if k < 4 {
        x
    } else {
        ((x >> (k - 4)) & 7) | ((k - 3) << 3)
    };
    let v = if c < 8 {
        c
    } else {
        ((c & 7) | 8) << ((c >> 3) - 1)
    };
    24 + v
}

#[test]
fn quantization_by_hand() {
    assert_eq!(quantized(23), 23);
    assert_eq!(quantized(39), 39);
    assert_eq!(quantized(41), 40);
    assert_eq!(quantized(100), 96);
}
