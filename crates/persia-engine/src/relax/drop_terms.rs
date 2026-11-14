//! L2 `drop_terms` (SPEC §7.6, ROADMAP 8.2.2): fewer of the query's terms required.
//!
//! A *term* of L2 is a required clause of the query: of a compiled query, a clause of its text that is not excluded
//! (`-`), whatever its parts (a phrase, a word split in two, a word over several fields); of a programmatic one, a
//! `must` clause of its root `bool`. Terms are dropped least important first ([`Importance`], ROADMAP 8.2.2.1).

use std::cmp::Ordering;

use super::{LevelInput, LevelTransform, Step};
use crate::query::{BoolQuery, Compiled, Filter, Query};
use crate::score::idf;
use crate::segment::ReadError;
use crate::stats::StatisticsProvider;

/// How important a term of L2 is (SPEC §7.6, ROADMAP 8.2.2.1): L2 drops the least important first.
///
/// Importance compares lexicographically, with no weights to tune: a term no document holds (ROADMAP 8.2.2.5) is less
/// important than any other, then a stopword is less important than any other term, then a lower IDF is less
/// important, then a later position in the query (what a user types last tends to qualify
/// what came first: `red running shoes` keeps `shoes` over `red` when they are as rare).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Importance {
    /// Whether no document holds the term, by the collection's `df`s ([`Weight::none`]): no hit of any step holds it,
    /// so a step that kept it would name in its `effective` a word none of its hits has.
    pub(crate) missing: bool,
    /// Whether the term is a stopword of a field it searches ([`crate::query::CompiledClause::stopword`]). A
    /// programmatic query's terms are not: it has no text to read the word from.
    pub(crate) stopword: bool,
    /// The term's IDF, about as BM25F scores it ([`Weight::idf`]).
    pub(crate) idf: f64,
    /// Its position among the query's terms, from 0.
    pub(crate) position: usize,
}

impl Importance {
    /// `Less` when `self` is less important than `other`: a term no document holds before any other, then a stopword,
    /// then the lower IDF, then the later position. Positions are distinct among a query's terms, so no two of them tie.
    pub(crate) fn compare(&self, other: &Self) -> Ordering {
        other
            .missing
            .cmp(&self.missing)
            .then_with(|| other.stopword.cmp(&self.stopword))
            .then_with(|| self.idf.total_cmp(&other.idf))
            .then_with(|| other.position.cmp(&self.position))
    }
}

/// The indexes of `terms`, least important first: the order L2 drops them in.
pub(crate) fn drop_order(terms: &[Importance]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..terms.len()).collect();
    order.sort_by(|&a, &b| match (terms.get(a), terms.get(b)) {
        (Some(a), Some(b)) => a.compare(b),
        _ => Ordering::Equal,
    });
    order
}

/// [`Weight::idf`] of `query` alone, for tests.
///
/// # Errors
/// A `df` that cannot be read ([`StatisticsProvider::df`]).
#[cfg(test)]
pub(crate) fn query_idf(query: &Query, stats: &dyn StatisticsProvider) -> Result<f64, ReadError> {
    Ok(weigh(query, stats)?.idf)
}

/// [`Weight::none`] of `query` alone, for tests: whether it matches no document by the `df`s (ROADMAP 8.2.2.5).
///
/// # Errors
/// A `df` that cannot be read ([`StatisticsProvider::df`]).
#[cfg(test)]
pub(crate) fn matches_none(
    query: &Query,
    stats: &dyn StatisticsProvider,
) -> Result<bool, ReadError> {
    Ok(weigh(query, stats)?.none)
}

