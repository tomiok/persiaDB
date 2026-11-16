# ADR-0048: The request filter is kept beside the compiled query, which is replaced only through it

- Status: proposed
- Date: 2026-11-15
- Related: SPEC §7.1, §7.1.1, §7.6; ROADMAP 8.1.17, 8.2.5, 8.2.6, 8.2.13, 8.2.14; ADR-0039, ADR-0044, ADR-0047

## Context

A request's `filter` joins a compiled query as the filter clauses of its root `bool` (SPEC §7.1, ROADMAP 8.1.17). The
relaxation levels read it back from there: L1 to L4 through `Compiled::root`, L5 `fallback` (ADR-0044 item 1) and the
filter step (ADR-0047 item 2) through `Compiled::root` too. `Compiled::query` was a public field, so a caller could put
any query in its place. When that query did not hold the clauses' parts, `Compiled::root` returned `None`:

- L1 to L4 built no step, which is safe: they build from the clauses.
- L5 became `match_all` without the exclusions and without the filter, so a filtered query fell back to documents
  its filter rejects under `filters: "never"`, the default that promises filters never relax.
- The filter step skipped, and L0 itself searched without the filter when the new query left it out.

SPEC §7.6 and ADR-0044/0047 stated this as a known limitation (ROADMAP 8.2.14). Several points were open: where the
filter lives, whether a caller may still replace the query, what L5 and the filter step do with a replaced query that
is not a root `bool`, and what "the request's filters" means once a caller's query has filters of its own.

## Decision

1. **The compilation keeps the request filter beside its query.** `Compiled::with_filter` joins the filter to the
   query as before and also records it, in order (`Compiled::filter`). A filter the root holds already is not added
   twice.
2. **`Compiled::query` is private; the query is replaced only through `Compiled::with_query`.** `query()` reads it,
   `into_query()` takes it out. `with_query(q)` puts `q` in place and joins the kept filter to it again, as
   `with_filter` joins it: added to a root `bool` without `should` clauses or a `minimum_should_match`, else
   `bool { must: [q], filter }` (a filter beside `should` clauses would make them optional), and not twice. So
   `Compiled::query` holds every request filter at its root `bool` whatever a caller puts in, and L0 never searches
   without it.
3. **The clauses describe the new query only while it holds their parts where they were.** A one-part compilation
   checks the query against its part, as since ROADMAP 8.1.17, so it describes a later query that holds the part
   again. For any other, `with_query` compares the new root with the current one: a `bool` without `should` clauses
   or a `minimum_should_match` whose `must` is the current one and whose `must_not` holds the clauses' entries at the
   same indexes keeps them (a caller may add exclusions after them, which every level keeps). A required clause a
   caller adds detaches them: the word levels rebuild `must` from the clauses alone and would relax it away unlisted.
   Otherwise the compilation is detached from then on: `parts` and `root` are `None`, and L1 to L4 build no step. A
   compilation of no clause is always detached by a replacement. Before, any root `bool` was read as the clauses'
   root, so a query with other entries at those indexes was read as their parts.
4. **L5 keeps the request filter, read from the compilation.** Its query is `match_all` under the root `bool`'s
   `must_not` and `filter`, for a compiled query and a programmatic one alike, plus the kept request filter when the
   root lacks it (it cannot, by item 2, but L5 does not depend on that). A caller's root `bool` keeps its exclusions
   and filters at L5. A caller's query with `should` clauses is wrapped to join the filter, so its exclusions are
   one level down and L5 does not keep them, as for a programmatic query with a deeper `must_not`.
5. **The filter step drops the request filter alone.** Of a compiled query, the step is L0's root without the kept
   request filter. A filter the caller's root holds itself is part of that query and stays, unless it equals a request
   filter: the join does not add that one twice, so the two cannot be told apart, and it counts as the request's. When
   the join wrapped the caller's query (its root has `should` clauses or a `minimum_should_match`), the wrapped root is
   the caller's, and the request's filters are dropped from it too (`Compiled` records that it wrapped): a clone of
   `Compiled::query` with `should` clauses added, which holds the request filter one level down, would otherwise keep
   it, and the step would search L0's filtered matches again and find nothing. A
   compiled query with no request filter builds no step even when the caller's root has filters. A programmatic query
   is unchanged: every filter of its root `bool` is dropped. The no-word rule is ADR-0047 item 2's, as refined on main
   (a recursive check through `bool`, `boost` and `constant_score`).
6. **Every other level is unchanged.** L1 to L4 read the clauses' root, which holds the request filter by item 2, and
   keep its `must_not` and `filter` as before. `LevelTransform::covers_earlier` still holds: every earlier step keeps
   the root's `must_not` and `filter`, which L5 keeps.

## Consequences

- **Breaking** for `persia-engine` callers: `Compiled::query` is no longer a public field. Read it with `query()`,
  take it with `into_query()`, replace it with `with_query()`. Nothing outside the engine's own tests wrote it, and
  the README's quickstart takes the query with `into_query()`.
- `filters: "never"` now holds for any query a caller puts in, at every level, L5 included. A `db::relaxed` test
  replaces a filtered query with terms, `match_all`, a `should` root, `boost` and `constant_score` and checks every
  hit, and the filter step's property test puts a random query in place of the compiled one.
- The cascade's plan, steps and `applied` are the same for a query that was not replaced: the root's filters are the
  request filter, so L5 and the filter step build the same queries. `search_relaxed/l0`, `off`, `l1`–`l5` and
  `filters/*` are within 1% of main measured last, either way, with one codegen unit (docs/benchmarks.md).

## Alternatives considered

- **Keep `query` public and have the cascade re-join the kept filter to every step, L0 included**: not breaking, but
  a plain search of `compiled.query` stays unprotected, and telling whether a root already holds the filter is a
  heuristic over a query anyone may have rewritten.
- **Store the query without the filter and join it at search time**: every reader of `Compiled::query` would have to
  know to join it; a reader that forgot would search unfiltered.
- **The filter step drops every root filter of a replaced query** (as of a programmatic one): it would relax filters
  the caller wrote into their own query, which are not the request's.
- **The filter step skips any replaced query**: safe, but `last_resort` would stop working for a caller who only
  adds an exclusion beside the clauses.
