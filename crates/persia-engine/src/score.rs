//! BM25F scoring (SPEC §7.2, ROADMAP 7.3): one [`TermScorer`] per query word, plus the static rank.
//!
//! A [`TermScorer`] is built once per query from the collection statistics ([`crate::stats`]); it then scores a
//! document from its frequency and norm byte in each of the word's fields, with no other lookup: each field has a
//! table of its 256 norm bytes' factors `w_f / (1 − b_f + b_f · len/avglen_f)` (the "norm decode table"). The same
//! function, given a block's (or a term's) `max_tf` and `min_norm`, is an upper bound of every score in it, for
//! `MaxScore` and block-max WAND (SPEC §7.3): the score is computed in a form where every floating-point step is
//! monotonic, so the bound holds bit for bit, not just in exact arithmetic.
//!
//! ```
//! use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
//! use persia_engine::score::{FieldHit, FieldParams, TermScorer};
//! use persia_engine::stats::{FieldTotals, StatisticsProvider};
//! # use persia_engine::schema::FieldId;
//! # use persia_engine::segment::ReadError;
//! # struct Stats;
//! # impl StatisticsProvider for Stats {
//! #     fn doc_count(&self) -> u64 { 10 }
//! #     fn field(&self, _: FieldId) -> FieldTotals { FieldTotals { docs_with_field: 10, sum_field_len: 40 } }
//! #     fn df(&self, _: FieldId, _: &[u8]) -> Result<u64, ReadError> { Ok(2) }
//! # }
//!
//! let schema = SchemaBuilder::new("c").field("title", FieldKind::Text(TextOptions::default())).build()?;
//! let title = schema.field("title").map(|f| f.id()).ok_or("no field")?;
//! // 10 documents, 2 of them with the word, titles of 4 tokens on average (`Stats` above).
//! let params = FieldParams::of(&schema, title, None)?;
//! let scorer = TermScorer::new(&[params], Stats.df(title, b"shoe")?, &Stats)?;
//! let one = scorer.score(&[Some(FieldHit { tf: 1, norm: 4 })]);
//! let two = scorer.score(&[Some(FieldHit { tf: 2, norm: 4 })]);
//! assert!(0.0 < one && one < two && two < scorer.idf() * 2.2);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod static_rank;

pub use static_rank::{static_rank, static_rank_value};

use crate::query::{CombinedQuery, PhraseQuery, TermQuery};
use crate::schema::{CollectionSchema, Field, FieldId, FieldKind};
use crate::segment::norms;
use crate::segment::{BlockMax, ReadError, TermInfo};
use crate::stats::StatisticsProvider;

/// `k1` of a `keyword` field, which has none in the schema; its `b` is 0 (no norms) and its weight 1 (SPEC §7.2).
pub const KEYWORD_K1: f64 = 1.2;

/// The smallest length-normalization denominator: `1 − b + b · len/avglen` is 0 only for `b = 1` and an empty
/// field, which holds no term; this keeps a damaged norm byte from dividing by zero.
const MIN_DENOMINATOR: f64 = f64::EPSILON;

/// Why a scorer cannot be built.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScoreError {
    /// A field id that is not a `text` or `keyword` field of the schema (a `combined` takes `text` only).
    #[error("field id {field} cannot be scored as {what}")]
    Field {
        /// The field.
        field: FieldId,
        /// What it was scored as.
        what: &'static str,
    },
    /// Parameters outside their ranges (SPEC §6.2.2, §7.1): `0 < weight ≤ 1000`, `0 ≤ k1 ≤ 10`, `0 ≤ b ≤ 1`.
    #[error("field id {field}: BM25F parameters out of range")]
    Params {
        /// The field.
        field: FieldId,
    },
    /// No fields.
    #[error("a term scorer needs at least one field")]
    NoFields,
    /// Reading a statistic failed.
    #[error(transparent)]
    Read(#[from] ReadError),
}

/// How one field contributes to a BM25F term (SPEC §7.2): its weight `w_f`, `k1` and `b`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldParams {
    /// The field.
    pub field: FieldId,
    /// `w_f`: `0 < weight ≤ 1000`.
    pub weight: f64,
    /// `0 ≤ k1 ≤ 10`.
    pub k1: f64,
    /// `0 ≤ b ≤ 1`.
    pub b: f64,
}

