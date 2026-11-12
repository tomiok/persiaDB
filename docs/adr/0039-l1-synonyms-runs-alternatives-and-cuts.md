# ADR-0039: L1 `synonyms`: runs of whole clauses, phrase alternatives, one step, and cuts in group order

- Status: proposed
- Date: 2026-11-11
- Related: SPEC §6.3.2, §6.3.4, §7.1, §7.1.3, §7.6; ROADMAP 8.1.8, 8.1.13, 8.1.14, 8.2.1; ADR-0037, ADR-0038

## Context

SPEC §7.6's L1 expands a query's terms with the collection's synonym groups, "OR within group", with at most 50
expansions per matched term. §6.3.2 already defined the lookup over an analyzed token stream: groups analyzed per
field, the longest term with equal texts and position gaps, no overlap, and alternatives in group, then term order.
Several things were still open:
- how a multi-word alternative is searched;
- whether L1 is one step or one per matched term;
- which alternatives survive when the step does not fit `QueryLimits` (ROADMAP 8.1.13);
- what "the analyzed query" is, when the compiler analyzes each clause on its own, for each default field;
- where the scan runs, given that the cascade's levels get no analyzers.

## Decision

The user decided items 1, 2 and 6 (2026-11-11). The others follow from §6.3.2 and ADR-0037/0038.

1. **A multi-word alternative is a phrase.** Each alternative is the term as written, compiled over the default
   fields as the quoted phrase `"<term>"` would be. That is a phrase in a field with positions, every term required in
   a field without them, and one word over the fields when it is one token. Synonyms of several words are mostly fixed
   expressions (`new york`, `television set`), and requiring the words in any order would match `set of television`.
2. **One step.** L1 builds one step (`synonyms`), and every run is ORed with its alternatives in it. A step per run
   would multiply the steps, and each step excludes every earlier one (ADR-0037 item 2). That costs `max_terms`
   quadratically, while ranking by how many runs a document matched as written adds little.
3. **The scan runs at compile time** (`Compiled::synonyms`). It stores each run's clauses and its compiled
   alternatives, as 8.1.7 stored the clauses' spans and stopword flags for later levels. The compiler holds the
   analyzers and the raw groups. Its per-field maps are built lazily, on the first compilation that searches the
   field. L1 then only assembles the step, reads no statistics, and needs no new input. SPEC §6.3.2 already said that
   "the query compiler ORs the match with its alternatives".
4. **What is scanned.** Each default field scans its own analysis of the required plain words. An excluded clause,
   a phrase or a `field:` clause ends a run, so a match never spans one, and none is expanded. Expanding an exclusion
   would exclude more, which is the opposite of relaxing. Each word is analyzed alone, but numbered after the words
   before it by the positions they took in the tokenizer (`Analyzer::analyze_query_counted`). Every removed token
   keeps its gap, so a run reads as one analysis of its words would, one space apart.
5. **Whole clauses.** A match starts at a word's first token and ends at a word's last
   (`SynonymMap::find_between`), so it replaces clauses, not part of a word (`mail` in `e-mail`). Each boundary takes
   the longest term that ends on a boundary, so a term ending inside a word does not hide a shorter one that ends with
   the word.
6. **Cuts keep the order the groups give** (ROADMAP 8.1.13). Each run keeps a prefix of its alternatives, in group,
   then term order. Every list is lowered to the highest count that fits, the longest first and none below 1. The
   room left goes one each, in text order, to the lists that count cut, as `prefix` and `fuzzy` expansions are shared
   (ADR-0037 item 2). A cut step is `synonyms:cut`. One that does not fit with one alternative each is
   `synonyms:skipped`. Cutting needs no `df` read. The groups' order is the author's statement of preference, and
   the 50-cap already relies on it.
7. **Fields share runs.** Fields that match the same clauses share their alternatives. Each alternative is
   reported at its place in the group it expands from, so the union keeps group, then term order. A different
   overlapping match from a later default field is dropped. An alternative is compiled over every default field,
   also one whose analysis did not match the run. It stands for what the user would have typed, and the phrase is
   analyzed by each field as typed text would be.
8. **Bounded at compile time.** L1's step holds L0's terms twice: its own clauses, plus L0, which it excludes. It
   also holds at least one term per alternative. So the compilation keeps at most `max_terms − 2 · n` alternatives
   in all, `n` being L0's terms, shared as in item 6, and one each when that is below one per run, so the cascade
   lists the step skipped. It compiles only those, plus one more source per cut run, to tell a cut from duplicates.
   A text of a thousand matching words therefore costs about as much again as its L0 does. This bounds the work. It
   does not make the step fit, because an alternative of several words or over several fields takes more terms. The
   cascade still holds the step to the limits (item 6). A run whose share left out an alternative is marked
   `CompiledSynonym::cut`, and its step is listed `synonyms:cut` even when it then fits as it is.
9. **The step.** The step keeps L0's other required clauses, its excluded clauses and its filters. Its `effective` is
   L0's text (ROADMAP 8.1.14), because every clause is kept. A programmatic query builds no step: it has no text to
   scan, and its terms are already analyzed.

## Alternatives considered

- **Every word of an alternative required (AND).** This gives more recall, but `television set` would match any
  document that holds both words.
- **A step per run, or per alternative.** This gives finer ranking, but `max_terms` and planning grow quadratically in
  the steps (above).
- **Cut the alternatives fewest documents hold first** (`df`). This keeps more recall per term. But it needs a `df`
  read per alternative, it disagrees with the order the 50-cap keeps, and it would make the response depend on
  statistics in a second place.
- **Scan inside L1.** The level would need the analyzers, the default fields and the raw groups, so it would need a
  new input that every caller supplies (the facade, the server). That leaves less zero friction.
- **Analyze the run's text in one call.** A tokenizer that does not split at whitespace (`keyword`) would join
  clauses into one token, and a dropped clause's text would be read again.

## Consequences

- A query compiled with synonyms costs more than one without: about 41 µs against 2.5 µs for a run with 50
  alternatives (bench `compile/synonyms`). Most of that is analyzing the alternatives again, and it is paid when L0
  already has hits. ROADMAP 8.1.16 builds them lazily or caches them.
- The compile-time bound uses the compilation's limits. A search held to larger limits than the compilation's
  (`RelaxPlan::with_limits`) sees only the alternatives compiled. The cascade may then find the step fits as it is,
  at equal limits too (one default field, one-word alternatives). So the cut is carried on the compiled run
  (`CompiledSynonym::cut`), and the step is listed `synonyms:cut` either way.
- Fitting a cut checks the whole step for each count it tries (about `log2(longest list) + runs` checks).
  Computing it from each alternative's usage would cost one check. Runs are bounded by item 8.
- A clause that one field analyzes to nothing, for a reason other than a stopword (`!!!` under `standard`), adds no
  position there. Another field's match over it replaces that clause's requirement in every field. This is
  consistent with analyzing the run as one text.
- A collapsed compiled query (one clause, one part) with a filter joined at the root is ambiguous: `Compiled::parts`
  reads the whole root as the clause. ROADMAP 8.1.17 settles how a request's filter joins a compiled query.
