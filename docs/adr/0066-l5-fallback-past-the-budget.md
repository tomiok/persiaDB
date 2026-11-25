# ADR-0066: L5 `fallback` runs past the budget when the page would be empty

- Status: proposed
- Date: 2026-11-25
- Related: SPEC §7.4, §7.6 ("Budget", L5 `fallback`); ROADMAP 8.1.4, 8.1.33, 8.1.34, 8.2.5; ADR-0043, ADR-0047

## Context

The relaxation budget (`relax.budget_ms`, SPEC §7.6) runs from the moment L0 finishes, and once it has passed the
cascade stops before the next level and before the next step, and drops a step in progress whole. The response holds
the steps completed, with `truncated: true`. A query whose L0 found nothing, and whose budget passes before L5
`fallback`, therefore answers an empty page, though L5, which matches every document the filters keep, would have
filled it. That breaks pillar 3 ("never zero results by default") exactly when the index is slow, and the budget is
meant to bound latency, not to give up on the one level that needs no words (found in the wave 7 planning).

## Decision

1. **When the budget passed in a step of the page and the page would otherwise be empty, a plan with L5 still runs
   L5**, as its own step after the steps completed, planned and searched as any L5 step (its order, its exclusions
   of the earlier steps, its facets when the page counts them), bounded by the request's own deadline and
   cancellation and not by the budget. Those stay errors (SPEC §7.3), so a request that set a deadline is never
   answered later than it asked.
2. **Nothing else changes about a truncated page.** The steps the budget skipped stay skipped (they are not built,
   listed or excluded), L6 `filters` after L5 is not built, and the response stays `truncated: true`. Its `level` and
   `applied` describe the steps built, now ending with `fallback`; `fallback` names L5's order (L5 is the hit step);
   the hits' cursors record L5's step as the cascade's last, as for any truncated page; the metrics count the query at
   `fallback` (SPEC §7.6 "Metrics").
3. **"Would otherwise be empty"** is: the steps completed found no more hits than the page's `offset` (so none on the
   page), and the cascade still wants hits (fewer than it collects: `offset + limit`, and from the top at least
   `min_results`, as for any step). A page that holds any hit of the steps completed is not
   rescued: its hits are a stricter level's, and adding L5's below them would be a page the budget allowed less time
   for than it took.
4. **When it does not run:** the plan has no L5 (`fallback: "none"`, or a cascade that runs L0 alone): the empty
   truncated page is the answer. L5 was built already: searched (its result is the page's), or skipped for the limits
   of SPEC §7.1.3 (`fallback:skipped`, it does not fit). A budget that passed inside L5 dropped it whole, so L5 is run
   again past the budget, whole. On a cursor's page, while the cursor's step has not been rebuilt and searched (item 5).
5. **A cursor knows its step is L5's** (ROADMAP 8.1.34). The rescued L5 takes the step index after the steps
   completed, which is not the index L5 has in a cascade the budget does not cut, so a cursor recording only the
   index would page another level's step next time. Cursor token version 2 (not yet shipped) gets one more flag, 32,
   "the step is L5 `fallback`'s" (`StepCursor::fallback`, set for every L5 hit). The page after such a cursor builds the
   steps before the cursor's step, then runs L5 at the cursor's step, whatever level the cascade would build there,
   searched after the cursor and not bounded by the budget: the steps before it are the same steps on the same
   snapshot, so it continues the cursor's ranking exactly, whether or not this page's budget passes. After a cursor
   of another level's step, L5 never takes that step: it would read that level's cursor (its sort values and
   location) in L5's own order, skip L5's documents and could return the previous page's hits again. It runs past the
   budget only once the cursor's step was rebuilt and searched (then from its top, as on a page from the top); when
   the budget passes before that, the page is the empty truncated page, and the next page tries again.

## Consequences

- `search_relaxed` gains one post-cascade step (`fallback_past_budget`); the cascade loop only stops, after a cursor
  of L5's step, before building another level at that step.
- A truncated page from the top whose L0 found nothing now holds L5's hits. Tests that asserted the empty page under a
  plan with L5 now assert L5's hits.
- L5 is the cheap level (no term walk, FOR-block bounds or a recency walk that stops once it has the page), so running
  it past the budget costs about one more step; the deadline bounds it.
- After a page L5 filled past the budget, the query's later pages are L5's ranking from that point (they do not go back
  to the levels the budget skipped): the pages by cursor are the pages by offset of that page's ranking.
- With `fill` and facets, a page after an L5 cursor does not build the steps past its own only to count them, and its
  counts are still those of a page holding every hit: L5 matches every document the filters keep that no earlier
  step matched, so the steps through L5 count each such document once (SPEC §7.5; a test compares them).

## Alternatives considered

- **Raise the budget, or start it later.** It would still pass on a slow enough index, and moves the latency bound
  for every query, not only the ones that would come back empty.
- **Run L5 past the budget on every truncated page.** A page with stricter hits does not need it, and L5's hits below
  them would make every truncated page as slow as an untruncated one.
- **Run the whole rest of the cascade past the budget when the page is empty.** L2 to L4 read statistics and walk term
  dictionaries (the slow part the budget exists for); L5 reads neither.
