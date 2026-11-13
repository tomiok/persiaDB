//! L3 `prefix` (SPEC §7.6, ROADMAP 8.2.3): the query with its words also matching the terms they start.
//!
//! One step, listed as `prefix`, built from L0's query: the last term, and every other term of at least
//! [`MIN_LEN`] characters, becomes a `prefix` query of [`DEFAULT_MAX_EXPANSIONS`] terms, which planning expands over
//! the views' terms (an FST range scan per segment, SPEC §7.1) and keeps by collection `df`. Only words of the user
//! text are expanded: excluded clauses, phrases and `field:` clauses are not, as they ask for their text exactly. Nor
//! are the terms of a field whose analyzer stacks tokens (`edge_ngram`, SPEC §6.3.1): its index already holds every
//! prefix of a word, so the term matches the documents a prefix would, and a prefix would only add the stacked
//! grams' scores together.

use super::{LevelInput, LevelTransform, Step};
use crate::query::{
    BoolQuery, BoostQuery, ClauseKind, CombinedQuery, Compiled, DEFAULT_MAX_EXPANSIONS, Filter,
    PrefixQuery, Query,
};
use crate::schema::FieldId;
use crate::segment::ReadError;

/// The fewest characters (Unicode scalar values) of an analyzed term that L3 expands when it is not the last term
/// (SPEC §7.6): a shorter prefix would match too many terms to say much. The last term is expanded whatever its
/// length: it is the word a user may still be typing.
pub(crate) const MIN_LEN: usize = 3;

/// The largest `boost` a query holds (SPEC §7.1: `0 < w ≤ 1000`).
const MAX_BOOST: f64 = 1000.0;

/// The smallest `boost` a query holds: the least positive `f64` (SPEC §7.1: `0 < w`).
const MIN_BOOST: f64 = f64::from_bits(1);

/// L3 `prefix` (SPEC §7.6, ROADMAP 8.2.3, ADR-0040).
///
/// A term is a field's analyzed term of a word: one per field of a `combined` (a word over the default fields), or a
/// `term` of a programmatic query. A term of a `text` field whose analyzer does not stack tokens is expanded when it is
/// the last term or has at least [`MIN_LEN`] characters. The last term is the last part of the last required clause
/// of the text, when that clause is a word (of a word the fields split apart, each field's last token), or the last
/// `must` clause of a programmatic root `bool` (a programmatic `term` or `combined` alone is its own last term).
///
/// Expanded, a `combined` becomes a `bool` of `should` clauses: the fields it keeps exact, as one `combined`, and a
/// `prefix` per expanded field. A `prefix` scores with its field's schema `boost` as the field weight (SPEC §7.2), as a
/// `term` does, so it is boosted only by the ratio of a `fields` weight to that boost, when they differ. So a document matches the word when it holds the term
/// in an exact field or a term starting with it in an expanded one, and each expanded field scores its own prefix
/// (SPEC §7.1: the kept terms share the largest `df`). A step that expands nothing is not built.
#[derive(Debug)]
pub(crate) struct Prefix;

impl LevelTransform for Prefix {
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        let query = match input.compiled {
            Some(compiled) => of_compiled(compiled, input.expands),
            None => of_query(input.original, input.expands),
        };
        Ok(query
            .map(|query| Step {
                query,
                applied: "prefix".to_owned(),
                effective: input.compiled.map(Compiled::effective),
                alternatives: None,
            })
            .into_iter()
            .collect())
    }
}

/// The step of a compiled query: its words expanded, its other required clauses, excluded clauses and filters as they
/// are. `None` when nothing is expanded, or the query is not the one its clauses describe.
fn of_compiled(compiled: &Compiled, expands: &dyn Fn(FieldId) -> Option<f64>) -> Option<Query> {
    // The root's `must_not` and `filter` stay as they are, as at L2: a clause a caller excluded by hand too.
    let (must_not, filter): (Vec<Query>, Vec<Filter>) = match compiled.root() {
        // The compiler builds no `should` at the root: a root changed since is not its clauses'.
        Some(root) if !root.should.is_empty() || root.minimum_should_match != 0 => return None,
        Some(root) => (root.must_not.clone(), root.filter.clone()),
        None => (Vec::new(), Vec::new()),
    };
    let last = compiled.clauses.iter().rposition(|c| !c.negated);
    let mut must = Vec::new();
    let mut expanded = false;
    for (index, clause) in compiled.clauses.iter().enumerate() {
        let parts = compiled.parts(index)?;
        if clause.negated {
            continue;
        }
        for (i, part) in parts.iter().enumerate() {
            let is_last = Some(index) == last && i + 1 == parts.len();
            match clause.kind {
                ClauseKind::Word => match expand(part, is_last, expands) {
                    Some(q) => {
                        expanded = true;
                        must.push(q);
                    }
                    None => must.push(part.clone()),
                },
                _ => must.push(part.clone()),
            }
        }
    }
    if !expanded {
        return None;
    }
    Some(
        if must.len() == 1 && must_not.is_empty() && filter.is_empty() {
            must.pop()?
        } else {
            Query::Bool(BoolQuery {
                must,
                must_not,
                filter,
                ..BoolQuery::default()
            })
        },
    )
}