/// What L2 reads of a term from the statistics, each `df` once.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Weight {
    /// The term's IDF about as BM25F scores it (SPEC §7.2): a `term`'s own; a `combined` query's from the largest `df`
    /// of its terms, as its score takes it; a `phrase`'s the sum of its terms'. A `bool` sums its `must` clauses' (all
    /// of them score) and adds the largest of its `should` clauses' (the query compiler's alternatives over fields, of
    /// which usually one matches; BM25F sums every one a document matches); `filter` and `must_not` clauses do not
    /// score. `boost` and `constant_score` are their query's. `prefix`, `fuzzy` and `match_all` have no `df` of their
    /// own until planned, so they are 0: the least important of a programmatic query's terms.
    pub(crate) idf: f64,
    /// Whether the term matches no document by the `df`s alone (ROADMAP 8.2.2.5): a `term` of `df` 0, a `combined`
    /// whose every field's term has `df` 0, a `phrase` with a term of `df` 0, a `bool` with such a `must` clause, or
    /// whose `should` clauses are all it requires (no `must` or `filter`) and all such, and a `boost` or
    /// `constant_score` of such a query. A `df` counts deleted documents too (SPEC §7.2), so a term of `df` above 0
    /// may still be held by none: that one is not caught. `prefix`, `fuzzy` and `match_all` are not: their terms are
    /// only known once planned.
    pub(crate) none: bool,
}

/// The [`Weight`] of `query` over `stats`.
///
/// # Errors
/// A `df` that cannot be read ([`StatisticsProvider::df`]).
pub(crate) fn weigh(query: &Query, stats: &dyn StatisticsProvider) -> Result<Weight, ReadError> {
    let n = stats.doc_count();
    let weight = |idf, none| Weight { idf, none };
    Ok(match query {
        Query::Term(t) => {
            let df = stats.df(t.field, t.term.as_bytes())?;
            weight(idf(n, df), df == 0)
        }
        Query::Combined(c) => {
            let mut df = 0_u64;
            for f in &c.fields {
                df = df.max(stats.df(f.field, f.term.as_bytes())?);
            }
            weight(idf(n, df), df == 0)
        }
        Query::Phrase(p) => {
            let (mut sum, mut none) = (0.0, false);
            for t in &p.terms {
                let df = stats.df(p.field, t.term.as_bytes())?;
                sum += idf(n, df);
                none |= df == 0;
            }
            weight(sum, none)
        }
        Query::Bool(b) => weigh_bool(b, stats)?,
        Query::Boost(b) => weigh(&b.query, stats)?,
        Query::ConstantScore(c) => weigh(&c.query, stats)?,
        Query::Prefix(_) | Query::Fuzzy(_) | Query::MatchAll => weight(0.0, false),
    })
}

fn weigh_bool(query: &BoolQuery, stats: &dyn StatisticsProvider) -> Result<Weight, ReadError> {
    let (mut sum, mut must_none) = (0.0, false);
    for q in &query.must {
        let w = weigh(q, stats)?;
        sum += w.idf;
        must_none |= w.none;
    }
    let (mut best, mut every_should_none): (f64, bool) = (0.0, true);
    for q in &query.should {
        let w = weigh(q, stats)?;
        best = best.max(w.idf);
        every_should_none &= w.none;
    }
    // `should` clauses are all it requires only without `must` and `filter` clauses.
    let should_only = query.must.is_empty() && query.filter.is_empty() && !query.should.is_empty();
    Ok(Weight {
        idf: sum + best,
        none: must_none || should_only && every_should_none,
    })
}

/// The fewest terms L2 relaxes (SPEC §7.6, ROADMAP 8.2.2.3). A query of one term (one required clause, whatever its
/// parts: a phrase, a word split in two, a word over several fields, beside any excluded clauses) skips the level, and
/// so does one of none: no count below L0's is at least 1, and dropping the one term would match every document.
const MIN_TERMS: usize = 2;

/// L2's percentages of the `n` terms (SPEC §7.6), after the 100% that is L0 itself (every term required), and the
/// last step's one term.
const LADDER: [(&str, Rung); 3] = [
    ("75%", Rung::Percent(75)),
    ("50%", Rung::Percent(50)),
    ("1", Rung::One),
];

