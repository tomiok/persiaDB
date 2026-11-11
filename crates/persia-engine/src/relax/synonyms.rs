//! L1 `synonyms` (SPEC §7.6, ROADMAP 8.2.1, ADR-0039): the query with each run of words that is a synonym term
//! required as itself or one of its alternatives.
//!
//! The query compiler finds the runs and compiles their alternatives (SPEC §6.3.2, [`Compiled::synonyms`]); this level
//! only builds the step: one, whatever the number of runs, listed as `synonyms`. A programmatic query has no user text
//! to scan, and builds none.

use super::{LevelInput, LevelTransform, Step};
use crate::query::{BoolQuery, Compiled, Query};
use crate::segment::ReadError;

/// A required clause of L1's step: L0's own, or a run of clauses that one of its alternatives may stand for.
#[derive(Debug, Clone, PartialEq)]
enum Unit {
    Plain(Query),
    Expanded {
        original: Query,
        alternatives: Vec<Query>,
    },
}

/// L1's step before it is held to the limits: its required clauses, and L0's excluded ones beside them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Expansion {
    units: Vec<Unit>,
    must_not: Vec<Query>,
}

impl Expansion {
    /// The expansion of `compiled`, or `None` when it has no run of synonyms, or its query is not the one its clauses
    /// describe (a caller changed it).
    pub(crate) fn of(compiled: &Compiled) -> Option<Self> {
        if compiled.synonyms.is_empty() {
            return None;
        }
        let must_not: Vec<Query> = match &compiled.query {
            Query::Bool(root) => {
                // The compiler builds no `should` or filter at the root: a root changed since is not its clauses'.
                if !root.should.is_empty()
                    || !root.filter.is_empty()
                    || root.minimum_should_match != 0
                {
                    return None;
                }
                let mut must_not = Vec::new();
                for (index, clause) in compiled.clauses.iter().enumerate() {
                    if clause.negated {
                        must_not.extend_from_slice(compiled.parts(index)?);
                    }
                }
                must_not
            }
            _ => Vec::new(),
        };
        let mut synonyms = compiled.synonyms.iter().peekable();
        let mut units = Vec::new();
        let mut index = 0;
        while index < compiled.clauses.len() {
            let clause = compiled.clauses.get(index)?;
            if let Some(synonym) = synonyms.next_if(|s| s.clauses.start == index) {
                if synonym.clauses.is_empty() || synonym.alternatives.is_empty() {
                    return None;
                }
                let mut parts = Vec::new();
                for covered in synonym.clauses.clone() {
                    if compiled.clauses.get(covered)?.negated {
                        return None;
                    }
                    parts.extend_from_slice(compiled.parts(covered)?);
                }
                let original = match parts.len() {
                    1 => parts.pop()?,
                    _ => Query::Bool(BoolQuery {
                        must: parts,
                        ..BoolQuery::default()
                    }),
                };
                units.push(Unit::Expanded {
                    original,
                    alternatives: synonym.alternatives.clone(),
                });
                index = synonym.clauses.end;
                continue;
            }
            if !clause.negated {
                units.extend(compiled.parts(index)?.iter().cloned().map(Unit::Plain));
            }
            index += 1;
        }
        // A run out of order or past the last clause.
        if synonyms.next().is_some() {
            return None;
        }
        Some(Self { units, must_not })
    }

    /// The step's query with run `i` or one of its first `keep[i]` alternatives (all of them past the end of `keep`):
    /// a `bool` requiring each unit, with L0's excluded clauses, or the one unit alone.
    pub(crate) fn query(&self, keep: &[usize]) -> Query {
        let mut run = 0;
        let mut must: Vec<Query> = self
            .units
            .iter()
            .map(|unit| match unit {
                Unit::Plain(q) => q.clone(),
                Unit::Expanded {
                    original,
                    alternatives,
                } => {
                    let kept = keep.get(run).copied().unwrap_or(alternatives.len());
                    run += 1;
                    let mut should = Vec::with_capacity(kept.saturating_add(1));
                    should.push(original.clone());
                    should.extend(alternatives.iter().take(kept).cloned());
                    Query::Bool(BoolQuery {
                        should,
                        ..BoolQuery::default()
                    })
                }
            })
            .collect();
        if must.len() == 1 && self.must_not.is_empty() {
            return must.pop().unwrap_or(Query::MatchAll);
        }
        Query::Bool(BoolQuery {
            must,
            must_not: self.must_not.clone(),
            ..BoolQuery::default()
        })
    }
}

/// L1 `synonyms` (SPEC §7.6, ROADMAP 8.2.1): one step, L0's query with every run of words that is a synonym term
/// required as `should: [the run, alternatives...]` (any of them, the matches summed by BM25F). L0's excluded clauses
/// stay excluded and are not expanded. Its `effective` text is L0's: every clause is kept.
#[derive(Debug)]
pub(crate) struct Synonyms;

impl LevelTransform for Synonyms {
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        let Some(compiled) = input.compiled else {
            return Ok(Vec::new());
        };
        let Some(expansion) = Expansion::of(compiled) else {
            return Ok(Vec::new());
        };
        Ok(vec![Step {
            query: expansion.query(&[]),
            applied: "synonyms".to_owned(),
            effective: Some(compiled.effective()),
        }])
    }
}

#[cfg(test)]
mod tests;
