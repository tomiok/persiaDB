//! Query execution over one [`SearchView`] (SPEC §7.3, ROADMAP 7.4): a [`SearchPlan`] holds a query's scorers,
//! built once from the collection statistics; [`SearchPlan::matcher`] turns it into a tree of [`Matcher`]s over a
//! view (a segment or the memtable), and a [`TopK`] keeps the best hits.
//!
//! Matchers walk documents in docid order and skip with [`Matcher::advance`]: a term's postings, a conjunction
//! that leapfrogs its clauses from the cheapest, a disjunction, an exclusion, and a phrase that checks positions on
//! the documents holding all its terms. A `bool`'s filters become a bitmap per view, which its matcher keeps to
//! (SPEC §7.3). Views already leave deleted documents out (SPEC §8.3).
//!
//! ```
//! use std::sync::Arc;
//! use persia_analysis::AnalyzerRegistry;
//! use persia_engine::document::{Document, Value};
//! use persia_engine::query::{BoolQuery, Query, TermQuery};
//! use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
//! use persia_engine::search::SearchPlan;
//! use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
//! use persia_engine::stats::SegmentStatistics;
//! use persia_engine::view::SegmentView;
//! use persia_format::ByteSource;
//!
//! let schema = SchemaBuilder::new("c").field("body", FieldKind::Text(TextOptions::default())).build()?;
//! let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
//! let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
//! for (id, text) in [("a", "red shoes"), ("b", "red red hat"), ("c", "blue shoes")] {
//!     b.add(&Document::new(id).with(body, Value::String(text.into())))?;
//! }
//! let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
//! let segment = Arc::new(Segment::open(ByteSource::new(SegmentWriter::new(&b, meta).finish()?.into()), &schema)?);
//! let schema = Arc::new(schema);
//! let stats = SegmentStatistics::new(&schema, &[Arc::clone(&segment)]);
//!
//! let term = |t: &str| Query::Term(TermQuery { field: body, term: t.into() });
//! // red AND NOT hat
//! let query = Query::Bool(BoolQuery { must: vec![term("red")], must_not: vec![term("hat")], ..BoolQuery::default() });
//! let plan = SearchPlan::new(&query, &schema, &stats)?;
//! let hits = plan.top_k(&SegmentView::new(&segment, None), 10)?;
//! assert_eq!(hits.iter().map(|h| h.docid).collect::<Vec<_>>(), [0]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod block_max_wand;
mod boolean;
mod filter;
mod max_score;
mod phrase;
mod plan;
mod top_k;
mod word;

pub use plan::SearchPlan;
pub use top_k::{Hit, TopK};

use crate::query::QueryError;
use crate::schema::FieldId;
use crate::score::ScoreError;
use crate::segment::{ReadError, SectionKind, SegmentCorruption};
use crate::view::NormReader;
#[cfg(doc)]
use crate::view::SearchView;

/// How a top-k search skips documents that cannot enter it (SPEC §7.3, ROADMAP 7.4.4). Every strategy returns
/// the same hits, bit for bit: a score is always summed the same way, and a document is skipped only when an
/// upper bound of its score, summed in that same order, is at most the worst hit kept.
///
/// Pruning reaches the matchers a threshold on the whole score applies to: a term, the `should` clauses of a
/// `bool` without `must` (a filter scores 0, so with filters too), the `must` clause of a `bool` with one and no
/// `should` (filters or not), and what `must_not` or `boost` wraps. Conjunctions and `should` clauses beside `must`
/// are walked whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Pruning {
    /// Every match is scored.
    Exhaustive,
    /// `MaxScore`: a disjunction leaves out the clauses whose bounds together cannot beat the threshold, and only
    /// looks them up on the documents of the others; whole-list bounds (`max_tf`, `min_norm` of the term).
    MaxScore,
    /// Block-max WAND: a disjunction finds the first document whose clauses' bounds can beat the threshold, then
    /// checks the bounds of their postings blocks (the Skip entries, SPEC §5.3.2) before scoring it; a term skips
    /// the blocks that cannot.
    #[default]
    BlockMaxWand,
}

/// A bound of the score of every document a matcher matches from some target through `up_to`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreBound {
    /// The last docid the bound covers.
    pub up_to: u32,
    /// The bound, at least 0.
    pub score: f64,
}

/// `values` summed in order from `0.0`, as a disjunction or a conjunction sums its clauses' scores (SPEC §7.2).
/// Floating-point addition is monotonic in each argument, so with each value at least the score in its place
/// (0 for a clause that misses), the sum is at least the score: exactly, without rounding slack.
fn sum_in_order(values: impl IntoIterator<Item = f64>) -> f64 {
    values.into_iter().fold(0.0, |sum, v| sum + v)
}

