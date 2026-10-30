//! Reference query evaluation (SPEC §7.1, §7.2) over plain documents: whether a document matches a query and its
//! score, by scanning its tokens. A token's position is its index in its field.

#![allow(dead_code, clippy::cast_precision_loss)]

use super::bm25f::{Clause, Corpus, Doc, quantized};

/// A query of the reference, by field index.
#[derive(Debug, Clone)]
pub enum Query {
    /// One word over one or more fields (a `term` or a `combined`).
    Word(Vec<Clause>),
    /// Terms at relative positions of one `text` field, scored with weight `weight` (the schema `boost`).
    Phrase {
        field: usize,
        terms: Vec<(String, u32)>,
        weight: f64,
    },
    Bool {
        must: Vec<Query>,
        should: Vec<Query>,
        must_not: Vec<Query>,
    },
    Boost(Box<Query>, f64),
    Constant(Box<Query>, f64),
    All,
}

/// `idf(t)` as SPEC §7.2 writes it.
pub fn idf(n: u64, df: u64) -> f64 {
    let df = df.min(n);
    (1.0 + (n as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln()
}

/// How often the phrase occurs in `tokens`: the starts `s` with each term at `s` plus its position relative to the
/// first term.
pub fn phrase_freq(tokens: &[String], terms: &[(String, u32)]) -> usize {
    let Some(first) = terms.first().map(|t| t.1) else {
        return 0;
    };
    (0..tokens.len())
        .filter(|&s| {
            terms.iter().all(|(t, p)| {
                tokens
                    .get(s + (*p - first) as usize)
                    .is_some_and(|tok| tok == t)
            })
        })
        .count()
}

impl Corpus {
    /// `Some((score, scale))` when `doc` matches `q`; `scale` is the sum of the largest scores of the query's scored
    /// leaves (`idf·(k1+1)`, times boosts), what the engine's rounding is relative to.
    pub fn eval(&self, q: &Query, doc: &Doc) -> Option<(f64, f64)> {
        match q {
            Query::Word(clauses) => {
                let held = clauses.iter().any(|c| doc[c.field].contains(&c.term));
                held.then(|| (self.score(clauses, doc), self.word_top(clauses)))
            }
            Query::Phrase {
                field,
                terms,
                weight,
            } => {
                let freq = phrase_freq(&doc[*field], terms);
                if freq == 0 {
                    return None;
                }
                let f = self.fields[*field];
                let idf: f64 = terms
                    .iter()
                    .map(|(t, _)| idf(self.n(), self.df(*field, t)))
                    .sum();
                let len = quantized(doc[*field].len() as u64) as f64;
                let tf = weight * freq as f64 / (1.0 - f.b + f.b * len / self.avglen(*field));
                let score = idf * tf * (f.k1 + 1.0) / (f.k1 + tf);
                Some((score, idf * (f.k1 + 1.0)))
            }
            Query::Bool {
                must,
                should,
                must_not,
            } => {
                if must_not.iter().any(|c| self.eval(c, doc).is_some()) {
                    return None;
                }
                let mut sum = (0.0, 0.0);
                for c in must {
                    let (s, scale) = self.eval(c, doc)?;
                    sum = (sum.0 + s, sum.1 + scale);
                }
                let mut any = false;
                for c in should {
                    if let Some((s, scale)) = self.eval(c, doc) {
                        any = true;
                        sum = (sum.0 + s, sum.1 + scale);
                    }
                }
                (any || !must.is_empty() || should.is_empty()).then_some(sum)
            }
            Query::Boost(inner, boost) => self
                .eval(inner, doc)
                .map(|(s, scale)| (s * boost, scale * boost)),
            Query::Constant(inner, score) => self.eval(inner, doc).map(|_| (*score, *score)),
            Query::All => Some((0.0, 0.0)),
        }
    }

    fn word_top(&self, clauses: &[Clause]) -> f64 {
        let df = clauses
            .iter()
            .map(|c| self.df(c.field, &c.term))
            .max()
            .unwrap_or(0);
        let wsum: f64 = clauses.iter().map(|c| c.weight).sum();
        let k1 = clauses
            .iter()
            .map(|c| c.weight * self.fields[c.field].k1)
            .sum::<f64>()
            / wsum;
        idf(self.n(), df) * (k1 + 1.0)
    }
}

#[test]
fn phrase_freq_by_hand() {
    let toks: Vec<String> = "a b a b a".split(' ').map(str::to_owned).collect();
    let t = |s: &str, p| (s.to_owned(), p);
    assert_eq!(phrase_freq(&toks, &[t("a", 0), t("b", 1)]), 2);
    assert_eq!(phrase_freq(&toks, &[t("a", 0), t("a", 2)]), 2);
    assert_eq!(phrase_freq(&toks, &[t("b", 3), t("b", 5)]), 1);
    assert_eq!(phrase_freq(&toks, &[t("b", 0), t("a", 0)]), 0);
}