impl FieldParams {
    /// The schema's parameters of `field`: a `text` field's `boost` (or `weight` instead, as a query's `fields`
    /// sets it), `k1` and `b`; a `keyword` field's weight 1 (`weight` is ignored: SPEC §7.2 fixes it), [`KEYWORD_K1`] and
    /// `b = 0`.
    ///
    /// # Errors
    /// [`ScoreError::Field`] if `field` is not a `text` or `keyword` field of `schema`.
    pub fn of(
        schema: &CollectionSchema,
        field: FieldId,
        weight: Option<f64>,
    ) -> Result<Self, ScoreError> {
        match schema.field_by_id(field).map(Field::kind) {
            Some(FieldKind::Text(o)) => Ok(Self {
                field,
                weight: weight.unwrap_or(o.boost),
                k1: o.k1,
                b: o.b,
            }),
            Some(FieldKind::Keyword(_)) => Ok(Self {
                field,
                weight: 1.0,
                k1: KEYWORD_K1,
                b: 0.0,
            }),
            _ => Err(ScoreError::Field {
                field,
                what: "a term",
            }),
        }
    }

    fn valid(&self) -> bool {
        self.weight > 0.0
            && self.weight <= 1000.0
            && (0.0..=10.0).contains(&self.k1)
            && (0.0..=1.0).contains(&self.b)
    }
}

/// A document's frequency of a field's term and its norm byte in the field (SPEC §5.3.4); or, as a bound, the
/// largest frequency and the smallest norm byte of a block or a term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldHit {
    /// The frequency.
    pub tf: u32,
    /// The norm byte; 0 for a `keyword` field.
    pub norm: u8,
}

impl From<BlockMax> for FieldHit {
    fn from(b: BlockMax) -> Self {
        Self {
            tf: b.max_tf,
            norm: b.min_norm,
        }
    }
}

impl From<&TermInfo> for FieldHit {
    fn from(info: &TermInfo) -> Self {
        Self {
            tf: info.max_tf(),
            norm: info.min_norm(),
        }
    }
}

/// The BM25F scorer of one query word over its fields (SPEC §7.2).
#[derive(Debug, Clone)]
pub struct TermScorer {
    fields: Vec<FieldId>,
    /// Per field, by norm byte: `w_f / (1 − b_f + b_f · len/avglen_f)`.
    factors: Vec<[f64; 256]>,
    idf: f64,
    k1: f64,
    /// `idf · (k1 + 1)`, the score's limit as `tf~` grows.
    top: f64,
    /// `top · k1`.
    top_k1: f64,
}

impl TermScorer {
    /// The scorer of a word over `fields` (in the order [`Self::score`] takes their hits) with document frequency
    /// `df`, under `stats`. `k1` is the fields' weighted mean `Σ w_f · k1_f / Σ w_f`: their common `k1` when they
    /// agree.
    ///
    /// # Errors
    /// [`ScoreError::NoFields`] for no fields, [`ScoreError::Params`] for a parameter out of range.
    pub fn new(
        fields: &[FieldParams],
        df: u64,
        stats: &(impl StatisticsProvider + ?Sized),
    ) -> Result<Self, ScoreError> {
        let first = fields.first().ok_or(ScoreError::NoFields)?;
        if let Some(bad) = fields.iter().find(|p| !p.valid()) {
            return Err(ScoreError::Params { field: bad.field });
        }
        let k1 = if fields.iter().all(|p| p.k1.to_bits() == first.k1.to_bits()) {
            first.k1
        } else {
            let weights: f64 = fields.iter().map(|p| p.weight).sum();
            fields.iter().map(|p| p.weight * p.k1).sum::<f64>() / weights
        };
        Ok(Self::with_idf(
            fields,
            k1,
            idf(stats.doc_count(), df),
            stats,
        ))
    }

    fn with_idf(
        fields: &[FieldParams],
        k1: f64,
        idf: f64,
        stats: &(impl StatisticsProvider + ?Sized),
    ) -> Self {
        let top = idf * (k1 + 1.0);
        Self {
            fields: fields.iter().map(|p| p.field).collect(),
            factors: fields
                .iter()
                .map(|p| factors(p, stats.field(p.field).avglen()))
                .collect(),
            idf,
            k1,
            top,
            top_k1: top * k1,
        }
    }