/// Why a query cannot be planned.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SearchError {
    /// A scorer cannot be built: a field of the wrong type, parameters out of range, a statistic unreadable.
    #[error(transparent)]
    Score(#[from] ScoreError),
    /// A query the engine does not execute yet: `prefix` and `fuzzy` (their expansion over the collection's terms,
    /// ROADMAP 7.4.12).
    #[error("{what} queries are not executed yet")]
    Unsupported {
        /// The query.
        what: &'static str,
    },
    /// A query [`crate::query::Query::validate`] rejects and that would otherwise plan into different results: a
    /// phrase on a field without positions or with decreasing positions, a boost or constant score out of range.
    #[error(transparent)]
    Query(#[from] QueryError),
}

/// The documents a query matches in one view, in docid order, with their scores.
///
/// A new matcher is before its first document: [`Self::next`] or [`Self::advance`] moves it onto one. After the
/// last document, or an error from [`Self::next`] or [`Self::advance`], it is exhausted.
pub trait Matcher {
    /// An upper bound on the documents it matches, 0 only when it matches none; a conjunction leads with its
    /// cheapest clause.
    fn cost(&self) -> u64;

    /// The current document, `None` before the first and after the last.
    fn doc(&self) -> Option<u32>;

    /// Moves to the next document and returns it, or `None` at the end.
    ///
    /// # Errors
    /// A corruption of the view.
    fn next(&mut self) -> Result<Option<u32>, ReadError>;

    /// Moves to the first document at or after `target` and returns it, or `None` if there is none. Never moves
    /// back: when the current document is already at or after `target`, it stays.
    ///
    /// # Errors
    /// As [`Self::next`].
    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError>;

    /// The current document's score (SPEC §7.2); 0 when not on a document. Calling it twice gives the same score.
    ///
    /// # Errors
    /// As [`Self::next`].
    fn score(&mut self) -> Result<f64, ReadError>;

    /// An upper bound of [`Self::score`] on every document it matches in the view (SPEC §7.3): at least the score,
    /// exactly, so a document whose bound is at most a threshold cannot score above it.
    fn max_score(&self) -> f64;

    /// An upper bound of the score of each document it matches from `target` through [`ScoreBound::up_to`]; `None`
    /// when it matches none at or after `target`. Does not move the matcher. By default the whole view and
    /// [`Self::max_score`]; a term narrows it to its postings blocks holding the first document at or after `target`
    /// (block-max WAND, SPEC §5.3.2). `up_to` is at least `target`.
    ///
    /// # Errors
    /// As [`Self::next`]; the matcher is exhausted after one.
    fn block_bound(&mut self, _target: u32) -> Result<Option<ScoreBound>, ReadError> {
        Ok(Some(ScoreBound {
            up_to: u32::MAX,
            score: self.max_score(),
        }))
    }

    /// From now on, [`Self::next`] and [`Self::advance`] may skip documents that score at most `score`: they cannot
    /// enter a top-k whose worst hit scores `score`, because each hit it keeps has a lower docid and wins the tie
    /// (ROADMAP 7.4.4). A threshold at or below the one in force, or NaN, is ignored. By default nothing is skipped.
    fn set_min_competitive(&mut self, _score: f64) {}
}

impl<M: Matcher + ?Sized> Matcher for Box<M> {
    fn cost(&self) -> u64 {
        (**self).cost()
    }

    fn doc(&self) -> Option<u32> {
        (**self).doc()
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        (**self).next()
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        (**self).advance(target)
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        (**self).score()
    }

    fn max_score(&self) -> f64 {
        (**self).max_score()
    }

    fn block_bound(&mut self, target: u32) -> Result<Option<ScoreBound>, ReadError> {
        (**self).block_bound(target)
    }

    fn set_min_competitive(&mut self, score: f64) {
        (**self).set_min_competitive(score);
    }
}

/// Where a matcher is: the state [`Matcher::doc`] alone cannot tell apart (before the first, after the last).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pos {
    Unstarted,
    At(u32),
    Done,
}

impl Pos {
    fn doc(self) -> Option<u32> {
        match self {
            Self::At(d) => Some(d),
            Self::Unstarted | Self::Done => None,
        }
    }

    /// The target [`Matcher::next`] advances to; `None` when there is no next document.
    fn next_target(self) -> Option<u32> {
        match self {
            Self::Unstarted => Some(0),
            Self::At(d) => d.checked_add(1),
            Self::Done => None,
        }
    }

    fn of(doc: Option<u32>) -> Self {
        doc.map_or(Self::Done, Self::At)
    }
}

/// Runs `matcher` to its end and keeps its best `k` hits, best first (score descending, then docid ascending).
/// Once `k` are kept, the worst one's score goes to [`Matcher::set_min_competitive`] each time it rises, so a
/// pruning matcher can skip what could not replace it.
///
/// # Errors
/// A corruption of the view the matcher reads.
pub fn collect(matcher: &mut (impl Matcher + ?Sized), k: usize) -> Result<Vec<Hit>, ReadError> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let mut top = TopK::new(k);
    let mut threshold = None;
    while let Some(docid) = matcher.next()? {
        // The threshold only moves when a hit is kept; most are not, once the heap is warm.
        if top.push(docid, matcher.score()?)
            && let Some(t) = top.threshold()
            && threshold.is_none_or(|old| t > old)
        {
            threshold = Some(t);
            matcher.set_min_competitive(t);
        }
    }
    Ok(top.into_hits())
}

/// Raises the threshold `min` to `score`; `false`, leaving it, when `score` is NaN or not above it, so a
/// threshold only rises and a pruning matcher never needs to step back ([`Matcher::set_min_competitive`]).
fn raise(min: &mut Option<f64>, score: f64) -> bool {
    if score.is_nan() || min.is_some_and(|m| score <= m) {
        return false;
    }
    *min = Some(score);
    true
}

/// The norm byte of `docid` in `field`, checked against `min_norm`, the smallest norm of a postings block holding it
/// (SPEC §5.3.2): a norm below it scores above the bound pruning skips by, so a document scored and one skipped
/// would disagree. It is corruption, never a hit dropped silently (ROADMAP 7.4.10).
fn bounded_norm(
    norms: &mut impl NormReader,
    field: FieldId,
    docid: u32,
    min_norm: Option<u8>,
) -> Result<u8, ReadError> {
    let norm = norms.get(docid)?;
    if min_norm.is_some_and(|m| norm < m) {
        return Err(ReadError::Section {
            kind: SectionKind::Norms,
            field: field.get(),
            position: u64::from(docid),
            reason: SegmentCorruption::Invalid {
                what: "a norm below its postings block's min_norm",
            },
        });
    }
    Ok(norm)
}