#[derive(Debug, Clone, Copy)]
enum Rung {
    Percent(usize),
    One,
}

/// The ladder of `n` terms: each step's label and its count, `max(1, ⌊p · n / 100⌋)` of the terms (rounded down, as
/// Elasticsearch does). A count that repeats the previous step's is not built again, and L0 is the step before the
/// first, at `n`: 100% is L0's query, which every later step excludes, so it would match nothing.
pub(crate) fn ladder(n: usize) -> Vec<(&'static str, usize)> {
    let mut previous = n;
    let mut steps = Vec::new();
    for (label, rung) in LADDER {
        let count = match rung {
            Rung::Percent(p) => (n.saturating_mul(p) / 100).max(1),
            Rung::One => 1,
        };
        if count < previous {
            steps.push((label, count));
            previous = count;
        }
    }
    steps
}

/// A term of L2: its query, the clause of the user text it compiled from (`None` for a programmatic query), and
/// whether that clause is a stopword.
#[derive(Debug, Clone)]
struct Term {
    query: Query,
    clause: Option<usize>,
    stopword: bool,
}

/// L0's query as L2 reads it: its terms, and the `must_not` and `filter` clauses beside them, which every step keeps.
#[derive(Debug)]
struct Root<'a> {
    terms: Vec<Term>,
    must_not: &'a [Query],
    filter: &'a [Filter],
}

impl<'a> Root<'a> {
    /// The terms of `input`'s query. `None` when L2 has no terms to drop in it: fewer than [`MIN_TERMS`], a
    /// programmatic `bool` with `should` clauses or a `minimum_should_match` already (its `must` are not all of what
    /// it requires), or a compilation whose clauses do not index its query.
    fn of(input: &LevelInput<'a>) -> Option<Self> {
        // A compiled query's root around its clauses (ROADMAP 8.1.17): its own query when that is its one part, which is
        // one term and skips the level.
        let root = match (input.compiled, input.original) {
            (Some(compiled), _) => compiled.root(),
            (None, Query::Bool(root)) => Some(root),
            (None, _) => None,
        };
        let (must_not, filter): (&[Query], &[Filter]) =
            root.map_or((&[], &[]), |root| (&root.must_not, &root.filter));
        // Counted before a term is copied or its `df` read: a single term skips the level at no cost.
        if Self::count(input)? < MIN_TERMS {
            return None;
        }
        let terms = match (input.compiled, input.original) {
            (Some(compiled), _) => Self::compiled(compiled)?,
            (None, Query::Bool(root)) => root
                .must
                .iter()
                .map(|q| Term {
                    query: q.clone(),
                    clause: None,
                    stopword: false,
                })
                .collect(),
            (None, _) => return None,
        };
        Some(Self {
            terms,
            must_not,
            filter,
        })
    }

    /// How many terms `input`'s query has: its required clauses, or a programmatic root `bool`'s `must` clauses.
    /// `None` for any other programmatic query, which L2 does not relax.
    fn count(input: &LevelInput<'_>) -> Option<usize> {
        match (input.compiled, input.original) {
            // The compiler builds no `should` at the root: a root changed since has requirements L2 would drop.
            (Some(compiled), _)
                if compiled
                    .root()
                    .is_some_and(|r| !r.should.is_empty() || r.minimum_should_match != 0) =>
            {
                None
            }
            (Some(compiled), _) => Some(compiled.clauses.iter().filter(|c| !c.negated).count()),
            (None, Query::Bool(root))
                if root.should.is_empty() && root.minimum_should_match == 0 =>
            {
                Some(root.must.len())
            }
            // Another programmatic query is one term (`match_all` none), or a `bool` L2 does not relax.
            (None, _) => None,
        }
    }