    /// The scorer of a `term` query: a `text` field's term as a `combined` over that field with its `boost`, a
    /// `keyword` field's with weight 1, `k1` [`KEYWORD_K1`] and `b = 0` (SPEC §7.2).
    ///
    /// # Errors
    /// As [`FieldParams::of`]; a failure to read `df`.
    pub fn for_term(
        query: &TermQuery,
        schema: &CollectionSchema,
        stats: &(impl StatisticsProvider + ?Sized),
    ) -> Result<Self, ScoreError> {
        let params = FieldParams::of(schema, query.field, None)?;
        let df = stats.df(query.field, query.term.as_bytes())?;
        Self::new(&[params], df, stats)
    }

    /// The scorer of a `combined` query, its fields in the query's order; `df` is the largest of its terms' (SPEC
    /// §7.2).
    ///
    /// # Errors
    /// [`ScoreError::Field`] for a field that is not a `text` field; as [`Self::new`]; a failure to read a `df`.
    pub fn for_combined(
        query: &CombinedQuery,
        schema: &CollectionSchema,
        stats: &(impl StatisticsProvider + ?Sized),
    ) -> Result<Self, ScoreError> {
        let mut params = Vec::with_capacity(query.fields.len());
        let mut df = 0_u64;
        for clause in &query.fields {
            if !matches!(
                schema.field_by_id(clause.field).map(Field::kind),
                Some(FieldKind::Text(_))
            ) {
                return Err(ScoreError::Field {
                    field: clause.field,
                    what: "a combined term",
                });
            }
            params.push(FieldParams::of(schema, clause.field, Some(clause.weight))?);
            df = df.max(stats.df(clause.field, clause.term.as_bytes())?);
        }
        Self::new(&params, df, stats)
    }

    /// The scorer of a `phrase` query: one term over its `text` field with the schema `boost`, scored from the
    /// phrase frequency, whose `idf` is the sum of its terms' (SPEC §7.2). A phrase of one term scores as a `term`
    /// query on it.
    ///
    /// # Errors
    /// [`ScoreError::Field`] for a field that is not a `text` field; [`ScoreError::Params`] for a `boost`, `k1`
    /// or `b` out of range; a failure to read a `df`.
    pub fn for_phrase(
        query: &PhraseQuery,
        schema: &CollectionSchema,
        stats: &(impl StatisticsProvider + ?Sized),
    ) -> Result<Self, ScoreError> {
        if !matches!(
            schema.field_by_id(query.field).map(Field::kind),
            Some(FieldKind::Text(_))
        ) {
            return Err(ScoreError::Field {
                field: query.field,
                what: "a phrase",
            });
        }
        let params = FieldParams::of(schema, query.field, None)?;
        if !params.valid() {
            return Err(ScoreError::Params { field: query.field });
        }
        let mut sum = 0.0;
        for t in &query.terms {
            sum += idf(stats.doc_count(), stats.df(query.field, t.term.as_bytes())?);
        }
        Ok(Self::with_idf(&[params], params.k1, sum, stats))
    }

    /// The fields, in the order [`Self::score`] takes their hits.
    pub fn fields(&self) -> &[FieldId] {
        &self.fields
    }

    /// `idf(t)` (SPEC §7.2).
    pub fn idf(&self) -> f64 {
        self.idf
    }

    /// The `k1` the word saturates with.
    pub fn k1(&self) -> f64 {
        self.k1
    }

    /// `w_f / max(1 − b_f + b_f · len/avglen_f, ε)` of field `slot` of [`Self::fields`] for norm byte `norm`;
    /// multiplying a field frequency by it gives that field's contribution to [`Self::tf`]. Returns `None` past the fields.
    pub fn factor(&self, slot: usize, norm: u8) -> Option<f64> {
        self.factors
            .get(slot)
            .and_then(|table| table.get(usize::from(norm)))
            .copied()
    }

