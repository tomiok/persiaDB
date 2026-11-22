# ADR-0056: Relaxation never drops a hard filter or a caller's required clause; `soft_filter` holds what `last_resort` may drop

- Status: proposed
- Date: 2026-11-22
- Related: SPEC §7.1, §7.1.1, §7.1.2, §7.6; ROADMAP 8.2.6, 8.2.13, 8.2.14, 8.2.15, 10.1, 11.x; ADR-0047, ADR-0048

## Context

ADR-0047 made the filter step (L6) the explicit opt-in to drop filters: under `filters: "last_resort"` it searched L0's
query without every filter of the request (item 3: no filter could be marked as one that must hold). A request whose
filters enforce tenancy or permissions therefore could not opt in at all: the same key carried the filters that must
never relax and the ones a user would rather see relaxed than get nothing (a brand, a price range). ROADMAP 8.2.13 asked
for a request key for the filters that never relax.

A second gap was in how a compiled query takes a caller's own query (`Compiled::with_query`, ADR-0048 item 3). The word
levels (L1 to L4) rebuild a step's `must` from the clauses alone, so a required clause a caller added beside them would
have been relaxed away without being listed. ADR-0048 detached the compilation in that case, so the cascade built no
word step for it at all (ROADMAP 8.2.15).

Both are the same question: what may relaxation drop? The answer the product needs is: only what the request says it
may.

## Decision

1. **`filter` is hard; `soft_filter` is what `last_resort` may drop** (ROADMAP 8.2.13). A request has two filter keys
   of the same grammar and limits (SPEC §7.1.2): `filter`, which no level ever drops, and `soft_filter`, which only the
   filter step under `filters: "last_resort"` drops. Both join the compiled query as filter clauses of its root `bool`,
   hard first (`Compiled::with_filter`, `Compiled::with_soft_filter`), so a plain search and every level apply both;
   the compilation keeps each list beside its query (ADR-0048 item 1), and a query put in place is joined to both again.
   A filter given as both is hard. The filter step drops the soft filters alone: from the root, and from a caller's
   root the join wrapped (ADR-0048 item 5). With `last_resort` and no soft filter the level builds no step. L5 keeps
   both, as every word level does.
2. **A programmatic query's root filters are hard unless marked soft.** A query built in code has no request keys:
   every filter of its root `bool` is hard, so the filter step drops none of them, unless the caller marks some soft
   (`RelaxedQuery::with_soft_filter`): a root filter equal to a marked one is soft. Before, the filter step dropped
   every root filter of a programmatic query, which is the "all marked" case now.
3. **No request version.** This is an additive key before 1.0, beside a change of what `last_resort` drops: requests
   that used `last_resort` with `filter` now keep their filters, and build no filter step until they move the
   droppable ones to `soft_filter`. It is noted under Unreleased in the CHANGELOG. The request JSON itself is parsed
   in M10 (ROADMAP 10.1); the engine API above is what it maps to, and the server (11.x) and SDKs carry the key as is.
4. **A caller's required clauses ride along every step** (ROADMAP 8.2.15; amends ADR-0048 item 3). A query put in
   place of a compiled one whose root `must` starts with the clauses' parts keeps the compilation attached. The
   entries after them are required clauses the caller added (`Compiled::extra_must`): L1 requires them beside its
   runs, every L2 rung beside its `minimum_should_match` over the terms (they are not terms), L3 and L4 after the words
   they rewrite (they are not rewritten, and the last term stays the last clause's), L5 selects only documents that
   match them, and the filter step keeps them with L0's root. They have no source text, so no step's `effective`
   renders them. A required clause put before the clauses' parts moves them, and still detaches.

## Consequences

- A tenancy or permission filter can sit in `filter` beside `last_resort`: no hit of any level, page or cursor breaks
  it. `tests/relax_hard_filters.rs` checks that by property over random corpora, text, filters, `fill`, page sizes,
  offsets and cursors, and `relax/filters` and `db/relaxed/tests/filters.rs` pin the step's shape.
- **Breaking** for callers that relied on `last_resort` dropping `filter` (none outside the engine's own tests and the
  `filters/last_resort` bench, which now uses `soft_filter`): they move the droppable filters to `soft_filter`, or mark
  a programmatic root's filters soft.
- The differential suite (`tests/differential.rs`) passes its case's filter as the soft one, so its reference of L6 is
  unchanged.
- `Compiled::extra_must` is new public API; a caller who used to see a detached compilation (no word step) now gets
  every level with the clause kept.
- L5's `covers_earlier` still holds: every earlier step keeps the root's `must_not`, both filter lists and the extra
  `must` entries, and L5 requires all of them.

## Alternatives considered

- **A `hard` flag per filter, or a nested `{"hard": ..., "soft": ...}` object under `filter`**: one key, but the
  grammar of §7.1.2 would change for every request, and a filter with no flag would have to default one way. Two keys
  keep `filter` as it reads everywhere else (hard, as in Elasticsearch's `filter` context).
- **Keep `filter` droppable and add a `hard_filter`**: compatible with existing `last_resort` callers, but the safe
  reading would need the new key, and a request forgetting it would leak documents under `last_resort`.
- **Versioned request semantics**: not worth it before 1.0 (item 3).
- **State a caller's extra clauses in `effective`**: they have no user text, and rendering the AST would invent text
  the user never wrote (ADR-0037 item 9).
