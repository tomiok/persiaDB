---
title: "Synonym search and prefix search"
description: "How synonym search and prefix search work in Persia DB: L1 synonym groups, L3 FST prefix expansion in Rust, and which steps are still not built."
head:
  - tag: title
    content: "Synonym search and prefix search in Rust | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Synonym search and prefix search in Rust | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Synonym search and prefix search in Rust | PersiaDB"
  - tag: script
    attrs:
      type: application/ld+json
    content: |
      {
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "name": "Synonym search and prefix search",
        "url": "https://tomiok.github.io/persiaDB/guide/synonym-and-prefix-search/",
        "mainEntity": [
          {
            "@type": "Question",
            "name": "Does Persia DB do synonym search in Rust today?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "As library code, yes. L1 of CollectionViews::search_relaxed builds one synonyms step from the collection schema groups. A matched run of words matches the run or one of its alternatives. Phrases, field clauses, and excluded clauses stay exact. There is no search method on persia::Db. %%RELAXATION_CELL%%"
            }
          },
          {
            "@type": "Question",
            "name": "Does Persia DB do prefix search today?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "As a relaxation step, yes. L3 builds one prefix step. The last analyzed term, and other analyzed terms of at least 3 characters, become prefixes of at most 50 terms, kept by collection document frequency and expanded over each view with an FST range. Phrases, field clauses, excluded clauses, filters, and edge_ngram fields stay exact. A suggest completion API is not written. There is no search method on persia::Db."
            }
          },
          {
            "@type": "Question",
            "name": "Can I call Db::open and search with synonyms or a prefix?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. persia::Db is not in the crate, and Database has no search method. The engine call is CollectionViews::search_relaxed on a collection snapshot. Code samples that call Db::search on this site are illustrative."
            }
          }
        ]
      }
---

> **Status: pre-alpha.** %%PRE_ALPHA%%
> Counts below are read from [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) when the site is built:
> %%PROGRESS_SUMMARY%%.
> When this page and [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) disagree, the SPEC wins.
> Nothing here is a benchmark.

