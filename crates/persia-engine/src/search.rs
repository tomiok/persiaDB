//! Query execution over one [`SearchView`] (SPEC §7.3, ROADMAP 7.4): a [`SearchPlan`] holds a query's scorers,
//! built once from the collection statistics; [`SearchPlan::matcher`] turns it into a tree of [`Matcher`]s over a
//! view (a segment or the memtable), and a [`TopK`] keeps the best hits.
//!
//! Matchers walk documents in docid order and skip with [`Matcher::advance`]: a term's postings, a conjunction
//! that leapfrogs its clauses from the cheapest, a disjunction, an exclusion, and a phrase that checks positions on
//! the documents holding all its terms. Views already leave deleted documents out (SPEC §8.3).
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

mod boolean;
mod phrase;
mod plan;
mod top_k;
mod word;

pub use plan::SearchPlan;
pub use top_k::{Hit, TopK};

use crate::query::QueryError;
use crate::score::ScoreError;
use crate::segment::ReadError;
#[cfg(doc)]
use crate::view::SearchView;

/// Why a query cannot be planned.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SearchError {
    /// A scorer cannot be built: a field of the wrong type, parameters out of range, a statistic unreadable.
    #[error(transparent)]
    Score(#[from] ScoreError),
    /// A query the engine does not execute yet: `prefix` and `fuzzy` (term enumeration, ROADMAP 6.1.5) and `bool`
    /// filters (ROADMAP 7.4.5).
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
///
/// # Errors
/// A corruption of the view the matcher reads.
pub fn collect(matcher: &mut (impl Matcher + ?Sized), k: usize) -> Result<Vec<Hit>, ReadError> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let mut top = TopK::new(k);
    while let Some(docid) = matcher.next()? {
        top.push(docid, matcher.score()?);
    }
    Ok(top.into_hits())
}
