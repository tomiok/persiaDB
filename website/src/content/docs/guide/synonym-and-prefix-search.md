---
title: "Synonym search and prefix search"
description: "How synonym search and prefix search work in Persia DB: L1 synonym groups and L3 FST prefix expansion in Rust, through Collection::search. Fuzzy search and the fallback are separate steps."
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
              "text": "Yes. When the exact query has too few hits, L1 of the relaxation cascade that Collection::search runs builds one synonyms step from the collection's synonym groups. A matched run of words matches the run or one of its alternatives. Phrases, field clauses, and excluded clauses stay exact."
            }
          },
          {
            "@type": "Question",
            "name": "Does Persia DB do prefix search today?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "As a relaxation step, yes. L3 builds one prefix step (and, unless L2 already took those documents, a step that expands the last term alone first). The last analyzed term, and other analyzed terms of at least 3 characters, become prefixes of at most 50 indexed terms, kept by collection document frequency. Phrases, field clauses, excluded clauses, filters, and edge_ngram fields stay exact. A suggest completion API is not written."
            }
          },
          {
            "@type": "Question",
            "name": "Can I call Db::open and search with synonyms or a prefix?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes. Give the collection its synonyms when you create it, and Collection::search runs L1 and L3 whenever the exact query has fewer than min_results hits, unless the collection or the request turns relaxation off. A request can narrow the cascade with relax levels, for example synonyms or prefix only."
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

Synonym search and prefix search, in this engine, are two steps of [query relaxation](/guide/typo-tolerant-search/#query-relaxation) (SPEC §7.6). `Collection::search` and `Collection::search_json` run them when the exact query has fewer than `min_results` hits (default 1). L1 expands synonym groups. L3 expands prefixes. L2 `drop_terms` sits between them and is described on [Typo-tolerant search](/guide/typo-tolerant-search/).

%%RELAXATION_NOW%%

The sample opens a database in a temporary directory (the `tempfile` crate); any path works.

```rust
use persia::{Db, RelaxLevel};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("shop.persia"))?;
    let products = db.create_collection_json(
        r#"{"name": "products", "fields": {"title": {"type": "text"}},
            "synonyms": [["tv", "television"]]}"#,
    )?;
    products.put_json(r#"{"id": "p1", "title": "television stand"}"#)?;
    products.put_json(r#"{"id": "p2", "title": "red sandals"}"#)?;

    // L1: no document holds "tv"; its group's alternative "television" matches.
    let page = products.search_json(r#"{"q": "tv"}"#)?;
    assert_eq!(page.hits[0].document.id(), "p1");
    assert_eq!(page.relaxation.level, RelaxLevel::Synonyms);
    assert_eq!(page.relaxation.applied, ["synonyms"]);
    assert_eq!(page.relaxation.effective.as_deref(), Some("tv")); // L1 does not rewrite the text

    // L3: the last word is one a user may still be typing.
    let page = products.search_json(r#"{"q": "sand"}"#)?;
    assert_eq!(page.hits[0].document.id(), "p2");
    assert_eq!(page.relaxation.level, RelaxLevel::Prefix);
    assert_eq!(page.relaxation.applied, ["prefix"]);

    // Two words, with L3 alone: the last word expanded first, every other word as typed.
    let page = products.search_json(r#"{"q": "red sand", "relax": {"levels": ["prefix"]}}"#)?;
    assert_eq!(page.hits[0].document.id(), "p2");
    assert_eq!(page.relaxation.applied, ["prefix:last"]);

    db.close()?;
    Ok(())
}
```

## Synonym search in Rust

L1 is one step, listed as `synonyms` (ROADMAP 8.2.1, [ADR-0039](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0039-l1-synonyms-runs-alternatives-and-cuts.md)). It does not rewrite the user's text. `effective` stays the text L0 compiled, because every clause is kept. `corrected` stays empty on this step. [Fuzzy search](/guide/fuzzy-search/) states a correction when L4 is the step that found the hits (ROADMAP 8.2.4.3, 8.1.22).

### Where the groups live

Groups are a collection setting, `synonyms`, stored as given and analyzed when a query is compiled (SPEC §6.2.1, §6.3.2). In code they are `SchemaBuilder::synonyms`, and `CollectionSchema::synonyms` reads them back. A group has 2 to 64 distinct terms. A collection holds at most 10,000 groups, and at most 1 MiB of term bytes. A term is 1 to 256 bytes. The setting is not part of the schema hash, so changing it does not force a reindex (SPEC §6.2).

This is the shape in SPEC §6.2, a key of the collection JSON (`Db::create_collection_json`):

```json
"synonyms": [["tv", "television"]]
```

Each term is analyzed with the field's query-time analyzer, without filters that stack tokens such as `edge_ngram` (SPEC §6.3.4). [Text analysis](/guide/text-analysis/) is that pipeline: which built-ins stem, which drop stopwords, and which leave the text as written. A term that yields no tokens is ignored. Terms that analyze to the same tokens are one term. A group that collapses to a single term, such as `["New York", "new york"]` under lowercasing, has nothing to expand to and never matches.

The lookup runs when `q` is compiled (ROADMAP 4.2.5). L1 only assembles the step. It reads no collection statistics.

### How a synonym step is built

The compiler scans each default field's analysis of the required plain words. An excluded clause (`-`), a phrase, or a `field:` clause ends a run, so a match never spans one and none of those is expanded. Each word is analyzed on its own, and numbered after the words before it, so a run reads as one analysis of those words.

A match covers whole clauses. It starts at a word's first token and ends at a word's last, so `mail` inside `e-mail` is not replaced on its own. From each such start it takes the longest term that ends on a word boundary. Matches do not overlap. The alternatives are the other terms of every group that contains the match, in group order, then term order, each once, up to 50 per matched term (SPEC §7.6).

Fields that match the same clauses share those alternatives. An overlapping match from a later default field is dropped. Each alternative is searched over every default field as the quoted phrase `"<term>"` would be: a phrase where the field stores positions, every token required where it does not, and one word when the alternative is one token. A multi-word alternative such as `television set` therefore stays a phrase. Requiring those words in any order would also match `set of television`.

The step is L0's query with each run required as either the run or one of its alternatives. A document matches the run or any alternative. BM25F sums the ones it holds. L0's other required clauses stay required. A query with no such run builds no L1 step.

### What stays exact

Excluded clauses are not expanded. More alternatives there would exclude more documents, which is the opposite of relaxing. Phrases and `field:` clauses stay as the user wrote them. Filters stay strict. Only `relax.filters: "last_resort"` drops the `soft_filter`, in one last step after every other level (ROADMAP 8.2.6); the hard `filter` is never dropped.

The request's filters stay beside the query on every step (SPEC §7.1.1, ROADMAP 8.1.17), and L1 keeps the excluded clauses too. [Filtered full-text search](/guide/filtered-search/) is the DSL and how a filter joins the query. [Faceted search](/guide/faceted-search/) is facets and sort. Filters do not add to the BM25F score.

A document already returned by an earlier step is excluded from this step, so it is a hit once, at the earlier step's score. L1 builds one step. It is not a nested ladder, so later steps exclude that step in full (SPEC §7.6).

### Fitting alternatives into query limits

The step is held to the query limits beside the earlier steps it excludes (SPEC §7.1.3, ROADMAP 8.1.13). Each run keeps a prefix of its alternatives, in group order, then term order. Every list is lowered to the highest count that fits, longest first, and none below one. Room that is left is handed out one alternative at a time, in text order. A step that lost alternatives is listed as `synonyms:cut`. A step that cannot fit with one alternative per run is listed as `synonyms:skipped` and is not searched.

The compiler also refuses to compile more alternatives than the limits could hold: at most `max_terms − 2 · n` in all, where `n` is the query's term count. A run whose share left an alternative out is still `synonyms:cut` when the step later fits as compiled.

## Prefix search

L3 is one step, listed as `prefix` (ROADMAP 8.2.3, [ADR-0040](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0040-l3-prefix-one-step-over-analyzed-terms.md)), after a tier step `prefix:last` in the cases [Typo tiers](/guide/fuzzy-search/#typo-tiers) describes. It is built from L0's query. It does not continue from the terms L2 dropped. Every required word stays required. `effective` is again L0's text.

This is search relaxation, not a completion list. A `suggest` API that returns terms starting with a prefix is specified separately (ROADMAP 18) and is not written.

### Which terms become prefixes

A term here is one field's analyzed form of a word. The length is that analyzed term, in Unicode scalar values. Under the `en` analyzer, `running` is `run`, which is 3 characters, so it can be expanded even when it is not the last word. A shorter analyzed term is expanded only when it is the last term. The last term is expanded at any length: it is the word a user may still be typing. It is the last token of the last required word, per field when the fields split that word (`e-mail` yields `e` and `mail`).

Only `text` fields expand. A `keyword` value stays exact. A one-term query skips L2 and can still match at L3, as `sand` does in the sample above.

### How a prefix is expanded

Each expanded term expands to at most 50 indexed terms of its field that start with it (`max_expansions`, SPEC §7.1), the same terms in every segment and the memtable. The terms kept are the ones with the highest collection document frequency, ties broken by bytes. Every part of the index then scores those same terms. Their shared idf is the document frequency of the most frequent term kept. A prefix score is not multiplied by an edit distance. That weight belongs to L4 fuzzy, which builds its own cascade step (ROADMAP 8.2.4). [Fuzzy search](/guide/fuzzy-search/) is that step.

A word over several text fields matches when a field that stays exact holds the term, or an expanded field holds a term that starts with it. A prefix scores with that field's schema boost, the same field weight an exact term uses. When the request set a different `fields` weight (`"title^2"`), the step scales the prefix by the ratio of the two, clamped to the range a boost allows (SPEC §7.1).

Building the step reads no statistics. Choosing the 50 terms reads the document frequencies.

### What stays an exact term

Phrases stay exact, including a phrase of one token. `field:` clauses stay exact. Excluded clauses stay exact: a wider exclusion would exclude more. The request's filters stay on the step (ROADMAP 8.1.17).

A field whose analyzer stacks tokens stays exact. `edge_ngram` is that filter (SPEC §6.3.1). Its index already holds each word's prefixes, so a further prefix would match the same documents and add the grams' scores together. No built-in analyzer stacks tokens. When every field stacks, L3 builds no step.

### When the prefix step is cut or skipped

The step is held to the query limits the same way any later relaxation step is (SPEC §7.1.3). If the prefixes do not fit beside the steps they exclude, their expansions are lowered, largest first, and each prefix keeps its most frequent terms. The step is listed as `prefix:cut`. If one term each still does not fit, the step is listed as `prefix:skipped` and is not searched.

A query that expands nothing skips the level and is not listed in `applied`.

## Later cascade steps

| Piece | Status |
|---|---|
| L1 `synonyms` and L3 `prefix` | As this page describes. %%RELAXATION_CELL%% |
| L2 `drop_terms` | [How terms are dropped](/guide/typo-tolerant-search/#how-terms-are-dropped). |
| L4 `fuzzy` | One step from L0, after a `fuzzy:1` tier step when a word has 8 or more characters (ROADMAP 8.2.4, 8.2.7). `corrected` is stated when L4 found the hits (ROADMAP 8.2.4.3, 8.1.22). [Fuzzy search](/guide/fuzzy-search/). |
| L5 `fallback` | Static rank, then index-location recency (ROADMAP 8.2.5). The request's `sort` is ignored for that ranking. Hits still carry its values. Flagged with `Relaxation::fallback`. `none` allows an empty page. |
| Relaxing filters | Only with `last_resort`, as the final step after every enabled earlier level, and only the `soft_filter`. A query that searches no word does not build it (ROADMAP 8.2.6). |

## What is still not built

| Piece | Status |
|---|---|
| `suggest` completions | Not written (ROADMAP 18). L3 is not that API. |

A query can still come back empty. `fallback: "none"` leaves a miss that L1 through L4 did not match with no last resort. A hard filter no document passes stays empty; a `soft_filter` is dropped only with `relax.filters` set to `"last_resort"`, and a query that searches no word skips the filter step (ROADMAP 8.2.6). `RelaxOptions::disabled()`, and a collection whose `relaxation.enabled` is false, run L0 only. The zero-result regression gate is not written (ROADMAP 8.4).

[Getting started](/guide/getting-started/) opens a database and runs a first search. [What is an embedded search database?](/guide/embedded-search/) is the library shape.

## FAQ

### Does Persia DB do synonym search in Rust today?

Yes. When the exact query has too few hits, L1 of the cascade `Collection::search` runs builds one `synonyms` step from the collection's synonym groups. A matched run of words matches the run or one of its alternatives. Phrases, `field:` clauses, and excluded clauses stay exact. %%RELAXATION_CELL%%

### Does Persia DB do prefix search today?

As a relaxation step, yes. L3 builds one `prefix` step (and, unless L2 already took those documents, a `prefix:last` step first). The last analyzed term, and other analyzed terms of at least 3 characters, become prefixes of at most 50 indexed terms, kept by collection document frequency. Phrases, `field:` clauses, excluded clauses, filters, and `edge_ngram` fields stay exact. A `suggest` completion API is not written.

### Can I call Db::open and search with synonyms or a prefix?

Yes. Give the collection its `synonyms` when you create it, and `Collection::search` runs L1 and L3 whenever the exact query has fewer than `min_results` hits, unless the collection or the request turns relaxation off. A request can narrow the cascade with `relax`, for example `{"levels": ["prefix"]}`.

## Read next

- [Fuzzy search and did-you-mean](/guide/fuzzy-search/) — L4 edit distance, the correction, L5 fallback, and the filter step.
- [Typo-tolerant search](/guide/typo-tolerant-search/) — the whole cascade, `minimum_should_match`, and how L2 drops terms.
- [BM25F ranking](/guide/ranking/) — how a prefix and a synonym alternative are scored, and proximity off by default.
- [Faceted search](/guide/faceted-search/) — filters, facets, sort, cursor pages, and explain. Filters stay strict while L1 and L3 run.
- [What is an embedded search database?](/guide/embedded-search/) — in-process full-text search, one file, and no server.
- [FAQ](/guide/faq/) — production readiness, the file format, and the license.
- [Feature reference](/guide/features/#zero-results-relaxation) — the level table and the schema sketch that carries `synonyms`.
- [Getting started](/guide/getting-started/) — open a database, add documents, and search.
- [Comparison](/guide/comparison/) — next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