    /// `tf~(t, d)` of SPEC §7.2: `hits[i]` is the document's hit in field `i` of [`Self::fields`], `None` (or a
    /// frequency of 0) where the field lacks the term. Hits past the fields are ignored; a shorter slice scores
    /// the fields it names.
    #[allow(clippy::indexing_slicing)] // a u8 indexes a 256-entry table
    pub fn tf(&self, hits: &[Option<FieldHit>]) -> f64 {
        // `fields` and `factors` are built together and stay aligned; a mismatch would score the wrong field.
        debug_assert_eq!(self.fields.len(), self.factors.len());
        self.factors
            .iter()
            .zip(hits)
            .filter_map(|(factors, hit)| hit.filter(|h| h.tf > 0).map(|h| (factors, h)))
            .map(|(factors, h)| f64::from(h.tf) * factors[usize::from(h.norm)])
            .sum()
    }

    /// Whether the document holds the word: some field of [`Self::fields`] has a frequency above 0.
    ///
    /// This is the match. [`Self::score`] can be `0.0` for a real match when `tf~` is positive but so far below
    /// `k1` that the monotonic form in [`Self::score_tf`] rounds to 0, or just below, and `.max(0.0)` clamps it
    /// (SPEC §7.2). A legal weight may be any finite value in `(0, 1000]`, so the score is not given a floor.
    pub fn matches(&self, hits: &[Option<FieldHit>]) -> bool {
        debug_assert_eq!(self.fields.len(), self.factors.len());
        hits.iter()
            .take(self.fields.len())
            .any(|hit| hit.is_some_and(|h| h.tf > 0))
    }

    /// The document's score for the word (SPEC §7.2); 0 when it matches in no field, and also 0 for a match whose
    /// `tf~` underflows the monotonic form (see [`Self::matches`]).
    ///
    /// Monotonic in every frequency and antitone in every norm byte, so given a block's or a term's bounds
    /// ([`FieldHit`] from a [`BlockMax`] or a [`TermInfo`]) it is an upper bound of the score of each document in
    /// it (ROADMAP 7.3.3).
    pub fn score(&self, hits: &[Option<FieldHit>]) -> f64 {
        self.score_tf(self.tf(hits))
    }

    /// The score of a document whose `tf~` is `tf`.
    ///
    /// `0.0` when `tf` is not positive, and also when a positive `tf` is so small that
    /// `idf·(k1+1) − idf·(k1+1)·k1 / (k1 + tf)` rounds to 0 or just below. The `.max(0.0)` keeps the result
    /// non-negative, which the term and block bounds rely on; it does not mean the document missed the term.
    pub fn score_tf(&self, tf: f64) -> f64 {
        if tf.is_nan() || tf <= 0.0 {
            return 0.0;
        }
        // idf · tf · (k1+1) / (k1+tf), rewritten so each step is monotonic in `tf`, so is the result.
        (self.top - self.top_k1 / (self.k1 + tf)).max(0.0)
    }

    /// The largest score the word can give any document: its limit `idf · (k1 + 1)`, as `tf~` grows.
    pub fn max_score(&self) -> f64 {
        self.top
    }
}

/// `idf(t) = ln(1 + (N − df + 0.5)/(df + 0.5))` (SPEC §7.2), with `df` at most `N`: always positive.
#[allow(clippy::cast_precision_loss)] // counts far below 2^52
pub fn idf(doc_count: u64, df: u64) -> f64 {
    let df = df.min(doc_count);
    let rest = doc_count.saturating_sub(df) as f64;
    let df = df as f64;
    ((rest + 0.5) / (df + 0.5)).ln_1p()
}

/// `w_f / max(1 − b_f + b_f · len/avglen_f, ε)` for every norm byte: non-increasing in the byte, since the decoded
/// length grows with it (SPEC §5.3.4).
fn factors(p: &FieldParams, avglen: f64) -> [f64; 256] {
    let mut table = [0.0; 256];
    for (norm, slot) in (0..=u8::MAX).zip(table.iter_mut()) {
        let len = f64::from(norms::decode(norm));
        let denominator = (1.0 - p.b + p.b * len / avglen).max(MIN_DENOMINATOR);
        *slot = p.weight / denominator;
    }
    table
}

#[cfg(test)]
mod tests;
