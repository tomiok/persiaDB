# ADR-0058: Cache L1's compiled alternatives per set of default fields

- Status: accepted
- Date: 2026-11-22
- Related: SPEC §6.3.2, §7.1.1, §7.6; ROADMAP 8.1.16, 8.2.1; ADR-0039

## Context

A compilation finds the runs of required words that are a synonym term (SPEC §6.3.2) and compiles each run's
alternatives: every other term of its groups, as the quoted phrase `"<term>"` over the default fields would compile
(`CompiledSynonym::alternatives`, ADR-0039). The alternatives are compiled whether or not the cascade reaches L1, so
a query whose L0 has `min_results` hits pays for them too. Bench `compile/synonyms` (`w3 w40 w7 w120 nosuch`, `w3`
with 50 alternatives among 2 002 groups) measured 37.7 µs, against 2.4 µs without synonyms. Most of it was analyzing
each alternative's text again in every default field, at every compilation.

ROADMAP 8.1.16 offered two ways out: compile the alternatives lazily, when L1 builds its step, or cache them.

## Decision

1. **Cache, not lazy.** `Compiled::synonyms` is public, and what it holds depends on the compiled alternatives:
   a run whose alternatives all compile away (to the run's own query, or to each other) is dropped, and `cut` records
   whether the compilation's bound left one out, which a probe compile decides (ADR-0039). Deferring that would change
   the public `Compiled` (a handle on the compiler's analyzers inside it, or methods in place of fields) and move the
   bound's work into the cascade. A cache keeps `Compiled` and every result as they are.
2. **What is cached**: each synonym term's compiled alternative (or that it compiles to nothing), the first time a
   compilation needs it. An alternative depends only on its term's text, the default fields (their ids, weights and
   order) and their analyzers, not on the query or the limits, so it is the same at every compilation over those
   fields. The runs, their sources, the dedup, the bound and `cut` are still computed per compilation.
3. **Keyed by the default fields, the last 4 sets**: `QueryCompiler` keeps one table per set of `DefaultFields`
   (compared whole, weights included), the least recently used dropped first. A request naming its own `fields`
   (SPEC §7.1) compiles over another set, and most requests use few. Each table holds one slot per term of the
   schema's groups, filled once (`OnceLock`), so compilations on several threads share it; the list of tables is
   behind a `Mutex`, locked once per run.
4. **The compiler owns it**, as it owns the synonym maps (built at the first compilation per field, SPEC §6.3.2): a
   compiler holds its own copy of the schema, so its groups never change under the cache; a schema change, which
   may change the groups (§6.2.4), needs a new compiler, which starts empty.

## Consequences

- `compile/synonyms` measures 19.4 µs instead of 37.7 µs. A search of a query with one 50-alternative run whose L0 has
  enough hits, compiled each time (`search_relaxed/l0/compiled_synonyms`), costs 498 µs instead of 516 µs, against
  476 µs compiled without synonyms (docs/benchmarks.md). What is left is the scan for runs (about 5.5 µs here, over
  2 002 groups), and per alternative a clone and the dedup against the run's earlier ones.
- Memory: per cached set, one slot per synonym term (the groups hold at most 1 MiB of terms, §6.2.1), and the
  alternatives compiled so far: at most 4 sets.
- The first compilation of a run over a set of fields still analyzes its alternatives, as before.

## Alternatives considered

- **Lazy alternatives** (item 1): would also save the clones and the dedup when L1 is never built, but changes the
  public `Compiled` and what it reports before L1 runs.
- **A cache keyed per field** rather than per set: an alternative is one query over all the default fields (a
  `combined` over them, or a phrase per field with positions), so it cannot be assembled from per-field parts
  without repeating the compiler's clause logic.
- **No bound on the sets**: a client sending a new `fields` weight with each request would grow the cache without
  limit.
