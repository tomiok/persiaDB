//! The filter step (SPEC §7.6, ROADMAP 8.2.6, ADR-0047): the cascade's last resort, planned only with
//! `filters: "last_resort"`.
//!
//! Its query is L0's with the request's *soft* filters dropped (ROADMAP 8.2.13, ADR-0056): a compiled query's
//! `soft_filter`, which the compilation keeps beside its query ([`Compiled::soft_filter`]), and the filters of a
//! programmatic root `bool` its caller marked soft ([`crate::db::RelaxedQuery::with_soft_filter`]). A request's
//! `filter` ([`Compiled::filter`]) is hard and never dropped, nor is an unmarked filter of a programmatic root: with no
//! soft filter there is no step. Whatever query a caller put in, the step drops the soft filters and nothing else. Everything else stays as L0 has
//! it, excluded clauses (`must_not`), a filter a caller's query holds itself, and any filter deeper in the query
//! included. The cascade builds it only while the earlier steps found fewer than `min_results`
//! hits, never to fill a page ([`LevelTransform::fills`]), and excludes the earlier steps from it as from any step,
//! so its hits are the documents L0's words match that the filters rejected.

use super::{LevelInput, LevelTransform, Step};
use crate::query::{BoolQuery, Compiled, Filter, Query};
use crate::segment::ReadError;

/// What the filter step lists in `applied` (SPEC §7.6).
pub(crate) const APPLIED: &str = "filters";

/// The filter step: one step, L0's query without its soft root filters, whose `effective` is L0's text (no word
/// changes). A query without a soft filter at its root, or without a word to match, builds none, so the level is
/// skipped.
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

/// L0's query with the soft filters ([`LevelInput::soft`], ROADMAP 8.2.13) dropped from its root `bool`, `None` when it
/// holds none, or the root has no word to match. A compiled query's soft filters are its request's `soft_filter`
/// ([`Compiled::soft_filter`]), which `Compiled::query` holds at its root `bool` whatever query a caller put in
/// (ROADMAP 8.2.14), with any a caller marked; a programmatic query's are those its caller marked. A hard filter (the
/// request's `filter`, [`Compiled::filter`], even if marked soft too, or an unmarked filter of a programmatic root) is
/// never dropped. A filter of the caller's own stays, unless it equals a soft filter, which counts as one (the
/// join does not add it twice, so the two cannot be told apart). When the join wrapped the caller's query (a root
/// with `should` clauses, `Compiled::wrapped`), that root is the caller's: the soft filters are dropped from it too,
/// or a clone of the compiled query with `should` clauses added would keep them. The
/// step keeps the rest of the root as L0 has it, a compiled query's clauses' parts included.
///
/// A root that searches no word builds no step. A clause searches a word when it is a `term`, `combined`, `phrase`,
/// `prefix` or `fuzzy`, or a `bool`, `boost` or `constant_score` that holds such a clause in `must` or `should`.
/// `match_all`, an empty `bool` and a `bool` of exclusions alone do not, at any depth: a root whose `must` and
/// `should` are only those (an empty `q`, stopwords only, excluded clauses alone, a filter-only `bool`,
/// `should: [match_all]`, `boost` or `constant_score` of `match_all`) would, without its filters, match every live
/// document the exclusions leave, at score 0, the unfiltered L5 ADR-0047 rejects.
fn unfiltered(input: &LevelInput<'_>) -> Option<Query> {
    let Query::Bool(root) = input.original else {
        return None;
    };
    let words = root.must.iter().any(has_word) || root.should.iter().any(has_word);
    // The soft filters (ROADMAP 8.2.13), but any that is a compiled query's hard filter too: hard wins.
    let hard = input.compiled.map_or(&[][..], Compiled::filter);
    let soft: Vec<&Filter> = input.soft.iter().filter(|f| !hard.contains(f)).collect();
    if !words || soft.is_empty() {
        return None;
    }
    let without = |root: &BoolQuery| BoolQuery {
        filter: root
            .filter
            .iter()
            .filter(|f| !soft.contains(f))
            .cloned()
            .collect(),
        ..root.clone()
    };
    let mut step = without(root);
    let mut dropped = step.filter.len() < root.filter.len();
    // The wrapped caller root, only while L0 is the compilation's own query, which holds it as its one `must`.
    if let Some(compiled) = input.compiled
        && let (Some(Query::Bool(caller)), [only]) = (compiled.wrapped(), step.must.as_mut_slice())
        && matches!(only, Query::Bool(b) if b == caller)
    {
        let inner = without(caller);
        dropped |= inner.filter.len() < caller.filter.len();
        *only = Query::Bool(inner);
    }
    // Nothing to drop (a programmatic root holding none of the soft filters): no step.
    dropped.then_some(Query::Bool(step))
}

/// Whether `query` matches by at least one term (SPEC §7.6). A term inside `must_not` does not count: exclusions are
/// not words the query searches. `match_all` and an empty `bool` match documents without naming a term, and so does a
/// `bool` whose only clauses are exclusions.
fn has_word(query: &Query) -> bool {
    match query {
        Query::Term(_)
        | Query::Combined(_)
        | Query::Phrase(_)
        | Query::Prefix(_)
        | Query::Fuzzy(_) => true,
        Query::MatchAll => false,
        Query::Boost(q) => has_word(&q.query),
        Query::ConstantScore(q) => has_word(&q.query),
        Query::Bool(q) => q.must.iter().any(has_word) || q.should.iter().any(has_word),
    }
}

#[cfg(test)]
mod tests;
