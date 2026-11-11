//! L2 `drop_terms` (SPEC §7.6, ROADMAP 8.2.2): fewer of the query's terms required.
//!
//! A *term* of L2 is a required clause of the query: of a compiled query, a clause of its text that is not excluded
//! (`-`), whatever its parts (a phrase, a word split in two, a word over several fields); of a programmatic one, a
//! `must` clause of its root `bool`. Terms are dropped least important first ([`Importance`], ROADMAP 8.2.2.1).
#![expect(
    dead_code,
    reason = "the ladder that orders its terms by importance is ROADMAP 8.2.2.2"
)]

use std::cmp::Ordering;

use crate::query::{BoolQuery, Query};
use crate::score::idf;
use crate::segment::ReadError;
use crate::stats::StatisticsProvider;

/// How important a term of L2 is (SPEC §7.6, ROADMAP 8.2.2.1): L2 drops the least important first.
///
/// Importance compares lexicographically, with no weights to tune: a stopword is less important than any other term,
/// then a lower IDF is less important, then a later position in the query (what a user types last tends to qualify
/// what came first: `red running shoes` keeps `shoes` over `red` when they are as rare).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Importance {
    /// Whether the term is a stopword of a field it searches ([`crate::query::CompiledClause::stopword`]). A
    /// programmatic query's terms are not: it has no text to read the word from.
    pub(crate) stopword: bool,
    /// The term's IDF as BM25F scores it ([`query_idf`]).
    pub(crate) idf: f64,
    /// Its position among the query's terms, from 0.
    pub(crate) position: usize,
}

impl Importance {
    /// `Less` when `self` is less important than `other`: a stopword before any other term, then the lower IDF,
    /// then the later position. Positions are distinct among a query's terms, so no two of them tie.
    pub(crate) fn cmp(&self, other: &Self) -> Ordering {
        other
            .stopword
            .cmp(&self.stopword)
            .then_with(|| self.idf.total_cmp(&other.idf))
            .then_with(|| other.position.cmp(&self.position))
    }
}

/// The indexes of `terms`, least important first: the order L2 drops them in.
pub(crate) fn drop_order(terms: &[Importance]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..terms.len()).collect();
    order.sort_by(|&a, &b| match (terms.get(a), terms.get(b)) {
        (Some(a), Some(b)) => a.cmp(b),
        _ => Ordering::Equal,
    });
    order
}

/// The IDF of `query` as BM25F scores it (SPEC §7.2), over `stats`: a `term`'s own; a `combined` query's from the
/// largest `df` of its terms, as its score takes it; a `phrase`'s the sum of its terms'. A `bool` sums its `must`
/// clauses' (all of them score) and adds the largest of its `should` clauses' (the query compiler's alternatives over
/// fields, of which one matches); `filter` and `must_not` clauses do not score. `boost` and `constant_score` are their
/// query's. `prefix`, `fuzzy` and `match_all` have no `df` of their own until planned, so they are 0: the least
/// important of a programmatic query's terms.
///
/// # Errors
/// A `df` that cannot be read ([`StatisticsProvider::df`]).
pub(crate) fn query_idf(query: &Query, stats: &dyn StatisticsProvider) -> Result<f64, ReadError> {
    let n = stats.doc_count();
    Ok(match query {
        Query::Term(t) => idf(n, stats.df(t.field, t.term.as_bytes())?),
        Query::Combined(c) => {
            let mut df = 0_u64;
            for f in &c.fields {
                df = df.max(stats.df(f.field, f.term.as_bytes())?);
            }
            idf(n, df)
        }
        Query::Phrase(p) => {
            let mut sum = 0.0;
            for t in &p.terms {
                sum += idf(n, stats.df(p.field, t.term.as_bytes())?);
            }
            sum
        }
        Query::Bool(b) => bool_idf(b, stats)?,
        Query::Boost(b) => query_idf(&b.query, stats)?,
        Query::ConstantScore(c) => query_idf(&c.query, stats)?,
        Query::Prefix(_) | Query::Fuzzy(_) | Query::MatchAll => 0.0,
    })
}

fn bool_idf(query: &BoolQuery, stats: &dyn StatisticsProvider) -> Result<f64, ReadError> {
    let mut sum = 0.0;
    for q in &query.must {
        sum += query_idf(q, stats)?;
    }
    let mut best: f64 = 0.0;
    for q in &query.should {
        best = best.max(query_idf(q, stats)?);
    }
    Ok(sum + best)
}

#[cfg(test)]
mod tests;
