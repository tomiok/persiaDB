//! A search that relaxes its query when it finds too few hits (SPEC §7.6, ROADMAP 8.1.2): the relaxation cascade
//! over [`CollectionViews`].
//!
//! The cascade runs [`RelaxPlan`]'s steps in order, L0 (the query itself) first. Each step after the first searches
//! its query with every earlier step's query excluded (`must_not`), so a document is a hit of the first step that
//! matches it, at that step's score, and appears once. After each step the hits found so far are counted: the cascade
//! stops once they reach `min_results` (and, with `fill`, the page's window). The page is the steps' hits in step
//! order, each step's by the request's sort, from `offset`.
//!
//! Each step collects only the hits still needed (`max(offset + limit, min_results)` from the top, less those found),
//! so the decision to relax counts hits from the top whatever the page, and a query that has `min_results` hits at L0
//! runs one search of that query, as [`CollectionViews::search_with`] does.

use std::borrow::Cow;

use super::exec::Execution;
use super::search::{Collected, Part, Ranked, SearchHit, SearchRequest};
use super::views::CollectionViews;
use crate::query::{BoolQuery, Query};
use crate::relax::{LevelInput, RelaxLevel, RelaxPlan};
use crate::search::{FacetCounter, FacetCounts, RequestError, SearchError, SearchPlan};

/// A hit of a relaxed search: the hit, and the step of the cascade that found it.
#[derive(Debug, Clone, PartialEq)]
pub struct RelaxedHit {
    /// The hit. Its score is its step's.
    pub hit: SearchHit,
    /// The level of the step that matched it first.
    pub level: RelaxLevel,
    /// That step's index in the cascade: 0 is L0's query.
    pub step: usize,
}

/// What the cascade did (SPEC §7.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relaxation {
    /// The level of the last step run: [`RelaxLevel::Exact`] when L0 found enough or no later level built a step.
    pub level: RelaxLevel,
    /// What each step after L0 applied, in the order they ran.
    pub applied: Vec<String>,
}

/// What a relaxed search returns.
#[derive(Debug, Clone, PartialEq)]
pub struct RelaxedResults {
    /// The page, best first: the hits of each step in step order.
    pub hits: Vec<RelaxedHit>,
    /// One per requested facet, in request order, over the matches of every step run.
    pub facets: Vec<FacetCounts>,
    /// What the cascade did.
    pub relaxation: Relaxation,
}

