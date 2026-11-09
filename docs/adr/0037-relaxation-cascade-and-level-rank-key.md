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
   query as compiled into zero or more *steps*, strictest first (L2's ladder is several steps; the levels of ROADMAP
   8.2 add the inputs they need). A transform runs only when the cascade reaches its level, and one that builds no
   step skips the level. Until ROADMAP 8.2 only L0 builds one.
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
   further, so its `level`, `applied` and facets may state more steps than the first page. A query that has them at
   L0 (with `fill`, `need` of them) runs exactly one search of its query, as a plain search does, with a window of
   `need`: the request's own unless `min_results` is larger, when L0 collects (and prunes for) `min_results` hits,
   since fewer would not tell whether it has them. The default `min_results` is 1, so by default the window is the
   request's. The `search_relaxed` benchmarks (`docs/benchmarks.md`) check that.
4. **Facets count the union of the steps run.** Each step counts its own matches. They are disjoint (item 2), so the
   sum counts every document the response can return once, the same on every page without `fill`. This is the interim rule ROADMAP
   8.1.9 confirms or replaces.
5. **The page is the steps' hits in step order**, each step's in the request's sort, from `offset`. A hit carries
   its step and level. An explained hit is explained by its step's plan: L0's by the query's own plan, as a plain
   search explains it, and a later step's by a `bool` whose one `must` clause is the step's query (its `must_not`
   clauses do not score, so they add nothing). ROADMAP 8.3.4 states the level in the explanation itself.

6. **The level leads the rank key; no score offset** (ROADMAP 8.1.3). Hits compare by step, then by the sort, then
   by location. An `f64` offset cannot keep every stricter hit above every looser one over the whole score range.
   Scores are any value `≥ 0`, `+∞` included (`constant_score` takes any finite one, `boost`s nest to `1000^32` and
   overflow). A strictly increasing
   map that puts one copy of that range wholly below another would need twice the floating-point values the range
   holds (pigeonhole). An additive offset big enough to separate the levels also rounds away the scores' low bits,
   which ties or reorders hits inside a level. So each hit keeps its step's own score. For clients that sort or show
   one number, a hit also carries a **rank score**, `2^−(step+1) · (1 + (1 − 1/(1 + s)))`, rounded at each
   operation. Each operation is monotonic and the power of two is exact, so for `i < 1022` step `i`'s values lie in
   `[2^−(i+1), 2^−i]`: non-decreasing in the score within a step, and never below a later step's. From step 1022 on,
   where `2^−(i+1)` is not a normal number, the rank score is 0, which is still never below a later step's. It may
   tie where scores differ (rounding), and where adjacent bands meet (a step's score-0 hit and the next step's huge
   one both give `2^−(i+1)`), never invert. Property tests check both claims over every non-negative `f64` bit
   pattern's binade.
7. **Cursors carry the step and the cascade's last step** (ROADMAP 8.1.3). A page after a cursor builds the same
   steps through that last one. It does not search the steps before the cursor's (they rank before it) unless it
   counts facets, which count every step built (item 4). It searches the cursor's step after the cursor and later
   steps from their top. Each step keeps its own pruning threshold: one from a stricter step says nothing about a
   looser one, whose hits all rank below. Offset and cursor pages are therefore slices of one ranking. Without `fill`,
   they normally report the same `level` and `applied`, except when the budget truncates a cursor page; with `fill`,
   a cursor page relaxes further while it has room. A plain
   search, or one with `relax: false`, rejects a cursor from a step after L0, and any search rejects one whose step is
   after its last
   (`RequestError::CursorStep`).
8. **The budget starts when L0 finishes** (ROADMAP 8.1.4). It reads an injected clock (`Execution::budget_clock`, else
   the deadline's, else the system clock), so tests never read the wall clock. L0 is never cut by it: only the
   request's own deadline and token govern L0, and they stay errors in every step. The budget is checked before each
   level builds, before each step, and inside each step through the same interrupt reads as a deadline (SPEC §7.3):
   the step runs on a copy of the execution whose extra deadline is the budget's end. A step the budget stops is
   dropped whole: it has not seen all its matches, so the hits it holds need not be its best. Returning them would
   rank hits the full step ranks lower, or does not return at all, and a later page by cursor would not continue
   that ranking. The response is the completed steps' hits with `truncated: true`, `level` and `applied` describe
   those steps, and cursors record the last completed step. A deadline error inside a step counts as the budget only
   when the request's own execution is not interrupted. Latency is therefore at most L0's plus about `budget_ms`
   (plus one reader step between two interrupt reads, and reading the page).

## Consequences

- Every later level only builds queries; ranking, paging, counting and facets come from the orchestrator.
- A relaxed step re-plans the earlier steps' queries inside its `must_not`. Expansions (`prefix`, `fuzzy`) expand
  again over the same views, to the same terms. Step `i` copies the queries of the `i` steps before it into its
  `must_not`, quadratic in steps, which are few (L2's ladder is the longest level). L0 is searched as given, never
  copied.
- Scores down a relaxed page are not monotonic across steps. Clients that need one descending number use the rank
  score.
- A cursor page re-runs the transforms of the steps through the cursor's last one (no search for those it does not
  need), so without `fill` it reports the same cascade as the first page.
- `relax::Step` and the transform trait stay crate-private until a level outside the engine needs them.

## Alternatives considered

- **Exclude by the hits already collected** (a set of locations). Every earlier hit is known whenever the cascade
  escalates (it escalates only when a step found fewer than it asked for), but not for the steps a cursor's page
  skips. Matches are always known.
- **Decide on the page's hits** (after `offset` or the cursor). Page 2 could then relax where page 1 did not, so
  the pages would not be slices of one ranking.
- **An additive score offset per level** (SPEC §7.6's first wording). It is not correct over the whole score range
  (item 6), and it would change the scores clients see.
- **A cursor that re-decides the cascade from the top.** Exact, but every page would re-search the steps before
  the cursor's to count them. Recording the last step costs nothing.
- **A budget from the start of the search, L0 included.** It bounds the total latency, but a slow L0 would leave no
  time to relax exactly the queries that need it. Not chosen when the 8.1.4 wording was agreed (2026-11-08).
