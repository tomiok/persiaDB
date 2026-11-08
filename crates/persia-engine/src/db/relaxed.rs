//! A search that relaxes its query when it finds too few hits (SPEC §7.6, ROADMAP 8.1.2, 8.1.3): the relaxation
//! cascade over [`CollectionViews`].
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
//!
//! Hits rank by step first, then by the sort (ROADMAP 8.1.3): every hit of a stricter step above every hit of a
//! looser one, whatever their scores. A hit's cursor records its step and the last step of its page's cascade, so the
//! next page builds the same steps, searches the cursor's step after the cursor and the later ones from their top,
//! and the pages are slices of one ranking.

use std::borrow::Cow;

use super::exec::{ExecTerms, Execution};
use super::search::{Collected, Cursor, Part, Ranked, SearchHit, SearchRequest, StepCursor};
use super::views::CollectionViews;
use crate::query::{BoolQuery, Query};
use crate::relax::{LevelInput, RelaxLevel, RelaxPlan, rank_score};
use crate::schema::CollectionSchema;
use crate::search::{FacetCounter, FacetCounts, RequestError, SearchError, SearchPlan};
use crate::stats::SearchStatistics;

/// A hit of a relaxed search: the hit, and the step of the cascade that found it.
#[derive(Debug, Clone, PartialEq)]
pub struct RelaxedHit {
    /// The hit. Its score is its step's.
    pub hit: SearchHit,
    /// The level of the step that matched it first.
    pub level: RelaxLevel,
    /// That step's index in the cascade: 0 is L0's query.
    pub step: usize,
    /// [`rank_score`] of the step and the score: never below a later step's, so it does not increase down a page
    /// sorted by score descending.
    pub rank_score: f64,
    /// The last step of the cascade that returned the hit, for its cursor.
    last: usize,
}

impl RelaxedHit {
    /// The cursor of the page after this hit (SPEC §7.4, §7.6): [`SearchHit::cursor`] with the hit's step and the
    /// last step of its cascade, for [`CollectionViews::search_relaxed`].
    pub fn cursor(&self) -> Cursor {
        Cursor {
            relaxed: Some(StepCursor {
                step: self.step,
                last: self.last,
            }),
            ..self.hit.cursor()
        }
    }
}

/// What the cascade did (SPEC §7.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relaxation {
    /// The level of the last step built: [`RelaxLevel::Exact`] when L0 found enough or no later level built a step.
    pub level: RelaxLevel,
    /// What each step after L0 applied, in the order they were built.
    pub applied: Vec<String>,
}

/// What a relaxed search returns.
#[derive(Debug, Clone, PartialEq)]
pub struct RelaxedResults {
    /// The page, best first: the hits of each step in step order.
    pub hits: Vec<RelaxedHit>,
    /// One per requested facet, in request order, over the matches of every step built.
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

/// A step the cascade built, and what its search found when it ran one.
struct Ran {
    level: RelaxLevel,
    searched: Option<(SearchPlan, Collected)>,
}

/// Where the cascade starts and how far it must go: from a request's cursor, or from the top.
#[derive(Debug, Clone, Copy)]
struct Start {
    /// The step whose hits the page starts in, after the cursor.
    step: usize,
    /// The cascade builds at least the steps through this one (the cursor's page did).
    last: Option<usize>,
}

impl Start {
    /// From the top without a cursor; from `cursor`'s step otherwise, one without a step being step 0 of a cascade
    /// that stopped at L0.
    fn of(cursor: Option<&Cursor>) -> Self {
        match cursor {
            None => Self {
                step: 0,
                last: None,
            },
            Some(cursor) => {
                let at = cursor.relaxed.unwrap_or(StepCursor { step: 0, last: 0 });
                Self {
                    step: at.step,
                    last: Some(at.last),
                }
            }
        }
    }
}

impl CollectionViews<'_> {
    /// Searches `query` with `request`, relaxing it as `relax` says when it finds fewer than `min_results` hits (SPEC
    /// §7.6): each of the plan's steps in turn, until the hits found reach `min_results` (with
    /// [`RelaxPlan::fill`], also `offset + limit`) or the steps run out. Each step is planned over these views
    /// ([`SearchPlan::with_terms`] under `exec`, with [`super::CollectionSnapshot::search_statistics`] of their
    /// memtable view) and searched as [`Self::search_with`] searches, its earlier steps' matches excluded.
    ///
    /// With a `search_after` cursor ([`RelaxedHit::cursor`]), the steps through the cursor's last one are built
    /// again, those before the cursor's step are not searched for hits (they rank before it; with facets they are
    /// still counted), the cursor's step is searched after the cursor and later ones from their top, until the page
    /// is full or the last step is passed (with `fill`, until the page is full). A cursor without a step
    /// ([`SearchHit::cursor`]) is step 0 of a cascade that stopped at L0.
    ///
    /// # Errors
    /// As [`Self::search_with`]; [`RequestError::CursorStep`] for a cursor whose step is after its last;
    /// [`RelaxedSearchError::Plan`] when a step cannot be planned.
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
        let start = Start::of(request.search_after.as_ref());
        let min_results = usize::try_from(relax.min_results()).unwrap_or(usize::MAX);
        // Hits counted from the top: enough to tell `min_results`, and the page's window. After a cursor the decision
        // was made by its page (`start.last`), so only the window counts.
        let need = if start.last.is_none() {
            window.max(min_results)
        } else {
            window
        };
        // Whether the cascade builds step `i`, `found` hits found before it.
        let builds = |i: usize, found: usize| {
            i == 0
                || relax.relaxes()
                    && match start.last {
                        None => found < min_results || relax.fill() && found < need,
                        Some(last) => i <= last || relax.fill() && found < need,
                    }
        };
        let counting = !request.facets.is_empty();

