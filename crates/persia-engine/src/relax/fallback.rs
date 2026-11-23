//! L5 `fallback` (SPEC §7.6, ROADMAP 8.2.5): the collection's own ranking of the documents no earlier step matched.
//!
//! The step's query only selects: L0's root `must_not` and `filter` around `match_all`, so excluded clauses stay
//! excluded and filters are not relaxed (the filter step after it drops soft ones alone, ROADMAP 8.2.6, 8.2.13). The
//! cascade excludes the earlier steps from it as from any step, and ranks its matches itself (`db::relaxed`): by static
//! rank, then recency. A filter L0 holds that the step cannot keep (one below the root, in a clause the step drops)
//! means no step: L5 never matches a document L0's filters reject (ROADMAP 8.2.13).

use super::drop_terms::effective;
use super::{LevelInput, LevelTransform, Step};
use crate::query::{BoolQuery, Compiled, Filter, Query};
use crate::segment::ReadError;

/// What L5's step lists in `applied` (SPEC §7.6).
pub(crate) const APPLIED: &str = "fallback";

/// L5 `fallback`: one step, whose query is L0's exclusions and filters (SPEC §7.6). Its `effective` is the excluded
/// clauses alone, in text order (empty when there are none: no word is searched); it corrects nothing. It keeps the
/// root `bool`'s `must_not` and `filter`, where the compiler puts excluded clauses and a request joins its filters (SPEC
/// §7.1), a compiled query's request filters ([`Compiled::filter`], [`Compiled::soft_filter`], ROADMAP 8.2.13, 8.2.14),
/// which it keeps whatever query a caller put in, the `must_not` and `filter` of a caller's root the join wrapped
/// ([`Compiled::wrapped`]), and the required clauses a caller added ([`Compiled::extra_must`], ROADMAP 8.2.15). The
/// clauses it drops (the root's other `must` and its `should`, the wrapped root's) must hold no filter: a filter there
/// (a programmatic `boost` of a filtered `bool`, a filtered `bool` as a `must` clause) is one `match_all` would not
/// apply, so the level then builds no step (ROADMAP 8.2.13).
#[derive(Debug)]
pub(crate) struct Fallback;

impl LevelTransform for Fallback {
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        Ok(selection(input)
            .map(|query| Step {
                query,
                applied: APPLIED.to_owned(),
                effective: input.compiled.map(|compiled| effective(compiled, &[])),
                alternatives: None,
                corrected: Vec::new(),
            })
            .into_iter()
            .collect())
    }

    /// Every earlier step keeps L0's root `must_not` and `filter` (SPEC §7.6), so it matches only documents those keep:
    /// `match_all` under them matches all of them. A query with a filter below its root builds no L5 step.
    fn covers_earlier(&self) -> bool {
        true
    }
}

/// L5's query for `input`: every live document L0's exclusions, filters and a caller's required clauses keep, `None`
/// when L0 holds a filter in a clause L5 drops ([`Fallback`]).
fn selection(input: &LevelInput<'_>) -> Option<Query> {
    // Required clauses a caller added beside a compiled query's clauses stay required (ROADMAP 8.2.15): the root's last
    // `must` entries.
    let must = input.compiled.map_or(&[][..], Compiled::extra_must);
    // The root `bool` of L0's query, compiled or programmatic: no document L0 matches is one its `must_not` matches
    // or its `filter` rejects. Any other root has neither, and is dropped whole.
    let Query::Bool(root) = input.original else {
        return (!holds_filter(input.original)).then(|| query(must, &[], &kept(input, &[])));
    };
    // A caller's root `bool` the join wrapped, L0's one `must` while L0 is the compilation's own query (ROADMAP
    // 8.2.13): its exclusions and filters are L0's too, and only its other clauses are dropped.
    let caller = input
        .compiled
        .and_then(Compiled::wrapped)
        .filter(|w| {
            root.must
                .first()
                .is_some_and(|first| std::ptr::eq(first, *w))
        })
        .and_then(|w| match w {
            Query::Bool(caller) => Some(caller),
            _ => None,
        });
    let own = root
        .must
        .get(..root.must.len().saturating_sub(must.len()))
        .unwrap_or(&[]);
    let mut dropped = own
        .get(usize::from(caller.is_some())..)
        .unwrap_or(&[])
        .iter()
        .chain(&root.should)
        .chain(
            caller
                .into_iter()
                .flat_map(|c| c.must.iter().chain(&c.should)),
        );
    if dropped.any(holds_filter) {
        return None;
    }
    let mut must_not = root.must_not.clone();
    let mut filter = root.filter.clone();
    if let Some(caller) = caller {
        must_not.extend(caller.must_not.iter().cloned());
        for f in &caller.filter {
            if !filter.contains(f) {
                filter.push(f.clone());
            }
        }
    }
    Some(query(must, &must_not, &kept(input, &filter)))
}

/// `filter` with a compiled query's request filters, hard and soft, after it, each once: read from the compilation too
/// (ROADMAP 8.2.14), so L5 keeps them even if the root did not. L5 relaxes no filter (ROADMAP 8.2.13).
fn kept(input: &LevelInput<'_>, filter: &[Filter]) -> Vec<Filter> {
    let mut out = filter.to_vec();
    let request = input
        .compiled
        .map(|c| c.filter().iter().chain(c.soft_filter()))
        .into_iter()
        .flatten();
    for f in request {
        if !out.contains(f) {
            out.push(f.clone());
        }
    }
    out
}

/// Whether `query` holds a filter at any depth: a `bool` with one, or a clause of one (its `must_not` included) that
/// holds one. A level that drops such a clause would loosen that filter (ROADMAP 8.2.13).
pub(super) fn holds_filter(query: &Query) -> bool {
    match query {
        Query::Bool(b) => {
            !b.filter.is_empty()
                || b.must
                    .iter()
                    .chain(&b.should)
                    .chain(&b.must_not)
                    .any(holds_filter)
        }
        Query::Boost(q) => holds_filter(&q.query),
        Query::ConstantScore(q) => holds_filter(&q.query),
        Query::Term(_)
        | Query::Combined(_)
        | Query::Phrase(_)
        | Query::Prefix(_)
        | Query::Fuzzy(_)
        | Query::MatchAll => false,
    }
}

/// Every live document `must` requires (every one when it is empty), `filter` keeps and `must_not` does not match:
/// `match_all` when there is none of them.
pub(crate) fn query(must: &[Query], must_not: &[Query], filter: &[Filter]) -> Query {
    if must.is_empty() && must_not.is_empty() && filter.is_empty() {
        return Query::MatchAll;
    }
    Query::Bool(BoolQuery {
        must: if must.is_empty() {
            vec![Query::MatchAll]
        } else {
            must.to_vec()
        },
        must_not: must_not.to_vec(),
        filter: filter.to_vec(),
        ..BoolQuery::default()
    })
}

#[cfg(test)]
mod tests;
