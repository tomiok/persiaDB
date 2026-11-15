//! The filter step (SPEC §7.6, ROADMAP 8.2.6, ADR-0047): the cascade's last resort, planned only with
//! `filters: "last_resort"`.
//!
//! Its query is L0's with the root `bool`'s filters dropped: a compiled query's request filter (SPEC §7.1), a
//! programmatic root `bool`'s `filter`. Everything else stays as L0 has it, excluded clauses (`must_not`) and any filter
//! deeper in the query included. The cascade builds it only while the earlier steps found fewer than `min_results`
//! hits, never to fill a page ([`LevelTransform::fills`]), and excludes the earlier steps from it as from any step,
//! so its hits are the documents L0's words match that the filters rejected.

use super::{LevelInput, LevelTransform, Step};
use crate::query::{BoolQuery, Compiled, Query};
use crate::segment::ReadError;

/// What the filter step lists in `applied` (SPEC §7.6).
pub(crate) const APPLIED: &str = "filters";

/// The filter step: one step, L0's query without its root filters, whose `effective` is L0's text (no word changes).
/// A query whose root has no filter builds none, so the level is skipped.
#[derive(Debug)]
pub(crate) struct Filters;

impl LevelTransform for Filters {
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        Ok(unfiltered(input)
            .map(|query| Step {
                query,
                applied: APPLIED.to_owned(),
                effective: input.compiled.map(Compiled::effective),
                alternatives: None,
                corrected: Vec::new(),
            })
            .into_iter()
            .collect())
    }

    /// A filtered page is never padded with documents its filters reject: the step runs only to reach `min_results`.
    fn fills(&self) -> bool {
        false
    }
}

/// L0's query with its root `bool`'s filters dropped, `None` when that root has none. The root is where L5 reads the
/// filters it keeps (SPEC §7.6): a compiled query's root around its clauses (ROADMAP 8.1.17), else a programmatic root
/// `bool`. A compiled query's root is its query, so the step keeps its clauses' parts as L0 has them.
fn unfiltered(input: &LevelInput<'_>) -> Option<Query> {
    let root = match (input.compiled, input.original) {
        (Some(compiled), _) => compiled.root(),
        (None, Query::Bool(root)) => Some(root),
        (None, _) => None,
    }?;
    (!root.filter.is_empty()).then(|| {
        Query::Bool(BoolQuery {
            filter: Vec::new(),
            ..root.clone()
        })
    })
}

#[cfg(test)]
mod tests;