/// The step of a programmatic query: the leaves of a root `bool`'s `must` clauses expanded (the last clause's last
/// term the last term), or a `term`, `combined` or `boost` alone. Its `must_not` and `filter` stay as they are. `None`
/// when nothing is expanded, or for a root `bool` with `should` clauses or a `minimum_should_match` (its `must` are not
/// all it requires, as L2 reads it).
fn of_query(query: &Query, expands: &dyn Fn(FieldId) -> Option<f64>) -> Option<Query> {
    match query {
        Query::Bool(root) if root.should.is_empty() && root.minimum_should_match == 0 => {
            let mut root = root.clone();
            expand_must(&mut root.must, true, expands).then_some(Query::Bool(root))
        }
        Query::Bool(_) => None,
        other => expand(other, true, expands),
    }
}

/// Expands each of `must` in place, the last one `last` when the list is; whether any changed.
fn expand_must(must: &mut [Query], last: bool, expands: &dyn Fn(FieldId) -> Option<f64>) -> bool {
    let n = must.len();
    let mut changed = false;
    for (i, clause) in must.iter_mut().enumerate() {
        if let Some(q) = expand(clause, last && i + 1 == n, expands) {
            *clause = q;
            changed = true;
        }
    }
    changed
}

/// `query` with its terms prefix-expanded, `None` when none is. `last` marks the last term: in a `bool`, only its last
/// `must` clause inherits it, and each `should` clause does (each is one field's reading of the same word).
/// `must_not` and `filter` clauses, phrases, `prefix`, `fuzzy`, `match_all` and `constant_score` are not expanded: what
/// they match is not a word's terms.
fn expand(query: &Query, last: bool, expands: &dyn Fn(FieldId) -> Option<f64>) -> Option<Query> {
    let wanted = |field: FieldId, term: &str| {
        expands(field).is_some() && (last || term.chars().count() >= MIN_LEN) && !term.is_empty()
    };
    match query {
        Query::Term(t) if wanted(t.field, &t.term) => Some(prefix(t.field, &t.term)),
        Query::Combined(c) => {
            let (open, exact): (Vec<_>, Vec<_>) =
                c.fields.iter().partition(|f| wanted(f.field, &f.term));
            if open.is_empty() {
                return None;
            }
            let mut should = Vec::with_capacity(open.len() + 1);
            if !exact.is_empty() {
                should.push(Query::Combined(CombinedQuery {
                    fields: exact.into_iter().cloned().collect(),
                }));
            }
            should.extend(open.into_iter().map(|f| {
                let q = prefix(f.field, &f.term);
                // A `prefix` scores with the schema boost as its field weight (SPEC §7.2), as a `term` does: a `fields`
                // weight other than it scales the prefix by their ratio, within a boost's range (SPEC §7.1).
                let schema = expands(f.field).unwrap_or(f.weight);
                // A weight far below the boost can underflow the ratio to 0. A boost of 0 is invalid
                // (SPEC §7.1: `0 < w`), not the absence of a boost, so the ratio is clamped to the least
                // positive `f64` and the prefix stays scaled down.
                let ratio = (f.weight / schema).clamp(MIN_BOOST, MAX_BOOST);
                if ratio.to_bits() == 1.0_f64.to_bits() || ratio.is_nan() {
                    q
                } else {
                    Query::Boost(BoostQuery {
                        query: Box::new(q),
                        boost: ratio,
                    })
                }
            }));
            Some(if should.len() == 1 {
                should.pop()?
            } else {
                Query::Bool(BoolQuery {
                    should,
                    ..BoolQuery::default()
                })
            })
        }
        Query::Bool(b) => {
            let mut b = b.clone();
            let mut changed = expand_must(&mut b.must, last, expands);
            for clause in &mut b.should {
                if let Some(q) = expand(clause, last, expands) {
                    *clause = q;
                    changed = true;
                }
            }
            changed.then_some(Query::Bool(b))
        }
        Query::Boost(b) => expand(&b.query, last, expands).map(|q| {
            Query::Boost(BoostQuery {
                query: Box::new(q),
                boost: b.boost,
            })
        }),
        _ => None,
    }
}

fn prefix(field: FieldId, term: &str) -> Query {
    Query::Prefix(PrefixQuery {
        field,
        prefix: term.to_owned(),
        max_expansions: DEFAULT_MAX_EXPANSIONS,
    })
}

#[cfg(test)]
mod tests;
