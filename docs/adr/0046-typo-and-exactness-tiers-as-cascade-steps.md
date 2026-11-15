# ADR-0046: Typo and exactness tiers are steps of the cascade, by the most edits any one word needs

- Status: proposed
- Date: 2026-11-15
- Related: SPEC §7.1, §7.1.3, §7.2, §7.3, §7.4, §7.6, §7.7, §16.1; ROADMAP 7.4.11, 7.8.4, 8.1.3, 8.1.22, 8.2.3, 8.2.4,
  8.2.7, 8.2.10, 8.2.11, 8.2.12; ADR-0037, ADR-0040, ADR-0042, ADR-0043, ADR-0045

## Context

ROADMAP 8.2.7 asks that, within a relaxation level, 0 edits rank above 1 and 1 above 2, before BM25F, and that a word
matched as typed rank above one matched by its stem, and that above one matched by a completion. Before, L4 `fuzzy`
was one step, and its only typo signal was the edit-distance weight inside the score (SPEC §7.1: `(distance + 1 − d)
/ (distance + 1)`). A document two edits away that holds the word in a short title, or holds it several times, could
outscore one a single edit away. `braceelt` ranked `Bracket` above two bracelets. L3 `prefix` was one step too:
without L2 before it (`levels: ["prefix"]`, search as you type), `red sho` ranked `Redwood shoe rack` above red
sneakers.

Two shapes were open: a tiered sort key computed per hit, or a component of the score. ADR-0037 already rules out a
score offset: no offset added to an `f64` keeps every score's order, and a large one rounds away the low bits. A key
computed per hit (total edits, then the number of completed words) is the finest form. But it needs a tier method
on every matcher, it puts a key before the score in every tiered step, and that key stops score pruning there:
a document a threshold would skip may hold a better tier. The cascade already has an order before the sort, its
steps, with exclusion, rank bands, cursors, the budget, facets and `explain` built around it.

## Decision

1. **The tiers are steps of the cascade, built strictest first** (chosen over a per-hit key). A level's tier step is
   an ordinary step: its hits rank above the next step's at any score, it is excluded from the later steps, and
   pruning, paging, the budget, facets and `explain` treat it like any step. Neither the score nor the request's sort
   changes, and the score has no tier part (SPEC §7.2).
2. **The tier is the most edits any one word needs, not the sum.** L4 builds `fuzzy:1` before `fuzzy` when a term it
   rewrites has 8 characters or more (distance 2). `fuzzy:1` is the same query with every distance capped at 1. A
   query whose rewritten terms are all shorter builds the one step it built before. `fuzzy:1` holds the documents
   within one edit of every rewritten word. A document one edit off on two words ties, at the tier, with one that is
   one edit off on one word and exact on the other. BM25F and the edit-distance weight order them within the step,
   and the exact term weighs more there. The sum would need one step per distribution of edits over the words, which
   grows combinatorially, or a per-hit key (item 6).
3. **Exact above completed, at L3, unless L2 took those documents.** When L3 expands a term besides the last one, L3
   builds `prefix:last` first: the last term alone expanded, every other word as typed. It is not built when the
   cascade built L2's last rung (one term) and did not skip it. That rung takes every document holding one of the
   query's terms as typed, so the tier step would match nothing and only cost a search. The decision reads the steps
   built, not the plan. A query L2 builds no step for still gets the tier: one required clause, such as a word the
   analyzer splits (`spider-ma` is `spider` and `ma`), or a programmatic `bool` nested in one `must` clause. So does a
   query whose L2 rung was skipped for the limits, and any plan without L2, such as search as you type. With the
   default levels, a multi-clause query's L3 hits already need each of their words completed. The same snapshot,
   query and limits build the same steps, so every page of a query makes the same decision.
4. **Each tier step states its own text.** `fuzzy:1` finds its corrections within the capped distances. When it is
   the hit step (ADR-0043), `corrected` and `effective` name terms one edit away. `prefix:last` states L0's text, as
   `prefix` does. `level` stays the level's name (`fuzzy`, `prefix`), and `applied` lists the tier step
   (`["fuzzy:1", "fuzzy"]`), with `:cut` or `:skipped` as any step's entry.
