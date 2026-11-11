# ADR-0038: L2 `drop_terms`, a `minimum_should_match` ladder ordered by term importance

- Status: proposed
- Date: 2026-11-11
- Related: SPEC §7.1, §7.1.1, §7.2, §7.6; ROADMAP 8.1.6, 8.1.7, 8.1.10, 8.1.14, 8.2.2.1–8.2.2.5; ADR-0037

## Context

SPEC §7.6's L2 lowers how many of a query's terms a document must hold. Its row gave the ladder (100% → 75% → 50% →
1 term, `max(1, ⌊p · n / 100⌋)`) and said that "stopwords/low-IDF terms" are "dropped first", ordered by an
"IDF-importance". Three things were left open:
- what a `minimum_should_match` of `k` does with that order, when it matches any `k` of the terms;
- how importance combines IDF, position and a stopword flag;
- where the stopword flag comes from, when the language analyzers already remove stopwords from a compiled query.

ROADMAP 8.1.10 also left open whether later levels (L4 `fuzzy`) build on the full query or on L2's last step.

## Decision

1. **Any `k` of the terms.** A step of count `k` is `bool { should: terms, minimum_should_match: k }`, with L0's root
   `must_not` and `filter` clauses beside them (ROADMAP 8.1.6). A document holding any `k` of them matches, and
   BM25F (§7.2) ranks the ones holding rarer terms higher. This has the most recall of the shapes considered.
2. **What importance decides.** Importance does not decide which documents match. It orders the `should` clauses,
   most important first. It also picks the step's `effective` text (§7.6): the `k` most important terms and every
   excluded clause, in text order. That text names the terms the step keeps. It is not a query that compiles to the
   step's.
3. **Lexicographic importance.** A stopword is less important than any other term. Then a lower IDF is less
   important. Then a later position. There are no weights to tune, and a test can state the order. The IDF is about
   as BM25F scores the term: a `combined` from its largest `df`, a `phrase` the sum of its terms', a `bool` its
   `must` sum plus its best `should`.
4. **The stopword flag** (`CompiledClause::stopword`). A clause is a stopword when a default field analyzes it to no
   token because its analyzer's stopword filter removed one (`Analyzer::analyze_query_stopwords`, reported by
   `TokenFilter::removes_stopwords`). The language analyzers (`en`, `es`, `pt`, `fr`, `de`, `it`) have such a
   filter, after they normalize, so `the,`, `THE` and `ｔｈｅ` all count. Text in which the tokenizer finds no word
   (`—`) does not. The flag matters where another default field keeps the word, such as a `standard` title beside an
   `en` body.
5. **A term** is a required clause of a compiled query, whatever its parts, or a `must` clause of a programmatic root
   `bool` with no `should` and a `minimum_should_match` of 0. A query of fewer than 2 terms skips L2, before any
   `df` is read. L0 counts as the step before the ladder, at `n`, so 100% is never built: it is L0, which every
   later step excludes. A step is labeled by the rung that first gave its count, so 2 terms build one step,
   `drop_terms:75%`, of count 1. That matches §7.6's example.
6. **Every level builds on L0's query.** No level builds on an earlier level's steps, so L4 corrects every term
   (§7.6's example keeps both words). This settles ROADMAP 8.1.10.

## Alternatives considered

- **Ordered drop**: step `k` requires (`must`) the `k` most important terms. It is deterministic, and its `effective`
  is the query it runs. But it never matches a document that holds a different `k` terms, so it has less recall, and
  it does not use `minimum_should_match`.
- **A weighted importance**, such as `idf · position decay · stopword factor`. It would need constants that SPEC must
  fix and justify. Nothing measured yet would justify them.
- **The stopword lists matched against the raw word.** This misses words the analyzer normalizes (punctuation, case,
  compatibility forms), and it ties the list to a version of the analyzer.
- **Any empty analysis in a language field.** This flags text the tokenizer finds no word in (`—`), which another
  default field may keep as a real term.

## Consequences

- `effective` can name a term no hit holds. For example, a term in no document (`df` 0, often a typo) has the highest
  IDF, so it is kept unless it is a stopword (the flag is compared first). ROADMAP 8.2.2.5 decides whether that should change. L4's corrections are the other answer.
- Each step excludes L0 and every earlier step (ADR-0037 item 2). L2's steps are nested (each matches everything
  the one before matches), so with three L2 steps a long query reaches `max_terms` sooner (about 4·n terms by the
  last step), and planning grows with it. ROADMAP 8.2.2.4 excludes only a nested level's last step. Bench
  `search_relaxed/l2` and `search_relaxed/l2_every_step` measure this cost.
- Level transforms read the search's statistics and may fail. The deadline is checked before a level builds its
  steps.
