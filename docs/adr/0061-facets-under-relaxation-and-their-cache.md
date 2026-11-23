# ADR-0061: Facets under relaxation count the steps a page of every hit builds, once per query

- Status: proposed
- Date: 2026-11-22
- Related: SPEC §7.4, §7.5, §7.6; ROADMAP 8.1.2, 8.1.4, 8.1.9, 8.1.11, 8.2.6, 8.3.2; ADR-0037 (item 4 superseded),
  ADR-0045, ADR-0047

## Context

A facet counts, per value, the documents a query matches (SPEC §7.5). Under the relaxation cascade (§7.6) the question
is which matches: the strict query's, those of the level returned, or those of every step run. ADR-0037 item 4 set an
interim rule, the union of the steps each page built, each document once (the steps' matches are disjoint). Without
`fill` every page builds the same steps, so every page counted the same. With `fill` a deeper page builds more steps,
so it counted more, and the facets of one query changed from page to page (ROADMAP 8.1.9).

The interim rule had a cost too (ROADMAP 8.1.11): a cursor page does not search the steps before its cursor's for
hits, but it had to search them again, with every match visited, only to count them. A deep page paid most of the
first page's work, and its budget, so it could be truncated before any hit.

## Decision

1. **A facet counts the matches of every step up to and including the hit step that a page holding every hit would
   build, each document once, at its first step** (ROADMAP 8.1.9; replaces ADR-0037 item 4). That page is the one from
   the top with a window past every hit: it builds the steps any page builds without `fill`, and with `fill` every step
   of the levels that fill, until the steps run out. Its steps after its hit step match nothing (it collects every
   match of each step it searches, so a step with a match found a hit), so the set is the steps that page builds. So
   every page of one query on one snapshot counts the same, with or without `fill`, by offset or by cursor. The filter
   step (L6) counts when it is among those steps: when the steps before it found fewer than `min_results` hits from
   the top, which every page decides alike. A skipped step (§7.1.3) matches nothing; a cut one counts its cut matches,
   as every page cuts it alike.
2. **With `fill` and facets, a page counts the steps past its own.** Once a page has its hits, it builds the remaining
   steps of the levels that fill and searches each to count its matches only, collecting no hit. These steps are not
   the page's: they are not listed in `applied` nor `level`, and a hit's cursor does not record them as its cascade's
   last step. They are excluded from the later ones as any step, and held to the limits alike, so they are the steps
   the page of every hit builds.
3. **The budget.** Counting runs under the budget as every step does. A page the budget truncates (`truncated: true`)
   counts the steps it completed, as its hits are those of the steps it completed. The budget can also pass in a
   step a page searches only to count it (item 2): the page's own steps are done, so it stays whole
   (`truncated: false`, its hits, `level` and `applied` complete), but its counts cover only the steps searched before
   that one. They are partial, they are not kept (item 4), and nothing in the response flags them yet (ROADMAP
   8.1.31).
4. **The counts are kept once per query** (ROADMAP 8.1.11). A collection's snapshot keeps a small cache of facet counts
   keyed by the memtable view's version, the query and its compilation, the soft filters the filter step may drop,
   the relaxation settings (ADR-0045), the step limits, and the facets asked for: what the counts follow from beside
   the snapshot. Only a page from the top (without a cursor) keeps its counts there, and only when it counted every
   step of item 1 with the budget passing in none of them: neither in one of its own steps (`truncated: true`) nor in
   one it searched only to count it (item 3). A cursor page never keeps its counts, even untruncated: its cursor may
   come from a page the budget truncated, whose last step stops short of the steps a page from the top builds, so it
   may count fewer. A later page of the same query, by offset or cursor, reads the kept counts, and then searches only
   its own steps for hits, pruned, without counting: it no longer searches the steps before its cursor's, nor those
   past its own. The cache holds at most 32 counts and
   1 MiB (keys included, as estimated), evicts the least recently used first by a counter of uses (no clock), and
   goes with its snapshot at the next publish. A plan whose level transforms are not the engine's own (tests) is
   not cached.

## Consequences

- Every page of a query states the same facets, `fill` or not. A `fill` page with facets pays for the steps past its
  own once, L5 included (which matches every document the filters keep), and the next pages read the cache. Measured:
  bench `search_relaxed/facets/fill`, a first `fill` page of 10 with facets that reads no kept counts, went from
  1.39 ms to 12.8 ms (docs/benchmarks.md); the same page without facets is unchanged.
- A possible optimization within the same rule (not done): since L5 `covers_earlier`, the union of the steps through
  L5 equals L5's selection, so a page whose item-1 steps reach L5 could count that selection once, unscored, for
  identical counts instead of searching every step past its own.
- A deep cursor page with facets costs what a page without facets costs, plus a cache lookup: bench
  `search_relaxed/facets/deep` (docs/benchmarks.md). A page whose counts are cached is no longer truncated by
  counting.
- Counts of a truncated page are not kept: the next page counts again. Nor are those of a whole page whose budget
  passed in a counts-only step (item 3): the page reads `truncated: false` while its facets are partial, and until
  ROADMAP 8.1.31 nothing tells the client so.
- The cache is per snapshot, so a write that publishes drops it; a cursor of the old snapshot is an error anyway
  (SPEC §7.4).

## Alternatives considered

- **Count the strict query (L0) only.** Stable and cheap, but a relaxed response's facets would describe none of its
  hits.
- **Count the level returned, or the hit step alone.** It would not count the stricter steps whose hits rank first.
- **Count the steps each page built (ADR-0037 item 4).** Varies with `fill` from page to page.
- **Carry the counts in the cursor.** A cursor is client-held: counts of up to 16 facets of 1 000 values would bloat
  it, and a client could hand back forged counts. The cache keeps them on the server side of the API.
- **A time-based cache.** Eviction by wall clock would make tests depend on it; a counter of uses is deterministic.