/// Why a relaxed search failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RelaxedSearchError {
    /// A step's query could not be planned (or planning was interrupted).
    #[error(transparent)]
    Plan(#[from] SearchError),
    /// The request does not fit, or a step's search failed.
    #[error(transparent)]
    Request(#[from] RequestError),
}

/// A step the cascade ran.
struct Ran {
    plan: SearchPlan,
    level: RelaxLevel,
    collected: Collected,
}

impl CollectionViews<'_> {
    /// Searches `query` with `request`, relaxing it as `relax` says when it finds fewer than `min_results` hits (SPEC
    /// §7.6): each of the plan's steps in turn, until the hits found reach `min_results` (with
    /// [`RelaxPlan::fill`], also `offset + limit`) or the steps run out. Each step is planned over these views
    /// ([`SearchPlan::with_terms`] under `exec`, with [`super::CollectionSnapshot::search_statistics`] of their
    /// memtable view) and searched as [`Self::search_with`] searches, its earlier steps' matches excluded.
    ///
    /// A request with a `search_after` cursor runs L0 alone, as [`Self::search_with`] does: paging across levels is
    /// ROADMAP 8.1.3.
    ///
    /// # Errors
    /// As [`Self::search_with`]; [`RelaxedSearchError::Plan`] when a step cannot be planned.
    pub fn search_relaxed(
        &self,
        query: &Query,
        request: &SearchRequest,
        relax: &RelaxPlan,
        exec: &Execution,
    ) -> Result<RelaxedResults, RelaxedSearchError> {
        let window = self.check_request(request)?;
        let collection = self.collection();
        let schema = collection.schema();
        let stats = collection.search_statistics(self.memtable());
        let terms = exec.terms(self);
        let cascade = relax.relaxes() && request.search_after.is_none();
        let min_results = usize::try_from(relax.min_results()).unwrap_or(usize::MAX);
        // Hits counted from the top: enough to tell `min_results`, and the page's window.
        let need = if cascade {
            window.max(min_results)
        } else {
            window
        };
        let enough = |found: usize| found >= min_results && (!relax.fill() || found >= need);

        let mut ran: Vec<Ran> = Vec::new();
        let mut earlier: Vec<Query> = Vec::new();
        let mut applied = Vec::new();
        let mut found = 0usize;
        let done = |ran: &[Ran], found: usize| !ran.is_empty() && (!cascade || enough(found));
        'levels: for planned in relax.planned() {
            // Checked before a level builds its steps: one the cascade does not reach builds nothing.
            if done(&ran, found) {
                break;
            }
            // L0 searches the query itself, not a copy.
            let steps: Vec<(Cow<'_, Query>, String)> = if ran.is_empty() {
                vec![(Cow::Borrowed(query), String::new())]
            } else {
                let previous = earlier.last().unwrap_or(query);
                let input = LevelInput {
                    original: query,
                    previous,
                    schema,
                };
                planned
                    .transform
                    .steps(&input)
                    .into_iter()
                    .map(|s| (Cow::Owned(s.query), s.applied))
                    .collect()
            };
            for (step, label) in steps {
                if done(&ran, found) {
                    break 'levels;
                }
                let searched = if earlier.is_empty() {
                    Cow::Borrowed(step.as_ref())
                } else {
                    Cow::Owned(Query::Bool(BoolQuery {
                        must: vec![step.as_ref().clone()],
                        must_not: earlier.clone(),
                        ..BoolQuery::default()
                    }))
                };
                let plan = SearchPlan::with_terms(&searched, schema, &stats, &terms)?;
                let collected = self.collect(
                    &plan,
                    &Part {
                        sort: &request.sort,
                        after: request.search_after.as_ref(),
                        facets: &request.facets,
                        explain: request.explain,
                        window: need.saturating_sub(found),
                    },
                    exec,
                )?;
                found = found.saturating_add(collected.ranked.len());
                if !ran.is_empty() {
                    applied.push(label);
                }
                ran.push(Ran {
                    plan,
                    level: planned.level,
                    collected,
                });
                earlier.push(step.into_owned());
            }
        }
        let level = ran.last().map_or(RelaxLevel::Exact, |r| r.level);
        let (hits, facets) = self.relaxed_page(ran, request, exec)?;
        Ok(RelaxedResults {
            hits,
            facets,
            relaxation: Relaxation { level, applied },
        })
    }

    /// The page of the steps `ran`: their hits in step order from `request.offset`, at most `request.limit`, each
    /// explained by its step's plan when asked, and the facets of every step summed.
    fn relaxed_page(
        &self,
        ran: Vec<Ran>,
        request: &SearchRequest,
        exec: &Execution,
    ) -> Result<(Vec<RelaxedHit>, Vec<FacetCounts>), RequestError> {
        // Reading the page's documents is the last stage a deadline stops.
        exec.interrupt()?;
        let segments: Vec<_> = self.segments().collect();
        let mut facets: Vec<FacetCounter> = request
            .facets
            .iter()
            .copied()
            .map(FacetCounter::new)
            .collect();
        let mut skip = request.offset;
        let mut room = request.limit;
        let mut hits = Vec::with_capacity(room);
        for (
            step,
            Ran {
                plan,
                level,
                collected,
            },
        ) in ran.into_iter().enumerate()
        {
            for (facet, counted) in facets.iter_mut().zip(collected.facets) {
                facet.merge(counted);
            }
            let skipped = skip.min(collected.ranked.len());
            skip -= skipped;
            let page: Vec<Ranked> = collected
                .ranked
                .into_iter()
                .skip(skipped)
                .take(room)
                .collect();
            room -= page.len();
            if page.is_empty() {
                continue;
            }
            let explanations = if request.explain {
                self.explain_page(&plan, &segments, &page, collected.kept, exec)?
            } else {
                Vec::new()
            };
            let page = self.page_hits(&segments, page, explanations, &request.projection)?;
            hits.extend(page.into_iter().map(|hit| RelaxedHit { hit, level, step }));
        }
        Ok((hits, facets.into_iter().map(FacetCounter::finish).collect()))
    }
}

#[cfg(test)]
mod tests;
