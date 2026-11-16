//! L5 `fallback` (SPEC §7.6, ROADMAP 8.2.5): the collection's own ranking of the documents no earlier step matched.
//!
//! The step's query only selects: L0's root `must_not` and `filter` around `match_all`, so excluded clauses stay
//! excluded and filters are not relaxed (the filter step after it relaxes them, ROADMAP 8.2.6). The cascade excludes
//! the earlier steps from it as from any step, and ranks its matches itself (`db::relaxed`): by static rank, then
//! recency.

use super::drop_terms::effective;
use super::{LevelInput, LevelTransform, Step};
use crate::query::{BoolQuery, Compiled, Filter, Query};
use crate::segment::ReadError;

/// What L5's step lists in `applied` (SPEC §7.6).
pub(crate) const APPLIED: &str = "fallback";

/// L5 `fallback`: one step, whose query is L0's exclusions and filters (SPEC §7.6). Its `effective` is the excluded
/// clauses alone, in text order (empty when there are none: no word is searched); it corrects nothing. Only the root
/// `bool`'s `must_not` and `filter` are kept, where the compiler puts excluded clauses and a request joins its filter
/// (SPEC §7.1), beside a compiled query's request filter ([`Compiled::filter`], ROADMAP 8.2.14), which it keeps
/// whatever query a caller put in. A query with a filter deeper in it (under a `boost`, or in a nested `bool`, as a
/// caller's query a compilation wraps to join its filter) falls back without that one, as SPEC §7.6 states.
#[derive(Debug)]
pub(crate) struct Fallback;

impl LevelTransform for Fallback {
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        // The root `bool` of L0's query, compiled or programmatic: no document L0 matches is one its `must_not`
        // matches or its `filter` rejects. Any other root has neither. A compiled query's request filter is read from
        // the compilation too (ROADMAP 8.2.14): `Compiled::query` holds it at its root whatever query a caller put in,
        // and reading it here keeps L5 from losing it even if the root did not.
        let root = match input.original {
            Query::Bool(root) => Some(root),
            _ => None,
        };
        let must_not: &[Query] = root.map_or(&[], |root| &root.must_not);
        let mut filter: Vec<Filter> = root.map_or_else(Vec::new, |root| root.filter.clone());
        for kept in input.compiled.map_or(&[][..], Compiled::filter) {
            if !filter.contains(kept) {
                filter.push(kept.clone());
            }
        }
        Ok(vec![Step {
            query: query(must_not, &filter),
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
