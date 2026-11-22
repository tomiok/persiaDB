# ADR-0057: L3 expands the last term from 2 characters

- Status: accepted
- Date: 2026-11-22
- Related: SPEC §7.1, §7.1.3, §7.6; ROADMAP 8.1.20, 8.2.3, 8.2.7; ADR-0040 (amends item 4), ADR-0046

## Context

ADR-0040 item 4 expands the last term of a query at L3 whatever its length: it is the word a user may still be
typing. Planning expands a `prefix` over every view's terms (an FST range per segment) and keeps the 50 of highest
collection `df` (SPEC §7.1). So a one-character last word (`w`) walks a large share of its field's term dictionary in
every view before the cut keeps 50. In the bench collection, where every term starts with `w`, `search_relaxed/l3`
(`w3 w`) took about 22.6 ms, against about 1.4 ms for an L2 step. And the step it builds says almost nothing: any
document holding `w3` and any of the 50 most frequent words.

ROADMAP 8.1.20 listed three ways to bound it: a minimum length for the last term too, a per-view walk budget, or
`df`-ordered term walks.

## Decision

1. **A minimum length of 2 for the last term** (chosen with the user, 2026-11-22). The last term is expanded when its
   analyzed term has at least 2 characters; every other term still needs 3 (ADR-0040 item 4). A one-character last
   term stays exact, as a short term elsewhere does.
2. **Counted as the other rule counts**: Unicode scalar values of the analyzed term, per field and per part of a word
   the fields split apart (`ñ` is one character in 2 bytes; of `e-m` in a field that splits it, the last token `m`
   stays exact).
3. **The tier step follows**: `prefix:last` (ADR-0046 item 3) expands the last term alone, so with a one-character last
   term it expands nothing and is not built. L3 then builds one step when another term has 3 characters or more, and
   none otherwise (the level is skipped, and the cascade goes on to L4 or L5).
4. **Programmatic queries alike**: the last `must` clause of a root `bool`, or a `term` or `combined` alone, is held to
   the same 2 characters.

## Consequences

- A query ending in a one-letter word no longer pays for a dictionary walk at L3. `search_relaxed/l3` (`w3 w`, L3
  alone) skips the level and reaches L5: 26 µs instead of 22.6 ms. `l3/tiers` (`w12 w`) builds one step that finds
  nothing: 86 µs instead of 22.7 ms (docs/benchmarks.md).
- A user who has typed one letter of the last word gets no completion of it at L3. L2 may still drop that word, and
  L5 still fills when `fallback` is on. Two letters are enough to complete.
- A two-character last word still walks every term starting with it. In a vocabulary with few distinct first letters
  this stays the costly case (`l3/two_letters`, `w3 w1` at the largest `min_results`: 14.9 ms, unchanged). A walk
  budget or `df`-ordered walks remain possible later without changing what a step matches.

## Alternatives considered

- **A per-view walk budget**: bounds the cost at any length, but which terms a step keeps would then depend on how
  the segments split the dictionary, not only on collection `df`, so the same query over the same documents could
  match differently after a merge.
- **`df`-ordered term walks**: keeps SPEC §7.1's rule and bounds the walk, but needs a per-segment index of terms by
  `df` (an on-disk format change) for a case a length rule settles.
- **3 characters for the last term too** (the same rule as the others): `ca` would no longer complete to `cat`, the
  case the last-term rule exists for.
