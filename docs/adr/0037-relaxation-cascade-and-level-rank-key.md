# ADR-0037: The relaxation cascade, and a rank key led by the level

- Status: proposed
- Date: 2026-11-08
- Related: SPEC §6.2.3, §7.3, §7.4, §7.5, §7.6; ROADMAP 8.1.1–8.1.5, 8.1.9, 8.3.2, 8.3.4

## Context

SPEC §7.6 makes zero-results avoidance the default. When a query finds fewer than `min_results` hits, the engine
loosens it level by level (L0 `exact` through L5 `fallback`), stops at the first level that has enough (or fills the
page with `fill`), ranks every hit of a stricter level above every hit of a looser one ("score offset per level"),
and stays within `budget_ms`. M8's levels (ROADMAP 8.2) all plug into one orchestrator, so its contract has to hold
before any of them exists. Several things were not specified. What counts as "the number of hits"? How do paging
(`offset`, `search_after`) and pruning work across levels? Which matches does a facet count? What does a cut-short
cascade return?

## Decision

1. **A plan of levels, each building steps** (`relax::RelaxPlan`, ROADMAP 8.1.1). The collection's `relaxation`
   settings, already part of the stored schema (SPEC §6.2.3), give the defaults. A query's `RelaxOptions` override
   each setting they give, with the same range rules, so no manifest change is needed. A level's transform turns the
   query as compiled (and the last query built before it) into zero or more *steps*, strictest first: L2's ladder is
   several steps. A transform runs only when the cascade reaches its level, and one that builds no step skips the
   level. Until ROADMAP 8.2 only L0 builds one.
2. **Each step excludes the steps before it** (ROADMAP 8.1.2). Step `i` searches
   `bool { must: [query_i], must_not: [query_0 … query_{i-1}] }`. A `must_not` clause does not score, and a `bool`
   with one `must` clause scores exactly that clause's score (`0.0 + s`). So a document is a hit of the first step
   that matches it, once, at that step's score, bit for bit. Exclusion goes by match, not by the hits collected, so
   it holds for documents outside the window and for steps a cursor skips. It costs one extra walk of the earlier
   queries' postings in each later step, which is cheap next to what a relaxed query already pays.
3. **Hits count from the top.** The cascade needs `need = max(offset + limit, min_results)` hits from the top. Step
   `i` collects `need − found` hits (pruned as any search, SPEC §7.3) and stops the cascade once `found ≥ min_results`
   (with `fill`, also `found ≥ need`). Without `fill`, the decision to relax therefore does not depend on the page:
   a page of one still relaxes when the query has fewer than `min_results` hits. With `fill`, a deeper page may relax
   further. A query that has them at L0 (with `fill`, `need` of them) runs exactly one search of its query, as a
   plain search does, with a window of `need`: the request's own unless `min_results` is larger, when L0 collects
   (and prunes for) `min_results` hits, since fewer would not tell whether it has them. The default `min_results` is
   1, so by default the window is the request's. The `search_relaxed` benchmarks (`docs/benchmarks.md`) check that.
4. **Facets count the union of the steps run.** Each step counts its own matches. They are disjoint (item 2), so the
   sum counts every document the response can return once, whatever the page. This is the interim rule ROADMAP
   8.1.9 confirms or replaces.
5. **The page is the steps' hits in step order**, each step's in the request's sort, from `offset`. A hit carries
   its step and level. An explained hit is explained by its step's plan: L0's by the query's own plan, as a plain
   search explains it, and a later step's by a `bool` whose one `must` clause is the step's query (its `must_not`
   clauses do not score, so they add nothing). ROADMAP 8.3.4 states the level in the explanation itself.

## Consequences

- Every later level only builds queries; ranking, paging, counting and facets come from the orchestrator.
- A relaxed step re-plans the earlier steps' queries inside its `must_not`. Expansions (`prefix`, `fuzzy`) expand
  again over the same views, to the same terms.
- Until ROADMAP 8.1.3, a request with a `search_after` cursor runs L0 alone, and hits keep their step's own score
  with no offset. Until ROADMAP 8.1.4, `budget_ms` is not applied and no response is `truncated`.

## Alternatives considered

- **Exclude by the hits already collected** (a set of locations). Every earlier hit is known whenever the cascade
  escalates (it escalates only when a step found fewer than it asked for), but not for the steps a cursor's page
  skips. Matches are always known.
- **Decide on the page's hits** (after `offset` or the cursor). Page 2 could then relax where page 1 did not, so
  the pages would not be slices of one ranking.
