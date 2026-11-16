# ADR-0047: Filter relaxation (`filters: "last_resort"`) is one last step, L0 without its root filters

- Status: proposed (amended by ADR-0048: the request filter is kept beside the compilation, so L5 keeps it and the filter step drops it alone, whatever query a caller put in)
- Date: 2026-11-15
- Related: SPEC §6.2.3, §7.1, §7.1.3, §7.4, §7.5, §7.6, §7.7; ROADMAP 8.1.9, 8.2.5, 8.2.6, 8.2.13, 8.2.14, 8.3.1, 8.3.2, 8.3.4;
  ADR-0037, ADR-0043, ADR-0044, ADR-0045, ADR-0048

## Context

SPEC §6.2.3 allows `relaxation.filters: "never" | "last_resort"` and §7.6 says filters are never relaxed unless
`last_resort` is set explicitly. ADR-0045 accepted the key and put it in the settings a cursor records, but no level
dropped a filter, so `"last_resort"` searched as `"never"`. ADR-0044 kept the filters at L5 and rejected relaxing them
there, because filters are often hard constraints (tenancy, permissions). Several points were open:

- Where the relaxation runs: before L5, after it, or only when the cascade would otherwise return nothing.
- What it drops: every filter, or only some (there is no way to mark one as hard), and whether excluded clauses stay.
- Which query it relaxes: L0's, or a later level's.
- How the response, `explain` and cursors show it, and whether that breaks clients (ADR-0043, ADR-0045).
- How it interacts with facets (8.1.9's interim rule), `fill` and the budget.

## Decision

1. **One step, after every other level.** `filters: "last_resort"` (the request's, else the collection's) plans one
   more level, L6 `filters` (`RelaxLevel::Filters`), last: after L5, or after the last word level when `fallback` is
   `"none"`, or right after L0 when the plan has no other level. `"never"` (the default) never plans it, and
   `relax: false` runs L0 alone as before. It is not listed in `levels`: like L5 and `fallback`, only its key turns it
   on. Before L5 would rank documents the filters reject above documents they keep, which is ADR-0044's objection.
   After L5, with the default `fallback: "static_rank"`, the step runs only when fewer than `min_results` documents
   pass the filters (and the excluded clauses) at all: the filters themselves are why the page is short. That is
   what `last_resort` names, and what `FilterRelaxation::LastResort` already promised ("only after every other level
   failed").
2. **It drops every filter of the root `bool`, nothing else.** Of a programmatic query, a root `bool`'s `filter`. Of a
   query compiled from `q`, the request's filter (SPEC §7.1), which the compilation keeps beside its query
   (`Compiled::filter`, amended by ADR-0048): the step drops that filter alone, whatever query a caller put in place of
   the compiled one with `Compiled::with_query` (which joins the request filter to it again), and a filter the
   caller's root `bool` holds itself stays, unless it equals the request filter (then it counts as the request's). Before ADR-0048 the step read the filter through `Compiled::root`, which a
   replaced query could make `None`, losing the request filter at L5 (ROADMAP 8.2.14). Required clauses, `should` clauses, `minimum_should_match`, excluded clauses and
   any deeper filter stay. Excluded clauses are the user's words (`-blue`), not a filter, so a user who wrote `-blue`
   never gets blue, as at L5. The filters the step drops go together (of a programmatic query every filter of its root
   `bool`, of a compiled one its request filter): dropping them one at a time would need an order SPEC does not have.
   A programmatic query whose root has no filter, or whose root is not a `bool`, and a compiled query with no request
   filter, build no step, and the level is skipped. So does a query that searches no word. A clause searches a word when it names a term (`term`, `combined`,
   `phrase`, `prefix`, `fuzzy`) or holds such a clause in `must` or `should` of a `bool`, at any depth through `boost`
   and `constant_score`. A term only in `must_not` does not. `match_all`, an empty `bool` and a `bool` of exclusions
   alone do not, so neither does `should: [match_all]`, `boost` of `match_all`, `constant_score` of `match_all`, or a
   nested empty `bool`. Without its filters such a query would be every live document at score 0, the unfiltered L5
   rejected below, and "browse a category" (an empty `q` and a filter) would be padded with other categories. A root
   of `should` clauses that do search a word keeps them; without the filters one is then required, where L0 under the
   filters only scored them, so the step matches documents holding a word.
3. **No filter is marked hard in this leaf.** Telling a tenancy or permission filter from a soft one needs a new
   request key (for example a `filter` that never relaxes beside one that may), a design for the request JSON (ROADMAP
   10.1, 11.x). SPEC and the guides say a request whose filters enforce access must not set `last_resort`. ROADMAP
   8.2.13 adds the key.
4. **It relaxes L0's query.** Every level builds from L0 (SPEC §7.6), and L0 without the filters is the closest match
   the filters rejected. Its `effective` is L0's text (filters are not part of `q`), with no `corrected`. A later
   level's query is not a looser one in general (L4's `fuzzy` does not contain L2's last rung), so "the last level's"
   has no clear meaning. A misspelled query whose filters nothing passes may still get nothing from the step.
5. **It is built only to reach `min_results`, never to fill a page.** `LevelTransform::fills` is false for it alone:
   with `fill`, the cascade does not build the step because the page has room. Once built (the earlier steps found
   fewer than `min_results` from the top), it collects the hits still missing, as any step, so it fills the page as
   far as its matches go. The decision counts hits from the top, so every page of a query makes the same one: a
   cursor's page builds it only through the cursor's last step, and a deeper page with `fill` does not build it when
   the first page did not. A filtered page is never padded with documents its filters reject. A budget that stops a
   page before the step holds for the query's later cursor pages, `fill` or not, as the step is not built again to
   fill them.
6. **The response states it.** `level` is `filters` once the step is built. `applied` ends with `filters` (or
   `filters:cut` or `filters:skipped` under §7.1.3's limits: a compiled query's step has no expansions to cut, a
   programmatic one's `prefix` and `fuzzy` may be cut). A new flag,
   `Relaxation::filters_dropped` (`"filters_dropped": true`), is set when the filter step is the hit step
   (ADR-0043). Since the step is last, the flag means some hit of the ranking does not pass the request's filters. It
   is the same on every page, as `fallback` is, and each such hit's `level` is `filters`. `fallback` is absent then:
   L5 is not the hit step. An explained hit's `relaxed` node gives `level` `filters`, its step and `applied`
   `"filters"` around its query's tree.
7. **L5 covers every earlier step, when its step is searched uncut.** L5's query is `match_all` under L0's root
   `must_not` and `filter`, which every earlier step keeps, so it matches every document any earlier step matches.
   The cascade then excludes that uncut step alone from the filter step (`LevelTransform::covers_earlier`, as a
   nested level's latest step stands for its level, ROADMAP 8.2.2.4). A cut L5 step (`fallback:cut`) matches less
   than it was built to, so the earlier steps stay excluded beside it (SPEC §7.1.3). The filter step's query is L0
   without its filters, with only the uncut L5 step under its `must_not`, so it plans a few queries, not every
   expansion the levels before it built. Without L5 it excludes the earlier steps as any step does. A test compares
   both against excluding every step.
8. **Facets, budget, limits and cursors are as for any step.** Facets count its matches under 8.1.9's interim rule,
   so they show the values of the documents the filters rejected that it returned. The budget is checked before it is
   built, before it is searched and at its reads, and a step it passes during is dropped. The cursor already records
   `filters` (ADR-0045), and the plan's levels with it, so a cursor from `"never"` is `CursorRelax` under
   `"last_resort"` and the reverse.

## Consequences

- Not a breaking change. Under `"never"` the plan, its steps, their indices and `applied` are unchanged. Under
  `"last_resort"` the step comes after every step that plan built before, so no earlier index or `applied` entry
  moves. `RelaxLevel` and `Relaxation` are `#[non_exhaustive]`, so the new variant and field are additive. Cursors are
  not serialized yet (ROADMAP 10.1), so none from before this change exists. Clients that opted in already get hits
  they did not get before, which is what the option promised.
- Benchmarks (docs/benchmarks.md): `search_relaxed/l0`, `off` and `l1`–`l4` do not slow down under `"never"` (the
  plan is the same; the only new work is one check per level of whether it fills): with one codegen unit each is
  within 2.5% of main, either way. `search_relaxed/filters/last_resort` (790 µs) is a cascade that found nothing
  under a filter no document passes, then the filter step; `filters/never` (328 µs) is the same cascade without it.
  The step costs about what L0 of the same words does, as it excludes L5's step alone.
- A request that enforces access with a filter must not set `last_resort` until 8.2.13. This is stated in SPEC §7.6,
  the guide and the website.

## Alternatives considered

- **The step before L5** (after the last word level): users would see documents outside their filter above
  documents inside it. ADR-0044 rejected relaxing filters at L5 for that reason.
- **Only when the cascade would return nothing**: `min_results` is already the cascade's own "too few", and a
  separate zero rule would be a second threshold.
- **The cascade again without filters** (every word level rebuilt unfiltered): more recall for typos under a
  filter, but up to twice the steps, `applied` entries per level, and a much larger budget cost. It can be added as
  more steps after this one if needed.
- **An unfiltered L5** (the collection's ranking of every document): it returns arbitrary documents, not the closest
  matches of the words.
- **Dropping filters one at a time** (a ladder, as L2): needs an order of filters that SPEC does not define.
- **Building it under `fill` too**: a page with room would be padded with documents the filters reject even though
  the query had enough hits, which "last resort" should not do.
- **No flag, `level` and `applied` only**: `level` names the last step built, not the step that found hits, and a
  filter step that finds nothing still sets it. The flag follows the hit step, as `fallback` does.
