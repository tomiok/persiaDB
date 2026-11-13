# ADR-0040: L3 `prefix`: one step over the analyzed terms, the last one always, stacking fields exact

- Status: accepted
- Date: 2026-11-12
- Related: SPEC §6.3.1, §6.3.4, §7.1, §7.1.1, §7.1.3, §7.6; ROADMAP 7.4.12, 8.1.8, 8.1.14, 8.2.2.4, 8.2.3, 8.2.7;
  ADR-0037, ADR-0038, ADR-0039

## Context

SPEC §7.6's L3 row says: "Prefix-expand the last term (and others if len ≥ 3) via FST range scan (cap expansions,
default 50)". Since ROADMAP 7.4.12, planning already expands a `prefix` query over every view's terms (an FST range per
segment) and keeps the `max_expansions` with the highest collection `df`. §7.1.3 cuts expansions to fit the limits. So
L3 only has to build the `prefix` queries. Several things were still open:

- Is L3 one step, or two (the last term first, then the others)?
- Does it build on L0, or on L2's loosest step?
- Are expansions kept by `df` or in FST order?
- Is "len ≥ 3" counted on the analyzed term or on the raw word?
- What about a field whose analyzer stacks tokens (`edge_ngram`)? Its index already holds every prefix of each word.
- Does a programmatic query relax at L3?

## Decision

1. **One step, `prefix`** (chosen with the user, 2026-11-12). It expands the last term and every other term of at
   least 3 characters together. That matches the SPEC table, and it costs one search and one exclusion. Two steps
   would rank completions of the last word first, but they would not be nested: the `df` cap can drop the exact term,
   and the limits cut the two steps differently. So the second step would still exclude the first in full (ADR-0037
   item 2, ROADMAP 8.2.2.4). With one step, L3 has nothing to nest.
2. **From L0's query** (SPEC §7.6: every level builds from L0). Every term stays required; L3 does not combine with
   L2's dropped terms.
3. **Kept by collection `df`**, ties by bytes, as SPEC §7.1 already states for any `prefix`. Under the limits, the
   `max_expansions` are lowered and each prefix keeps its most frequent terms (§7.1.3). No new rule was needed.
4. **The length is the analyzed term's, in Unicode scalar values** (chosen with the user). The rule bounds fan-out,
   and fan-out depends on the bytes scanned: the analyzed term (`running` under `en` is `run`). It also applies per
   part of a split word (`e-mail` gives `e` and `mail`) and per field, as each field analyzes the word its own way.
   The last term is expanded whatever its length: it is the word a user may still be typing. It is the last part of
   the last required clause when that clause is a word (of a word the fields split apart, each field's last token).
5. **Stacking fields stay exact** (chosen with the user). In an `edge_ngram` field, `prefix(lap)` matches the same
   documents as `term(lap)`. It would also sum the scores of `lap`, `lapt`, `lapto`, and spend the 50-term cap on
   grams. The search reads which fields stack from the analyzers its collection's memtable indexes with
   (`MemtableView::stacks_tokens`), so it also holds for programmatic queries. With every field stacking, L3 builds
   no step. SPEC §6.3.4's `laptop*` sentence changed with it.
6. **Words only.** Excluded clauses are not expanded: a wider exclusion would exclude more. Phrases (even one of a
   single token, which compiles as a word) and `field:` clauses are not either, since the user asked for that text
   exactly. `CompiledClause::kind` (`ClauseKind`: word, phrase, field) records the form, as the parts alone do not
   tell a quoted word from a bare one.
7. **A `combined` becomes a `bool` of `should`s.** The AST has no multi-field prefix. The fields kept exact stay one
   `combined`, and each expanded field gets its own `prefix`. A `prefix` already scores with its field's schema
   `boost` as the field weight (SPEC §7.2), as a `term` does, so the step adds a `boost` only when the word's
   `fields` weight differs from it: their ratio, clamped to the range a `boost` allows (SPEC §7.1: at most 1000, and
   the least positive `f64` when a tiny weight rounds it to 0). (Boosting by the weight itself, as first
   built, counted the schema boost twice; the batch review found it.) A `fields` weight then scales the field's
   score instead of entering its saturation as `w_f`. A document matches when any field matches. Each expanded field scores its prefix with the largest kept `df` (§7.1), so BM25F's field
   combination is lost for the expanded fields: their scores add up instead of sharing one saturation.
8. **Programmatic queries relax too** (chosen with the user). The `term` and `combined` leaves of a root `bool`'s
   `must` clauses (through nested `bool`s and `boost`s) are expanded, the last `must` clause holding the last term. A
   `term` or `combined` alone is its own last term. Only `text` fields expand (a `keyword` value is an identifier).
   Within a nested `bool`, its last `must` clause holds the last term, and so does each `should` clause (each is one
   field's reading of the same word, as the compiler builds them). Its `must_not`, `filter` and phrases stay as
   they are. A root with `should` clauses or a `minimum_should_match` is not relaxed, as at L2. For a compiled
   query too, the root's whole `must_not` stays, as at L2 (L1 now reads it the same way).
9. **`effective` is L0's text**: every clause is kept, as at L1. Building the step reads no statistics; planning it reads the `df`s that keep
   the expansions (§7.1).

## Consequences

- A one-term programmatic query now relaxes past L0 by default (L3 builds a step where L1 and L2 built none).
- Expanded fields score as separate `prefix` queries. An exact match and a completion of the same word score the
  same at one frequency and length; ROADMAP 8.2.7 ranks exactness first.
- A step expands each term to up to 50 terms, so a long query can exceed `max_expansions` and is cut, or skipped
  when a prefix cannot keep one term each (`prefix:cut`, `prefix:skipped`).
- No schema can name an analyzer that stacks tokens yet (schemas name built-in analyzers, none of which stacks). Rule
  5 is tested by giving a collection's memtable such an analyzer directly.
- The last term expands at any length, so a one- or two-character last word makes each segment's FST range scan
  walk every term starting with it before the 50 kept by `df` are chosen: about 23 ms in `search_relaxed/l3` against
  about 1.4 ms for an L2 step. ROADMAP 8.1.20 is to bound it.
- Bench `search_relaxed/l3` measures the step: L0, then a two-word query whose last word is a prefix.

## Alternatives considered

- **Two steps** (last term, then the rest): better ordering, but one more search and a full exclusion (item 1).
- **Raw word length**: one length per clause, but it says nothing about the scanned prefix after stemming or folding.
- **Expand stacking fields anyway**: what §6.3.4 first said. It would add up the grams' scores, and with the cap it
  could keep only grams of the first words.
- **Skip programmatic queries**, as L1 does: they then had no typo-free completion. Not chosen (item 8).
- **Keep the exact term beside the prefix** (`bool { should: [term, prefix] }`), so the step would contain L0: that
  scores a document holding the exact term twice, and L0's hits are excluded from the step anyway.
- **A field weight on `prefix`** (the `w_f` a `combined` carries), so a `fields` override would enter saturation as at
  L0: an AST change for one level. The ratio boost (item 7) keeps the AST, and equals L0's weighting for the default
  fields, whose weights are the schema boosts.
