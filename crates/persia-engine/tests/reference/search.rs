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

/// `λ` of the proximity bonus (SPEC §7.2, ADR-0036).
pub const LAMBDA: f64 = 1.0;
/// The largest distance a pair's closeness counts.
pub const WINDOW: usize = 5;

/// The least distance between an occurrence of `a` and one of `b` in `tokens`, by brute force over every pair of
/// positions (a token's index): `q − p` when `q > p`, else `p − q + 1`; an occurrence is not paired with itself.
pub fn least_distance(tokens: &[String], a: &str, b: &str) -> Option<usize> {
    let at = |t: &str| -> Vec<usize> { (0..tokens.len()).filter(|&i| tokens[i] == t).collect() };
    let (ps, qs) = (at(a), at(b));
    let mut best: Option<usize> = None;
    for &p in &ps {
        for &q in &qs {
            if a == b && p == q {
                continue;
            }
            let d = if q > p { q - p } else { p - q + 1 };
            best = Some(best.map_or(d, |x| x.min(d)));
        }
    }
    best
}

impl Corpus {
    /// `idf` of a word (`df` the largest of its clauses').
    fn word_idf(&self, clauses: &[Clause]) -> f64 {
        let df = clauses
            .iter()
            .map(|c| self.df(c.field, &c.term))
            .max()
            .unwrap_or(0);
        idf(self.n(), df)
    }

    /// The proximity bonus of `must` in `doc` and its bound (SPEC §7.2): per pair of adjacent words, with the fields
    /// with positions where both have a clause, `λ · min(idf)` times the best field's `1 / d²` (0 beyond the
    /// window), summed in pair order; the bound sums the weights. A pair without such a field is left out.
    pub fn bonus(&self, must: &[Query], doc: &Doc) -> (f64, f64) {
        let (mut bonus, mut bound) = (0.0, 0.0);
        for w in must.windows(2) {
            let (Query::Word(a), Query::Word(b)) = (&w[0], &w[1]) else {
                continue;
            };
            let fields: Vec<(usize, &str, &str)> = a
                .iter()
                .filter(|c| !self.fields[c.field].keyword && self.fields[c.field].positions)
                .filter_map(|c| {
                    b.iter()
                        .find(|d| d.field == c.field)
                        .map(|d| (c.field, c.term.as_str(), d.term.as_str()))
                })
                .collect();
            if fields.is_empty() {
                continue;
            }
            let weight = LAMBDA * self.word_idf(a).min(self.word_idf(b));
            let closeness = fields
                .iter()
                .filter_map(|&(f, ta, tb)| least_distance(&doc[f], ta, tb))
                .filter(|&d| d <= WINDOW)
                .map(|d| 1.0 / (d as f64 * d as f64))
                .fold(0.0, f64::max);
            bonus += weight * closeness;
            bound += weight;
        }
        (bonus, bound)
    }

    /// `Some((score, scale))` when `doc` matches `q`, with the proximity bonus (SPEC §7.2); `scale` is the sum of
    /// the largest scores of the query's scored leaves (`idf·(k1+1)`, times boosts, and each bonus's bound), what
    /// the engine's rounding is relative to.
    pub fn eval(&self, q: &Query, doc: &Doc) -> Option<(f64, f64)> {
        self.eval_with(q, doc, true)
    }

    /// As [`Self::eval`], with or without the proximity bonus.
    pub fn eval_with(&self, q: &Query, doc: &Doc, proximity: bool) -> Option<(f64, f64)> {
        let eval = |q| self.eval_with(q, doc, proximity);
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
                if must_not.iter().any(|c| eval(c).is_some()) {
                    return None;
                }
                let mut sum = (0.0, 0.0);
                for c in must {
                    let (s, scale) = eval(c)?;
                    sum = (sum.0 + s, sum.1 + scale);
                }
                if proximity {
                    let (bonus, bound) = self.bonus(must, doc);
                    sum = (sum.0 + bonus, sum.1 + bound);
                }
                let mut any = false;
                for c in should {
                    if let Some((s, scale)) = eval(c) {
                        any = true;
                        sum = (sum.0 + s, sum.1 + scale);
                    }
                }
                (any || !must.is_empty() || should.is_empty()).then_some(sum)
            }
            Query::Boost(inner, boost) => eval(inner).map(|(s, scale)| (s * boost, scale * boost)),
            Query::Constant(inner, score) => eval(inner).map(|_| (*score, *score)),
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