        let mut ran: Vec<Ran> = Vec::new();
        let mut earlier: Vec<Query> = Vec::new();
        let mut applied = Vec::new();
        let mut found = 0usize;
        'levels: for planned in relax.planned() {
            // Checked before a level builds its steps: one the cascade does not reach builds nothing.
            if !builds(ran.len(), found) {
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
                let i = ran.len();
                if !builds(i, found) {
                    break 'levels;
                }
                // Hits this step is to collect: none before the cursor's step, or once the page is full.
                let wanted = if i >= start.step {
                    need.saturating_sub(found)
                } else {
                    0
                };
                let searched = if wanted > 0 || counting {
                    let plan = plan_step(&step, &earlier, schema, &stats, &terms)?;
                    let collected = self.collect(
                        &plan,
                        &Part {
                            sort: &request.sort,
                            after: request.search_after.as_ref().filter(|_| i == start.step),
                            facets: &request.facets,
                            explain: request.explain,
                            window: wanted,
                        },
                        exec,
                    )?;
                    found = found.saturating_add(collected.ranked.len());
                    Some((plan, collected))
                } else {
                    None
                };
                if i > 0 {
                    applied.push(label);
                }
                ran.push(Ran {
                    level: planned.level,
                    searched,
                });
                earlier.push(step.into_owned());
            }
        }
        let level = ran.last().map_or(RelaxLevel::Exact, |r| r.level);
        // The cursor's page built at least its last step; with fewer steps now (another plan), the page ends sooner.
        let last = ran.len().saturating_sub(1).max(start.last.unwrap_or(0));
        let (hits, facets) = self.relaxed_page(ran, request, start.step, last, exec)?;
        Ok(RelaxedResults {
            hits,
            facets,
            relaxation: Relaxation { level, applied },
        })
    }

    /// The page of the steps `ran`: their hits in step order from `request.offset` (counted from the cursor's step
    /// `first`), at most `request.limit`, each explained by its step's plan when asked, and the facets of every step
    /// summed. `last` is the cascade's last step, for the hits' cursors.
    fn relaxed_page(
        &self,
        ran: Vec<Ran>,
        request: &SearchRequest,
        first: usize,
        last: usize,
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
        for (step, Ran { level, searched }) in ran.into_iter().enumerate() {
            let Some((plan, collected)) = searched else {
                continue;
            };
            for (facet, counted) in facets.iter_mut().zip(collected.facets) {
                facet.merge(counted);
            }
            // A step before the cursor's was searched only to count its facets: its hits rank before the cursor.
            if step < first {
                continue;
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
            hits.extend(page.into_iter().map(|hit| RelaxedHit {
                rank_score: rank_score(step, hit.score),
                hit,
                level,
                step,
                last,
            }));
        }
        Ok((hits, facets.into_iter().map(FacetCounter::finish).collect()))
    }
}

/// The plan of a step's `query` with the `earlier` steps' queries excluded (SPEC §7.6): a document they match is
/// theirs. L0, with none earlier, is planned as it is. A `bool` with one `must` clause and only `must_not` beside it
/// scores that clause's score exactly (`0.0 + s`), so the step ranks as its query alone.
fn plan_step(
    query: &Query,
    earlier: &[Query],
    schema: &CollectionSchema,
    stats: &SearchStatistics<'_>,
    terms: &ExecTerms<'_>,
) -> Result<SearchPlan, SearchError> {
    if earlier.is_empty() {
        return SearchPlan::with_terms(query, schema, stats, terms);
    }
    let excluded = Query::Bool(BoolQuery {
        must: vec![query.clone()],
        must_not: earlier.to_vec(),
        ..BoolQuery::default()
    });
    SearchPlan::with_terms(&excluded, schema, stats, terms)
}

#[cfg(test)]
mod tests;
