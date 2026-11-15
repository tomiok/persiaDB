//! L5 `fallback` (SPEC §7.6, ROADMAP 8.2.5): the collection's own ranking of the documents no earlier step matched.
//!
//! The step's query only selects: L0's root `must_not` and `filter` around `match_all`, so excluded clauses stay
//! excluded and filters are not relaxed (the filter step after it relaxes them, ROADMAP 8.2.6). The cascade excludes the earlier steps
//! from it as from any step, and ranks its matches itself (`db::relaxed`): by static rank, then recency.

use super::drop_terms::effective;
use super::{LevelInput, LevelTransform, Step};
use crate::query::{BoolQuery, Filter, Query};
use crate::segment::ReadError;

/// What L5's step lists in `applied` (SPEC §7.6).
pub(crate) const APPLIED: &str = "fallback";

/// L5 `fallback`: one step, whose query is L0's exclusions and filters (SPEC §7.6). Its `effective` is the excluded
/// clauses alone, in text order (empty when there are none: no word is searched); it corrects nothing. Only the root
/// `bool`'s `must_not` and `filter` are kept, where the compiler puts excluded clauses and a request joins its filter
/// (SPEC §7.1); a programmatic query with a filter deeper in it (under a `boost`, or in a nested `bool`) falls back without
/// it, as SPEC §7.6 states.
#[derive(Debug)]
pub(crate) struct Fallback;

impl LevelTransform for Fallback {
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        // A compiled query's root around its clauses (ROADMAP 8.1.17), or a programmatic root `bool`: any other root
        // has neither exclusions nor filters.
        let root = match (input.compiled, input.original) {
            (Some(compiled), _) => compiled.root(),
            (None, Query::Bool(root)) => Some(root),
            (None, _) => None,
        };
        let (must_not, filter): (&[Query], &[Filter]) =
            root.map_or((&[], &[]), |root| (&root.must_not, &root.filter));
        Ok(vec![Step {
            query: query(must_not, filter),
            applied: APPLIED.to_owned(),
            effective: input.compiled.map(|compiled| effective(compiled, &[])),
            alternatives: None,
            corrected: Vec::new(),
        }])
    }

    /// Every earlier step keeps L0's root `must_not` and `filter` (SPEC §7.6), so it matches only documents those keep:
    /// `match_all` under them matches all of them. A deeper filter of a programmatic query only narrows an earlier step.
    fn covers_earlier(&self) -> bool {
        true
    }
}

/// Every live document `filter` keeps and `must_not` does not match: `match_all` when there is neither.
pub(crate) fn query(must_not: &[Query], filter: &[Filter]) -> Query {
    if must_not.is_empty() && filter.is_empty() {
        return Query::MatchAll;
    }
    Query::Bool(BoolQuery {
        must: vec![Query::MatchAll],
        must_not: must_not.to_vec(),
        filter: filter.to_vec(),
        ..BoolQuery::default()
    })
}

#[cfg(test)]
mod tests;
