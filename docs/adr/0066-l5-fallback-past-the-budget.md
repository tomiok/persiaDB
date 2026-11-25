# ADR-0066: L5 `fallback` runs past the budget when the page would be empty

- Status: proposed
- Date: 2026-11-25
- Related: SPEC §7.6 ("Budget", L5 `fallback`); ROADMAP 8.1.4, 8.1.33, 8.2.5; ADR-0043, ADR-0047

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
   again past the budget, whole. A cursor whose step is after the one L5 would take (its hits rank before the cursor).

## Consequences

- `search_relaxed` gains one post-cascade step (`fallback_past_budget`); the cascade loop is unchanged.
- A truncated page from the top whose L0 found nothing now holds L5's hits. Tests that asserted the empty page under a
  plan with L5 now assert L5's hits.
- L5 is the cheap level (no term walk, FOR-block bounds or a recency walk that stops once it has the page), so running
  it past the budget costs about one more step; the deadline bounds it.
- Open (ROADMAP 8.1.34): the rescued L5 takes the step index after the steps completed, which is not the index L5 has
  in a cascade the budget did not cut. A hit's cursor from such a page records that index, so the next page, if its
  budget does not pass at the same point, rebuilds other steps up to it and pages a step that is not L5: it may repeat
  or skip hits. A truncated page already only promises that the next page tries again; recording L5 in the cursor
  (a cursor token version) would make it exact.

## Alternatives considered

- **Raise the budget, or start it later.** It would still pass on a slow enough index, and moves the latency bound
  for every query, not only the ones that would come back empty.
- **Run L5 past the budget on every truncated page.** A page with stricter hits does not need it, and L5's hits below
  them would make every truncated page as slow as an untruncated one.
- **Run the whole rest of the cascade past the budget when the page is empty.** L2 to L4 read statistics and walk term
  dictionaries (the slow part the budget exists for); L5 reads neither.