5. **Interactions follow from item 1.** The steps lead the rank key before the request's sort, so the tiers order
   hits under any sort, and a field sort orders each tier step on its own. A cursor records a tier step's index in
   `StepCursor` as any step's. Its relaxation settings (ADR-0045) do not change, since the tier steps follow from the
   levels. Pruning is each step's own (SPEC §7.3, the score-led tie-break of ROADMAP 7.4.11), and no threshold
   crosses a step, so no pruning is lost. An explained hit's `relaxed` node names the tier step's `applied`.
6. **What is not tiered, and why.** L0's stemming is not: the index holds the stems alone, so a word matched as typed
   cannot be told from one matched by its stem without an unstemmed form per token, an on-disk format change
   (ROADMAP 8.2.10). L1 is not: L0 took every document holding all the runs as written, so every L1 hit lacks the
   original of some run, and counting runs per hit is item 2's per-hit key (ROADMAP 8.2.11). L2 already is, by its
   rungs.

## Consequences

- Relevance, on the 7.8 suite (`tests/relevance.rs`): the judged queries of ROADMAP 7.8.2 all have hits at L0, so
  their nDCG@10 is unchanged (mean 0.8824) and the 7.8.4 gate passes. A typo suite was added beside them
  (`typo_corpus.json`, `typo_judgments.json`, its own baseline under the same gate). It has five queries the tiers
  decide, four misspellings each one edit from the relevant documents' term and two from a decoy's, and `red sho`
  without L2. Its mean nDCG@10 is 0.6786 before the tiers and 0.9835 with them: 1.0000 on the four misspellings
  (from 0.6934, 0.6934, 0.6309 and 0.6653), and `red sho` from 0.7098 to 0.9173.
- A query that qualifies runs one more step: an extra planning and search, and at L4 its corrections walk the terms
  once more. Benchmarks `search_relaxed/l3/tiers` and `search_relaxed/l4/tiers` measure it (docs/benchmarks.md). A
  qualifying L4 query that finds nothing costs about 207 µs more (156 to 363 µs). An L3 query whose tier step finds
  the hits stops there and is 43% faster (39.8 to 22.5 ms). Queries that do not qualify move within the build's
  layout noise: within 1.4% with one codegen unit.
  Queries that do not qualify (every rewritten word shorter than 8 characters, or L3 after L2's one-term rung) build the same steps
  as before.
- The tier step counts in full against §7.1.3's limits in the steps that exclude it. L4's `fuzzy` then excludes
  `fuzzy:1`, whose `fuzzy` queries count their expansions again, so a query with many long words reaches
  `fuzzy:cut` or `fuzzy:skipped` sooner. The steps are not nested, since each `fuzzy` keeps its own 50 terms by
  `df`. Making them nested by construction would let the later steps exclude the latest one only (ROADMAP 8.2.12).
- `applied` gains `fuzzy:1` and `prefix:last`, and step indices shift by one after a tier step. Both are visible to
  clients that read them.

## Alternatives considered

- **A per-hit tier key, sum of edits then completed words, for score-led sorts only.** It is finer: a word exact and
  one off ranks above two words one off. But it needs a tier method across every matcher, a leading key in the
  collector and the cursor, and no score pruning in tiered steps, or a new tier-aware pruning. It is kept as ROADMAP
  8.2.11, should judged queries show the sum gains over the steps.
- **A score component.** ADR-0037's argument: an offset cannot keep every score's order, and it rounds away the low
  bits. The edit-distance weight is already one, and a weight is what let a two-edit decoy win.
- **`prefix:last` after L2's one-term rung too.** It matches nothing there, and it would cost every multi-clause query
  that reaches L3 one more search.
- **Deciding from the plan** (whether `levels` holds L2), as first written. A query L2 builds no step for (one split
  word) then lost the tier under the default levels, though its tier step is not empty.