Synonym search and prefix search, in this engine, are two steps of [query relaxation](/guide/typo-tolerant-search/#query-relaxation) (SPEC §7.6). They run inside `CollectionViews::search_relaxed` when the exact query has fewer than `min_results` hits (default 1). L1 expands synonym groups. L3 expands prefixes. L2 `drop_terms` sits between them and is described on [Typo-tolerant search](/guide/typo-tolerant-search/).

%%RELAXATION_NOW%% There is still no search on the `persia` database handle (M10). `persia::Db` is not in the crate. `persia_engine::db::Database` has `open`, `put`, and `commit`, and no `search`.

## Synonym search in Rust

L1 is one step, listed as `synonyms` (ROADMAP 8.2.1, [ADR-0039](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0039-l1-synonyms-runs-alternatives-and-cuts.md)). It does not rewrite the user's text. `effective` stays the text L0 compiled, because every clause is kept. `corrected` stays empty until fuzzy matching synthesizes a correction (ROADMAP 8.2.4.3), which is not written.

### Where the groups live

Groups are a collection setting, `synonyms`, stored as given and analyzed when a query is compiled (SPEC §6.2.1, §6.3.2). A group has 2 to 64 distinct terms. A collection holds at most 10,000 groups, and at most 1 MiB of term bytes. A term is 1 to 256 bytes. The setting is not part of the schema hash, so changing it does not force a reindex (SPEC §6.2).

This is the shape in SPEC §6.2. It is schema JSON, not a search call:

```json
"synonyms": [["tv", "television"]]
```

Each term is analyzed with the field's query-time analyzer, without filters that stack tokens such as `edge_ngram` (SPEC §6.3.4). A term that yields no tokens is ignored. Terms that analyze to the same tokens are one term. A group that collapses to a single term, such as `["New York", "new york"]` under lowercasing, has nothing to expand to and never matches.

`SynonymMap` in `persia-analysis` is that lookup (ROADMAP 4.2.5). The query compiler runs it and stores the runs on `Compiled::synonyms`. L1 only assembles the step. It reads no collection statistics.

### How a synonym step is built

The compiler scans each default field's analysis of the required plain words. An excluded clause (`-`), a phrase, or a `field:` clause ends a run, so a match never spans one and none of those is expanded. Each word is analyzed on its own, and numbered after the words before it, so a run reads as one analysis of those words.

A match covers whole clauses. It starts at a word's first token and ends at a word's last, so `mail` inside `e-mail` is not replaced on its own. From each such start it takes the longest term that ends on a word boundary. Matches do not overlap. The alternatives are the other terms of every group that contains the match, in group order, then term order, each once, up to 50 per matched term (SPEC §7.6).

Fields that match the same clauses share those alternatives. An overlapping match from a later default field is dropped. Each alternative is compiled over every default field as the quoted phrase `"<term>"` would be: a phrase where the field stores positions, every token required where it does not, and one word when the alternative is one token. A multi-word alternative such as `television set` therefore stays a phrase. Requiring those words in any order would also match `set of television`.

The step is L0's query with each run required as `bool { should: [the run, its alternatives] }`. A document matches the run or any alternative. BM25F sums the ones it holds. L0's other required clauses stay required. A query with no such run builds no L1 step. A programmatic query builds none either: it has no user text to scan.

### What stays exact

Excluded clauses are not expanded. More alternatives there would exclude more documents, which is the opposite of relaxing. Phrases and `field:` clauses stay as the user wrote them. Filters stay strict. `FilterRelaxation::LastResort` (`relax.filters: "last_resort"`) does not run (ROADMAP 8.2.6).

A request filter joins a compiled query at the root `bool`'s `filter` (SPEC §7.1, ROADMAP 8.1.17). L1 keeps that filter, and the root's `must_not`. [Faceted search](/guide/faceted-search/) is the filter DSL and how a filter is evaluated. Filter clauses do not add to the BM25F score.

A document already returned by an earlier step is excluded from this step, so it is a hit once, at the earlier step's score. L1 builds one step. It is not a nested ladder, so later steps exclude that step in full (SPEC §7.6).

### Fitting alternatives into query limits

`QueryLimits` holds the step beside the earlier steps it excludes (SPEC §7.1.3, ROADMAP 8.1.13). Each run keeps a prefix of its alternatives, in group order, then term order. Every list is lowered to the highest count that fits, longest first, and none below one. Room that is left is handed out one alternative at a time, in text order. A step that lost alternatives is listed as `synonyms:cut`. A step that cannot fit with one alternative per run is listed as `synonyms:skipped` and is not searched.

The compiler also refuses to compile more alternatives than the limits could hold: at most `max_terms − 2 · n` in all, where `n` is the query's term count. A run whose share left an alternative out is still `synonyms:cut` when the step later fits as compiled. A search held to larger limits than the compilation sees only the alternatives that were compiled.

## Prefix search

L3 is one step, listed as `prefix` (ROADMAP 8.2.3, [ADR-0040](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0040-l3-prefix-one-step-over-analyzed-terms.md)). It is built from L0's query. It does not continue from the terms L2 dropped. Every required word stays required. `effective` is again L0's text.

This is search relaxation, not a completion list. A `suggest` API that returns terms starting with a prefix is specified separately (ROADMAP 18) and is not written.

### Which terms become prefixes

A term here is one field's analyzed form of a word. The length is that analyzed term, in Unicode scalar values. Under the `en` analyzer, `running` is `run`, which is 3 characters, so it can be expanded even when it is not the last word. A shorter analyzed term is expanded only when it is the last term. The last term is expanded at any length: it is the word a user may still be typing. It is the last token of the last required word, per field when the fields split that word (`e-mail` yields `e` and `mail`).

The same rule applies to a programmatic query. L3 expands `term` and `combined` leaves of a root `bool`'s `must` clauses, including through nested `bool`s and `boost`s, or a `term` or `combined` that is the whole query. Only `text` fields expand. A `keyword` value stays exact. A root that has `should` clauses, or a `minimum_should_match` other than 0, is left alone, as L2 leaves it. A one-term query skips L2 and can still match at L3.

### How a prefix is expanded

Each expanded term becomes a `prefix` query with `max_expansions` 50 (`DEFAULT_MAX_EXPANSIONS`). Planning expands it once, over the terms of that field in every view the plan runs on (each segment and the memtable). The walk is an FST range (SPEC §7.1, ROADMAP 7.4.12). The terms kept are the ones with the highest collection document frequency, ties broken by bytes, at most 50. Every view then scores those same terms. Their shared idf is the document frequency of the most frequent term kept. A prefix score is not multiplied by an edit distance. That weight belongs to fuzzy matching, which does not build a cascade step.

An expanded `combined` clause, one word over several text fields, becomes a `bool` of `should` clauses: the fields that stay exact remain one `combined`, and each expanded field gets its own `prefix`. A document matches the word when an exact field holds the term, or an expanded field holds a term that starts with it. A `prefix` scores with that field's schema boost, the same field weight a `term` uses. When the query set a different `fields` weight, the step scales the prefix by the ratio of the two, clamped to the range a `boost` allows (SPEC §7.1).

Building the step reads no statistics. Planning reads the document frequencies that choose the 50 terms.

### What stays an exact term

Phrases stay exact, including a phrase of one token. `field:` clauses stay exact. Excluded clauses stay exact: a wider exclusion would exclude more. Filters stay on the step, including a request filter joined at the root (ROADMAP 8.1.17).

A field whose analyzer stacks tokens stays exact. `edge_ngram` is that filter (SPEC §6.3.1). Its index already holds each word's prefixes, so a further prefix would match the same documents and add the grams' scores together. No built-in analyzer stacks tokens. The search reads the flag from the analyzers the collection's memtable indexes with, so a programmatic query follows the same rule. When every field stacks, L3 builds no step.

### When the prefix step is cut or skipped

`QueryLimits` holds the step the same way it holds any later relaxation step (SPEC §7.1.3). If the prefixes do not fit beside the steps they exclude, their `max_expansions` are lowered, largest first, and each prefix keeps its most frequent terms. The step is listed as `prefix:cut`. If one term each still does not fit, the step is listed as `prefix:skipped` and is not searched. L3 is a single step, so it has no earlier step of its own to stand in for.

A query that expands nothing skips the level and is not listed in `applied`.

## What is still not built

| Piece | Status |
|---|---|
| L1 `synonyms` and L3 `prefix` | Library code, as this page describes. %%RELAXATION_CELL%% |
| L2 `drop_terms` | Library code. [How terms are dropped](/guide/typo-tolerant-search/#how-terms-are-dropped). |
| L4 `fuzzy` | The cascade builds no step (ROADMAP 8.2.4). An explicit `fuzzy` clause can still be planned over collection terms (ROADMAP 7.4.12). `Relaxation.corrected` stays empty (ROADMAP 8.2.4.3). A typo is not rewritten. |
| L5 `fallback` | Not written (ROADMAP 8.2.5). `Fallback::None` is the schema value that would allow an empty page. |
| Relaxing filters | `last_resort` does not run (ROADMAP 8.2.6). |
| `suggest` completions | Not written (ROADMAP 18). L3 is not that API. |
| `persia::Db::search` | Not in the crate (M10). |

A query can still come back empty. L4 and L5 build nothing, so a one-term query that is not a synonym and not a prefix of a kept term has nowhere left to go. A multi-term query returns hits from L1, L2, or L3 only when some document holds an alternative, enough of the terms, or a kept prefix. `RelaxOptions::disabled`, and a collection whose `RelaxationConfig.enabled` is false, run L0 only. The zero-result regression gate is not written (ROADMAP 8.4).

[Getting started](/guide/getting-started/) separates `CollectionViews::search_relaxed` from the illustrative `Db::search` samples. [What is an embedded search database?](/guide/embedded-search/) is the library shape those samples are aiming at.

## FAQ

### Does Persia DB do synonym search in Rust today?

As library code, yes. L1 of `CollectionViews::search_relaxed` builds one `synonyms` step from the collection's schema groups. A matched run of words matches the run or one of its alternatives. Phrases, `field:` clauses, and excluded clauses stay exact. There is no search method on `persia::Db`. %%RELAXATION_CELL%%

### Does Persia DB do prefix search today?

As a relaxation step, yes. L3 builds one `prefix` step. The last analyzed term, and other analyzed terms of at least 3 characters, become prefixes of at most 50 terms, kept by collection document frequency and expanded over each view with an FST range. Phrases, `field:` clauses, excluded clauses, filters, and `edge_ngram` fields stay exact. A `suggest` completion API is not written. There is no search method on `persia::Db`.

### Can I call Db::open and search with synonyms or a prefix?

No. `persia::Db` is not in the crate, and `Database` has no `search` method. The engine call is `CollectionViews::search_relaxed` on a collection snapshot. Code samples that call `Db::search` on this site are illustrative.

## Read next

- [Typo-tolerant search](/guide/typo-tolerant-search/) — the whole cascade, `minimum_should_match`, and how L2 drops terms.
- [Faceted search](/guide/faceted-search/) — filters, facets, sort, cursor pages, deadlines, and explain. Filters stay strict while L1 and L3 run.
- [What is an embedded search database?](/guide/embedded-search/) — in-process full-text search, one file, and no `persia::Db` yet.
- [FAQ](/guide/faq/) — production readiness, the file format, and the license.
- [Feature reference](/guide/features/#zero-results-relaxation) — the level table and the schema sketch that carries `synonyms`.
- [Getting started](/guide/getting-started/) — crates you can call, and the API that is still illustrative.
- [Comparison](/guide/comparison/) — next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
