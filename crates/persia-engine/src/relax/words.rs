//! The words L3 `prefix` and L4 `fuzzy` rewrite (SPEC §7.6, ROADMAP 8.2.3, 8.2.4): a query's terms, each a field's
//! analyzed term of a word, replaced by a query the level builds for it, the rest of the query as it is.
//!
//! A term is one field of a `combined` (a word over the default fields), or a `term` of a programmatic query. Only the
//! terms of the fields the search lets the levels rewrite are offered to the level: `text` fields whose analyzer does
//! not stack tokens ([`super::LevelInput::expands`]). The level decides which of those it rewrites, and into what.
//! Excluded clauses, phrases and `field:` clauses are not rewritten, as they ask for their text exactly, nor are
//! filters.
//!
//! A rewritten `combined` becomes a `bool` of `should` clauses: the fields it keeps exact, as one `combined`, and the
//! level's query per rewritten field. That query scores with its field's schema `boost` as the field weight (SPEC
//! §7.2), as a `term` does, so it is boosted only by the ratio of a `fields` weight to that boost, when they differ.

use crate::query::{BoolQuery, BoostQuery, ClauseKind, CombinedQuery, Compiled, Filter, Query};
use crate::schema::FieldId;

/// The largest `boost` a query holds (SPEC §7.1: `0 < w ≤ 1000`).
const MAX_BOOST: f64 = 1000.0;

/// The smallest `boost` a query holds: the least positive `f64` (SPEC §7.1: `0 < w`).
const MIN_BOOST: f64 = f64::from_bits(1);

/// How a level rewrites the terms of a query's words.
pub(crate) struct Words<'a> {
    /// The schema `boost` of a field whose terms the level may rewrite, `None` for any other
    /// ([`super::LevelInput::expands`]).
    pub(crate) expands: &'a dyn Fn(FieldId) -> Option<f64>,
    /// The query that replaces the analyzed `term` of `field`, `None` to keep it exact. `last` marks the query's last
    /// term (L3 expands it whatever its length). Asked only for a field `expands` accepts and a term that is not empty.
    pub(crate) leaf: &'a dyn Fn(FieldId, &str, bool) -> Option<Query>,
}

impl Words<'_> {
    /// The step of a compiled query: its words rewritten, its other required clauses, excluded clauses and filters as
    /// they are. `None` when nothing is rewritten, or the query is not the one its clauses describe.
    pub(crate) fn of_compiled(&self, compiled: &Compiled) -> Option<Query> {
        // The root's `must_not` and `filter` stay as they are, as at L2: a clause a caller excluded by hand too.
        let (must_not, filter): (Vec<Query>, Vec<Filter>) = match compiled.root() {
            // The compiler builds no `should` at the root: a root changed since is not its clauses'.
            Some(root) if !root.should.is_empty() || root.minimum_should_match != 0 => return None,
            Some(root) => (root.must_not.clone(), root.filter.clone()),
            None => (Vec::new(), Vec::new()),
        };
        let last = compiled.clauses.iter().rposition(|c| !c.negated);
        let mut must = Vec::new();
        let mut rewritten = false;
        for (index, clause) in compiled.clauses.iter().enumerate() {
            let parts = compiled.parts(index)?;
            if clause.negated {
                continue;
            }
            for (i, part) in parts.iter().enumerate() {
                let is_last = Some(index) == last && i + 1 == parts.len();
                match clause.kind {
                    ClauseKind::Word => match self.rewrite(part, is_last) {
                        Some(q) => {
                            rewritten = true;
                            must.push(q);
                        }
                        None => must.push(part.clone()),
                    },
                    _ => must.push(part.clone()),
                }
            }
        }
        if !rewritten {
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

    /// The step of a programmatic query: the leaves of a root `bool`'s `must` clauses rewritten (the last clause's last
    /// term the last term), or a `term`, `combined` or `boost` alone. Its `must_not` and `filter` stay as they are.
    /// `None` when nothing is rewritten, or for a root `bool` with `should` clauses or a `minimum_should_match` (its
    /// `must` are not all it requires, as L2 reads it).
    pub(crate) fn of_query(&self, query: &Query) -> Option<Query> {
        match query {
            Query::Bool(root) if root.should.is_empty() && root.minimum_should_match == 0 => {
                let mut root = root.clone();
                self.rewrite_must(&mut root.must, true)
                    .then_some(Query::Bool(root))
            }
            Query::Bool(_) => None,
            other => self.rewrite(other, true),
        }
    }

    /// Rewrites each of `must` in place, the last one `last` when the list is; whether any changed.
    fn rewrite_must(&self, must: &mut [Query], last: bool) -> bool {
        let n = must.len();
        let mut changed = false;
        for (i, clause) in must.iter_mut().enumerate() {
            if let Some(q) = self.rewrite(clause, last && i + 1 == n) {
                *clause = q;
                changed = true;
            }
        }
        changed
    }

    /// The level's query for `term` of `field`, when the field may be rewritten and the level rewrites it.
    fn leaf(&self, field: FieldId, term: &str, last: bool) -> Option<Query> {
        self.expands(field)?;
        if term.is_empty() {
            return None;
        }
        (self.leaf)(field, term, last)
    }

    fn expands(&self, field: FieldId) -> Option<f64> {
        (self.expands)(field)
    }

    /// `query` with its terms rewritten, `None` when none is. `last` marks the last term: in a `bool`, only its last
    /// `must` clause inherits it, and each `should` clause does (each is one field's reading of the same word).
    /// `must_not` and `filter` clauses, phrases, `prefix`, `fuzzy`, `match_all` and `constant_score` are not rewritten:
    /// what they match is not a word's terms.
    fn rewrite(&self, query: &Query, last: bool) -> Option<Query> {
        match query {
            Query::Term(t) => self.leaf(t.field, &t.term, last),
            Query::Combined(c) => {
                let mut open = Vec::new();
                let mut exact = Vec::new();
                for f in &c.fields {
                    match self.leaf(f.field, &f.term, last) {
                        Some(q) => open.push((f, q)),
                        None => exact.push(f.clone()),
                    }
                }
                if open.is_empty() {
                    return None;
                }
                let mut should = Vec::with_capacity(open.len() + 1);
                if !exact.is_empty() {
                    should.push(Query::Combined(CombinedQuery { fields: exact }));
                }
                should.extend(open.into_iter().map(|(f, q)| {
                    // The level's query scores with the schema boost as its field weight (SPEC §7.2), as a `term`
                    // does: a `fields` weight other than it scales the query by their ratio, within a boost's range
                    // (SPEC §7.1).
                    let schema = self.expands(f.field).unwrap_or(f.weight);
                    // A weight far below the boost can round the ratio to 0, which is no boost: the smallest one keeps
                    // the query scaled down.
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
                let mut changed = self.rewrite_must(&mut b.must, last);
                for clause in &mut b.should {
                    if let Some(q) = self.rewrite(clause, last) {
                        *clause = q;
                        changed = true;
                    }
                }
                changed.then_some(Query::Bool(b))
            }
            Query::Boost(b) => self.rewrite(&b.query, last).map(|q| {
                Query::Boost(BoostQuery {
                    query: Box::new(q),
                    boost: b.boost,
                })
            }),
            _ => None,
        }
    }
}
