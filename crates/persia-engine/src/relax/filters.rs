//! The filter step (SPEC §7.6, ROADMAP 8.2.6, ADR-0047): the cascade's last resort, planned only with
//! `filters: "last_resort"`.
//!
//! Its query is L0's with the request's filters dropped: a programmatic root `bool`'s `filter`, or a compiled query's
//! request filter, which the compilation keeps beside its query ([`Compiled::filter`], ROADMAP 8.2.14, ADR-0048), so
//! that whatever query a caller put in, the step drops that filter and nothing else. Everything else stays as L0 has
//! it, excluded clauses (`must_not`), a filter a caller's query holds itself, and any filter deeper in the query
//! included. The cascade builds it only while the earlier steps found fewer than `min_results`
//! hits, never to fill a page ([`LevelTransform::fills`]), and excludes the earlier steps from it as from any step,
//! so its hits are the documents L0's words match that the filters rejected.

use super::{LevelInput, LevelTransform, Step};
use crate::query::{BoolQuery, Compiled, Query};
use crate::segment::ReadError;

/// What the filter step lists in `applied` (SPEC §7.6).
pub(crate) const APPLIED: &str = "filters";

/// The filter step: one step, L0's query without its root filters, whose `effective` is L0's text (no word changes).
/// A query whose root has no filter, or no word to match, builds none, so the level is skipped.
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

/// L0's query with the request's filters dropped from its root `bool`, `None` when there are none, or the root has no
/// word to match. Of a programmatic query, every filter of its root `bool`. Of a compiled one, its request filter
/// ([`Compiled::filter`]), which `Compiled::query` holds at its root `bool` whatever query a caller put in (ROADMAP
/// 8.2.14), and nothing else: a filter of the caller's own stays, unless it equals a request filter, which counts as
/// the request's (the join does not add it twice, so the two cannot be told apart). When the join wrapped the
/// caller's query (a root with `should` clauses, `Compiled::wrapped`), that root is the caller's: the request's
/// filters are dropped from it too, or a clone of the compiled query with `should` clauses added would keep them. The
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
    let Some(compiled) = input.compiled else {
        return (words && !root.filter.is_empty()).then(|| {
            Query::Bool(BoolQuery {
                filter: Vec::new(),
                ..root.clone()
            })
        });
    };
    let request = compiled.filter();
    if !words || request.is_empty() {
        return None;
    }
    let without = |root: &BoolQuery| BoolQuery {
        filter: root
            .filter
            .iter()
            .filter(|f| !request.contains(f))
            .cloned()
            .collect(),
        ..root.clone()
    };
    let mut step = without(root);
    // The wrapped caller root, only while L0 is the compilation's own query, which holds it as its one `must`.
    if let (Some(Query::Bool(caller)), [only]) = (compiled.wrapped(), step.must.as_mut_slice())
        && matches!(only, Query::Bool(b) if b == caller)
    {
        *only = Query::Bool(without(caller));
    }
    Some(Query::Bool(step))
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
