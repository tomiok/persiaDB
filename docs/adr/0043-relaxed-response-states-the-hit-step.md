# ADR-0043: A relaxed response states the text of its hit step, the last step that found a hit

- Status: proposed
- Date: 2026-11-14
- Related: SPEC §7.4, §7.6; ROADMAP 8.1.3, 8.1.14, 8.1.22, 8.2.2.5, 8.2.3, 8.2.4.3, 8.2.5; ADR-0037, ADR-0040,
  ADR-0042, ADR-0044

## Context

SPEC §7.6 had the response state `effective` and `corrected` for the *last step built*. A last step can add no hit:
every document it matches was taken by an earlier step, which excludes it (ADR-0037 item 2). With the collection's
levels and `min_results` 10, `red shoez` over `red shoes` and `red hat` relaxes through L2 (`drop_terms:75%`: any one
word, so both `red` documents), L3 and L4 (`red` and a term near `shoez`). L4's matches are L2's, so every hit is
L2's. The response still said `corrected: shoez → shoes` and `effective: "red shoes"`: it named a correction and a
term that found no hit, and `red hat` holds neither. L3 is the same case, with L0's text. ADR-0042 left this open
(ROADMAP 8.1.22).

Any other rule must still give every page of a query on one snapshot the same text (SPEC §7.6). A page after a cursor
builds every step through the cursor's last one but searches only the cursor's step after the cursor and the later
steps while the page has room. A later step that found a hit on the first page may not be searched again on a later
one, so that page cannot tell by itself.

## Decision

1. **The response states the *hit step*: the last step of the cascade that found a hit, L0 when none did.**
   `effective` is the text of its query and `corrected` its corrections (empty unless it is L4's). `level` and
   `applied` keep describing what the cascade built: `red shoez` above reads `level: "fuzzy"`, `applied:
   ["drop_terms:75%", "prefix", "fuzzy"]`, `effective: "red"` and no correction.
2. **"Found a hit" means its search collected one.** A page from the top searches every step it builds for at least
   one hit: a step is built only while hits are missing (`found < min_results ≤ need`, or with `fill`
   `found < need`), so it is searched for up to `need − found > 0` hits and collects one whenever any document
   matches it (L3 above collects none). Whether a step found a hit therefore does not depend on the page. Steps a
   cursor's page searches only to count facets collect none and do not count. They are before the cursor's step, so
   they are not after the hit step anyway.
3. **The cursor carries the hit step** (`StepCursor::hit`, beside `step` and `last`). A cursor's page states the later
   of the cursor's hit step and the last step that found a hit on that page. Without `fill` those are the same as the
   first page's. With `fill`, a deeper page may build more steps and state a later one, as it may already state more
   `applied`. A budget-truncated page states the last completed step that found a hit, the budget's usual exception.
   A cursor whose hit step is before its step (its own hit was found by its step) or after its last is
   `RequestError::CursorStep`. A cursor from a search that did not relax is step 0 of a cascade that stopped at L0,
   with hit step 0.
4. **No hit at all states L0's text.** A query that finds nothing even relaxed reads as asked. A correction that
   found no hit is not a suggestion the index supports.

## Consequences

- The case ROADMAP 8.1.22 pinned in `tests/relax_fuzzy.rs` now asserts the hit step's text, and a second test pages a
  query whose hit step (L4) a cursor page does not search and still states its correction.
- "Did you mean" is quieter: a typo beside a held word whose L2 rung already finds the hits is not reported, because
  those hits were not searched with the correction. A query with every word misspelled, which L2 cannot help, still
  reports its corrections.
- The cursor grows by one `usize`. Cursors are in-memory values until the API and server serialize them (ROADMAP
  10.1, 11.x), so no stored format changes.
- L5's response marker (ADR-0044) uses the same rule: the response is flagged `fallback` when L5 is the hit step.

## Alternatives considered

- **Keep the last step built** (the old rule): simple and page-independent, but it states a text and corrections no
  hit was found by.
- **The hit step without carrying it in the cursor**: a cursor's page would state an earlier step whenever it did not
  search the hit step, so pages of one query would disagree.
- **Re-search the later steps on every cursor page** to learn whether they find a hit: a search per step and page only
  to choose a text, under the budget too, so a truncated page could still disagree.
- **Move `level` to the hit step too**: `level` and `applied` say what the cascade did, which clients and metrics
  (ROADMAP 8.3.3) need. The two questions have different answers when a built step adds nothing.
