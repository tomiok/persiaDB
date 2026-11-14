# ADR-0042: L4 `fuzzy`: one step over L3's terms, a length-based distance, and corrections of missing words

- Status: proposed
- Date: 2026-11-13
- Related: SPEC §7.1, §7.1.3, §7.2, §7.6; ROADMAP 7.4.12, 8.1.10, 8.1.14, 8.1.19, 8.2.2.5, 8.2.3, 8.2.4, 8.2.7;
  ADR-0037, ADR-0038, ADR-0039, ADR-0040

## Context

SPEC §7.6's L4 row says: "Edit distance 1 (len 4–7) / 2 (len ≥ 8) via Levenshtein automaton ∩ FST (cap expansions,
default 50); corrected terms reported". Since ROADMAP 7.4.12, planning already expands a `fuzzy` query over every
view's terms (an FST walk intersected with an optimal string alignment automaton per segment) and keeps the
`max_expansions` with the highest collection `df`, and §7.1.3 cuts expansions to fit the limits. So, as at L3, the
level mostly has to build the `fuzzy` queries. Several things were still open:

- The automaton walked a full table row per byte, allocating it: O(query length) per byte, for terms up to 32 KiB.
- Which terms L4 rewrites, and what "len" counts: bytes or characters, raw or analyzed.
- Whether L4 is one step, and whether it builds on L0 or on L2's loosest step (ROADMAP 8.1.10 settled: L0).
- Which terms are "corrected", to what, and what `effective` then says (ROADMAP 8.1.14: L4 must state one).

## Decision

1. **A banded automaton** (ROADMAP 8.2.4.1). As `D[i][j] ≥ |i − j|`, only the `2 · distance + 1` cells around the
   diagonal of each row can be within the distance. The state keeps those cells, and the band of the row before
   for a swap, in fixed arrays: a byte costs O(distance) and allocates nothing. A 1 KiB term at distance 2 plans
   in 103 µs instead of 4.94 ms (`search_expansion_collection/fuzzy_long/plan`), and `fuzzy/plan` gains 20%. A DFA
   compiled ahead (Schulz–Mihov) would make each byte one table lookup, but it costs a build per query, and the band
   already makes planning a small part of a fuzzy search.
2. **One step, `fuzzy`, from L0's query**, over L3's terms (`relax::words`, shared with L3): each field's analyzed
   term of a word clause, in `text` fields whose analyzer does not stack tokens, or a programmatic query's `term` and
   `combined` leaves of its root `must`. Excluded clauses, phrases, `field:` clauses, filters and `keyword` fields stay
   exact, for ADR-0040's reasons. A stacking field's grams would sum their scores, and its whole-word gram is a term a
   `fuzzy` over the other fields already reaches.
3. **The length is the analyzed term's, in Unicode scalar values** (ROADMAP 8.2.4.4), as at L3 (ADR-0040 item 4):
   distance 1 from 4 to 7, 2 from 8, none below 4. The automaton counts characters, so a byte count would give a
   two-letter word of accented characters a distance (`ñu` is 3 bytes) and an accented seven-letter word two edits.
   Each field counts its own analyzed term: `connections` is 11 in a `standard` field (distance 2) and `connect`, 7,
   under `en` (distance 1). Unlike L3, the last term has no rule of its own: a short last word is not a typo L4 can
   tell from a word.
4. **Every term that qualifies is rewritten**, whether or not some document holds it (SPEC §7.6: "L4 corrects every
   term"). A `fuzzy` keeps the term itself at weight 1 (§7.1), so a word that matched as typed still ranks its exact
   documents above its corrections at the same frequency and length.
5. **The cap is planning's**: 50 terms by collection `df`, ties by bytes, as for any `fuzzy` (SPEC §7.1); within the
   limits, `fuzzy:cut` or `fuzzy:skipped` (§7.1.3). No new rule was needed. Being one step, L4 has no earlier step of
   its own for a step to stand for.
6. **Corrections are the missing words** (ROADMAP 8.2.4.3). A required word of one token that no field holds
   (collection `df` 0) is corrected to the term its fields' `fuzzy` queries keep first: the highest `df`, then
   byte order, ties between fields to the earlier one. That term is searched by the step under any cut, since §7.1.3
   keeps the most frequent terms, so `effective` never names a term the step did not search. A word some document
   holds is widened but not reported: "did you mean" for a word that exists would mostly guess. `original` is the
   word as written. `corrected` is the index's term, so under a stemmer it is a stem (`zapatil`): the index has no
   surface form to give back. A word of several tokens (`e-mial`) is not reported: the compiled clause has no text
   per token, so `effective` could not replace just one.
7. **Found when the level builds, not when the step is planned.** L4 walks each missing word's fields with
   planning's own walk (`search::expand`, keeping one term) over the views' terms (`LevelInput::terms`). A cursor's
   page builds the step without always searching it, and every page must state the same `corrected` (SPEC §7.6),
   so taking them from the step's plan would leave some pages without them. The walk runs under the request's
   deadline and token and, once it has started, the budget. A budget that passes during it truncates the cascade,
   as it does inside a step.
8. **ROADMAP 8.2.2.5: a `df`-0 term is L2's least important** (below a stopword). L4 runs only when L2 found too few
   hits, so it cannot fix an L2 `effective` that names a word no hit holds. Ranking such a word last makes L2 drop it
   first and report the words its hits can hold.

## Consequences

- A query whose L0, L1, L2 and L3 find too few hits now relaxes to L4 by default; the default plan's L5 still
  builds nothing (ROADMAP 8.2.5).
- `search_relaxed/l4` measures the step: L0, then a two-word query whose second word is a typo. A missing word is walked twice,
  once for its correction and once when the step is planned, over the fields that rewrite it.
- The differential suite (`tests/differential.rs`) does not model L4: its reference model has no `prefix` or `fuzzy`
  query at all. ROADMAP 8.1.21 adds L4 beside 8.1.19's L3; meanwhile `tests/relax_fuzzy.rs` checks the step's matches
  against an oracle over random corpora.

## Alternatives considered

- **A DFA built per query**: faster bytes, slower builds; the band is enough while planning is a small share.
- **Two steps** (distance 1, then 2): better ordering by edits, but one more search and a full exclusion (as
  ADR-0040 item 1). The edit-distance weight already ranks fewer edits higher within the step, and ROADMAP 8.2.7
  makes edits a ranking signal.
- **Rewrite only the words no document holds**: a word that exists but is the wrong one (`shoe` for `shoes`) would
  never be widened, and SPEC §7.6 says every term.
- **Byte length**: see item 3.
- **Corrections from the step's plan**: no second walk, but a cursor page that does not search the step would state
  none (item 7).
- **Correct every word, held or not, to its most frequent neighbour** (Elasticsearch's "popular" suggest mode):
  `shoe` would read `shoes` whenever `shoes` is more frequent, though the user's word matched documents.
- **Leave a `df`-0 term to L4 (ROADMAP 8.2.2.5's other option)**: see item 8.