    /// The required clauses of `compiled`, each one term of its parts.
    fn compiled(compiled: &Compiled) -> Option<Vec<Term>> {
        let mut terms = Vec::new();
        for (index, clause) in compiled.clauses.iter().enumerate() {
            // An excluded clause's parts are read too: a root whose `must_not` no longer holds them is not the one the
            // clauses describe, and the steps would drop an exclusion their `effective` still states.
            let parts = compiled.parts(index)?;
            if clause.negated {
                continue;
            }
            let query = match parts {
                [one] => one.clone(),
                parts => Query::Bool(BoolQuery {
                    must: parts.to_vec(),
                    ..BoolQuery::default()
                }),
            };
            terms.push(Term {
                query,
                clause: Some(index),
                stopword: clause.stopword,
            });
        }
        Some(terms)
    }
}

/// L2 `drop_terms` (SPEC §7.6, ROADMAP 8.2.2.2): the `minimum_should_match` ladder over L0's terms.
///
/// Each step requires `count` of the `n` terms ([`ladder`]), any of them: its query is a `bool` whose `should` are
/// the terms, most important first ([`Importance`]), with that `minimum_should_match`, and L0's `must_not` and
/// `filter` clauses. Scores are BM25F's over the terms a document matches, so the rarer terms it holds rank it. The
/// step's `effective` text is the clauses it keeps: the `count` most important terms and every excluded clause, in
/// text order. The terms dropped first are the least important, though a hit may hold any `count` of them.
#[derive(Debug)]
pub(crate) struct DropTerms;

impl LevelTransform for DropTerms {
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        let Some(Root {
            terms,
            must_not,
            filter,
        }) = Root::of(input)
        else {
            return Ok(Vec::new());
        };
        // At least `MIN_TERMS`, so the ladder has a step.
        let rungs = ladder(terms.len());
        let mut importance = Vec::with_capacity(terms.len());
        for (position, term) in terms.iter().enumerate() {
            let weight = weigh(&term.query, input.stats)?;
            importance.push(Importance {
                missing: weight.none,
                stopword: term.stopword,
                idf: weight.idf,
                position,
            });
        }
        // Most important first.
        let mut kept = drop_order(&importance);
        kept.reverse();
        let should: Vec<Query> = kept
            .iter()
            .filter_map(|&i| terms.get(i).map(|t| t.query.clone()))
            .collect();
        let mut steps = Vec::with_capacity(rungs.len());
        for (label, count) in rungs {
            let Ok(minimum_should_match) = u32::try_from(count) else {
                continue;
            };
            let query = Query::Bool(BoolQuery {
                should: should.clone(),
                must_not: must_not.to_vec(),
                filter: filter.to_vec(),
                minimum_should_match,
                ..BoolQuery::default()
            });
            let effective = input.compiled.map(|compiled| {
                let mut keeps = vec![false; compiled.clauses.len()];
                for clause in kept
                    .iter()
                    .take(count)
                    .filter_map(|&i| terms.get(i)?.clause)
                {
                    if let Some(keep) = keeps.get_mut(clause) {
                        *keep = true;
                    }
                }
                effective(compiled, &keeps)
            });
            steps.push(Step {
                query,
                applied: format!("drop_terms:{label}"),
                effective,
                alternatives: None,
                corrected: Vec::new(),
            });
        }
        Ok(steps)
    }

    /// The ladder is nested (ROADMAP 8.2.2.4): every step has the same `should` clauses, `must_not` and `filter`, and a
    /// lower `minimum_should_match` than the one before, so a document that matches `k` of the terms matches `k − 1`.
    fn nested(&self) -> bool {
        true
    }
}

/// The text of the clauses of `compiled` a step keeps (SPEC §7.6 `effective`): those `kept` marks (by clause index) and
/// every excluded one, in text order, one space apart, as [`Compiled::effective`] renders all of them.
fn effective(compiled: &Compiled, kept: &[bool]) -> String {
    let mut text = String::new();
    for (index, clause) in compiled.clauses.iter().enumerate() {
        if !clause.negated && !kept.get(index).copied().unwrap_or(false) {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(compiled.source(index).unwrap_or(""));
    }
    text
}

#[cfg(test)]
mod tests;
