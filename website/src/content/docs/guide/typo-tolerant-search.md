---
title: "Typo-tolerant search and zero results"
description: "How Persia DB handles typo-tolerant embedded search in Rust: query relaxation and zero results, minimum_should_match, drop_terms, and fuzzy search with did-you-mean corrections."
head:
  - tag: title
    content: "Typo-tolerant search in Rust: query relaxation and zero results | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Typo-tolerant search in Rust: query relaxation and zero results | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Typo-tolerant search in Rust: query relaxation and zero results | PersiaDB"
  - tag: script
    attrs:
      type: application/ld+json
    content: |
      {
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "name": "Typo-tolerant search and zero results",
        "url": "https://tomiok.github.io/persiaDB/guide/typo-tolerant-search/",
        "mainEntity": [
          {
            "@type": "Question",
            "name": "Does Persia DB do typo-tolerant search?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes, through query relaxation, which is on by default. Collection::search runs the exact query first and, when it has fewer than min_results hits, loosens it one level at a time: synonyms, dropping terms, prefixes, fuzzy edit distance, then a fallback. L4 fuzzy corrects words within edit distance 1 (4 to 7 characters) or 2 (8 or more) and reports each correction. The response's relaxation says which level found the hits."
            }
          },
          {
            "@type": "Question",
            "name": "What is query relaxation?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The cascade of SPEC section 7.6. A search walks the levels from the exact query and stops at the first level with enough hits (min_results, default 1) unless fill is set. Hits of a stricter step rank above hits of a looser one. The response's Relaxation states the level, the steps applied, the original and effective text, and the corrected words."
            }
          },
          {
            "@type": "Question",
            "name": "Are filters relaxed when a search has zero results?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The hard filter, never. The soft filter, only with relax.filters set to last_resort: then one last step drops it when every earlier level found fewer than min_results hits and the query searches a word. Every other level keeps both filters. Filters add nothing to the BM25F score."
            }
          },
          {
            "@type": "Question",
            "name": "Can a search still return zero results?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes: with relaxation off (relax false), with fallback set to none when no earlier level matches, and when no document passes the hard filter. Otherwise the L5 fallback returns the documents the filters keep, ranked by static rank, then by recency, and the response flags that order with fallback."
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

Typo-tolerant search, in this design, is not a separate fuzzy index. A query that would otherwise come back empty is loosened one step at a time until it has enough hits. That cascade is [query relaxation](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#76-zero-results-avoidance-default-on) (SPEC §7.6), and it is on by default: `Collection::search` and `Collection::search_json` run it unless the collection or the request turns it off.

The product goal is embedded full-text search in Rust that, by default, never returns zero results ([SPEC §1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#1-goals-and-non-goals)). This page is the search behavior: how a typo is handled, what the response says, and how to tune or turn off the cascade. The edit-distance step and the correction are [Fuzzy search and did-you-mean](/guide/fuzzy-search/). How a hit's score is computed is [BM25F ranking](/guide/ranking/). Filters are [Filtered search](/guide/filtered-search/).

## What typo-tolerant search means here

A user types `runing shoes` and expects shoes, not an empty page. The behavior is:

1. Run the query as written. Every analyzed term is required (default AND).
2. If the hit count is below `min_results` (default 1), try the next relaxation level.
3. Stop at the first level that has enough hits, unless `fill` is set, in which case looser levels may complete the page.
4. Rank hits from a stricter level above hits from a looser level.
5. Say what changed: the level, the steps applied, the original text, the effective text, and any corrected words.

Filters stay as the caller wrote them. The hard `filter` is never dropped; the `soft_filter` only as a last resort the search opts into ([Filtered search](/guide/filtered-search/#relaxation)). A request turns the cascade off with `relax: false`.

The sample opens a database in a temporary directory (the `tempfile` crate); any path works.

```rust
use persia::{Db, RelaxLevel, RelaxOptions, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("shop.persia"))?;
    let products = db.create_collection_json(r#"{"name": "products", "fields": {"title": {"type": "text"}}}"#)?;
    products.put_json(r#"{"id": "p1", "title": "red sandals"}"#)?;
    products.put_json(r#"{"id": "p2", "title": "blue rain boots"}"#)?;

    // "snadals" is in no document. With the fuzzy level alone, it is corrected: did you mean "sandals"?
    let page = products.search_json(r#"{"q": "red snadals", "relax": {"levels": ["fuzzy"]}}"#)?;
    assert_eq!(page.hits[0].document.id(), "p1");
    assert_eq!(page.relaxation.level, RelaxLevel::Fuzzy);
    let correction = &page.relaxation.corrected()[0];
    assert_eq!((correction.original.as_str(), correction.corrected.as_str()), ("snadals", "sandals"));

    // With every level (the default), L2 finds the hit first: one of the two words is enough.
    let page = products.search(&SearchRequest::new("red snadals"))?;
    assert_eq!(page.hits[0].document.id(), "p1");
    assert_eq!(page.relaxation.level, RelaxLevel::DropTerms);
    assert_eq!(page.relaxation.effective.as_deref(), Some("red"));
    assert!(page.hits[0].step().is_some_and(|step| step > 0));

    // Relaxation off: the exact query alone, and nothing is spelled "snadals".
    let exact = products.search(&SearchRequest::new("red snadals").relax(RelaxOptions::disabled()))?;
    assert!(exact.hits.is_empty());
    assert_eq!(exact.relaxation.level, RelaxLevel::Exact);

    db.close()?;
    Ok(())
}
```

## Query relaxation

Each level is specified in SPEC §7.6. The names are the ones the response, the collection settings, and `RelaxLevel::name` use.

| Level | Name | What it changes |
|---|---|---|
| L0 | `exact` | Every analyzed term is required. Always first. |
| L1 | `synonyms` | A term expands through the collection's synonym groups; within a group the terms are alternatives. Expansions per matched term are capped (default 50). [Synonym search](/guide/synonym-and-prefix-search/). |
| L2 | `drop_terms` | The share of terms that must match steps from 100% to 75%, to 50%, then to one term. The least important terms are the ones a step reports dropped first. [minimum_should_match](#minimum_should_match). |
| L3 | `prefix` | The last term, and other terms of length at least 3, expand through a prefix scan of the terms the collection holds. Expansions are capped (default 50). [Prefix search](/guide/synonym-and-prefix-search/#prefix-search). |
| L4 | `fuzzy` | Edit distance 1 for a term of length 4–7, distance 2 for length 8 or more. Shorter terms stay exact. Hits within one edit of every rewritten term rank first. The corrected words are reported when this level found the hits. [Fuzzy search](/guide/fuzzy-search/). |
| L5 | `fallback` | The documents the filters keep, by static rank, then by recency. The request's `sort` does not order these hits. `fallback: "none"` turns it off. The response flags the order with `fallback`. |
| L6 | `filters` | Only with `filters: "last_resort"`: L0's words without the `soft_filter`, when every earlier level found fewer than `min_results` hits. The hard `filter` stays. [Filtered search](/guide/filtered-search/#relaxation). |

A level runs only when the cascade reaches it, so a query that already has `min_results` hits at L0 builds nothing more. Every level builds its steps from L0's query, not from an earlier level's: L4 corrects every term of the query, not only those the last L2 step requires. Each later step excludes the documents of the earlier steps, so a document is a hit of the first step that matches it, once, at that step's score.

## The response

`SearchResponse::relaxation` says what the cascade did (SPEC §7.6). `SearchResponse::to_json` writes it as the response JSON's `relaxation` object.

| `Relaxation` | JSON | Meaning |
|---|---|---|
| `level` | `level` | The level of the last step built and not skipped. `RelaxLevel::Exact` when L0 was enough. |
| `applied` | `applied` | What each step after L0 applied, in order: `drop_terms:75%`, `fuzzy`. A step cut to fit the query limits ends with `:cut`; one that cannot fit ends with `:skipped` and is not searched. |
| `original` | `original` | The user text, as read. |
| `effective` | `effective` | The text of the query of the last step that found a hit: at L2, the words it keeps; at L4, the corrected words. |
| `corrected()` | `corrected` | The words L4 replaced, each with the term searched instead. Empty unless L4 found the hits. |
| `fallback()` | `fallback` | The order L5 ranked its hits in (`static_rank` or `recency`), when L5 found them. |
| `filters_dropped()` | `filters_dropped` | Whether L6 found the hits, so they do not pass the `soft_filter`. |
| `truncated` | `truncated` | Whether the time budget cut the cascade short. |

```json
{
  "level": "fuzzy",
  "applied": ["drop_terms:75%", "fuzzy"],
  "original": "zapatilas rojs",
  "effective": "zapatillas rojas",
  "corrected": {"zapatilas": "zapatillas", "rojs": "rojas"},
  "truncated": false
}
```

Each hit has its `level` (`Hit::level`), and for a relaxed search its step (`Hit::step`, 0 being L0) and rank score (`Hit::rank_score`). Ranking is step first, then the request's sort: every hit of a stricter step ranks above every hit of a looser one, at any score. A hit's cursor records its step and the relaxation settings, so the next page continues the same ranking, and a cursor given to a search with other settings is refused with `ErrorCode::InvalidArgument`. `SearchResponse::query_truncated` is another thing: whether part of `q` was left out to stay within the query limits (SPEC §7.1.1).

## Settings

A collection's `relaxation` (SPEC §6.2.3) sets the defaults: `enabled`, `min_results` (1), `levels` (`"default"`, or a list of `synonyms`, `drop_terms`, `prefix`, `fuzzy` in cascade order), `fallback` (`"static_rank"` or `"none"`), `fill` (`false`), `filters` (`"never"` or `"last_resort"`), and `budget_ms` (50). A request's `relax` overrides them one key at a time: `false`, `true`, or an object of the same keys, in the request JSON or as a `RelaxOptions` (`SearchRequest::relax`). The values are checked strictly: `min_results` in 1–10,000, `budget_ms` in 1–60,000, `levels` distinct and in cascade order; a bad one is `ErrorCode::InvalidArgument` (ROADMAP 8.3.1).

With `fill`, the cascade keeps going until the page is full, and pages stay slices of one ranking (ROADMAP 8.3.2). The time budget, `budget_ms`, starts after the exact level, so that level is never cut by it. Past it, the completed steps come back with `truncated: true`, and a step in progress is dropped whole (ROADMAP 8.1.4).

```rust
use persia::{Db, FallbackOrder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("notes.persia"))?;
    let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    notes.put_json(r#"{"id": "n1", "body": "hello world"}"#)?;

    // No document holds "purple": the L5 fallback still returns one, and says in which order.
    let page = notes.search_json(r#"{"q": "purple"}"#)?;
    assert_eq!(page.hits.len(), 1);
    assert_eq!(page.relaxation.fallback(), Some(FallbackOrder::Recency)); // the schema has no static rank

    // A collection whose fallback is "none" allows an empty page.
    let strict = db.create_collection_json(
        r#"{"name": "strict", "fields": {"body": {"type": "text"}}, "relaxation": {"fallback": "none"}}"#,
    )?;
    strict.put_json(r#"{"id": "s1", "body": "hello world"}"#)?;
    assert!(strict.search_json(r#"{"q": "purple"}"#)?.hits.is_empty());

    db.close()?;
    Ok(())
}
```

## Never return an empty page

Never returning an empty page is a product rule (SPEC §1, §7.6). With the default settings, a search returns hits from:

- The exact query, when enough documents hold every required term.
- An L1 step, when the query holds a word of a synonym group and a document holds an alternative.
- An L2 step, when the query has at least two required terms and some documents hold fewer of them.
- An L3 step, when an expanded term is a prefix of a term the collection holds.
- An L4 step, when a term of length 4 or more is within edit distance of a term the collection holds.
- The L5 fallback, which returns the documents the filters keep.

What still comes back empty:

- A search with relaxation off: `relax: false` (`RelaxOptions::disabled()`), or a collection whose `relaxation.enabled` is false. An exact miss stays empty.
- A collection whose `fallback` is `none`, for a query no earlier level matched. L2 skips a one-term query, and a term shorter than 4 characters is not a fuzzy match.
- A hard filter no document passes. Every level keeps it, the fallback too. A `soft_filter` no document passes is dropped by L6 only with `filters: "last_resort"`, and only for a query that searches a word.

The quality gate is not written yet: a synthetic typo suite, a zero-result rate before and after relaxation, and a regression on that rate (ROADMAP 8.4). This project does not publish a zero-result rate.

## minimum_should_match

L2 is a count of required terms a document must match, as a ladder (SPEC §7.1, ROADMAP 8.2.2.2). A term is one required clause of the compiled query: a word, a phrase, a word split in two, or one word over several fields. An excluded clause (`-word`) is not a term and stays excluded.

The rungs after L0, for a query of `n` terms, are 75%, then 50%, then one term. A percentage `p` is the count `max(1, floor(p · n / 100))`. A count that repeats the previous rung is not built again, and 100% is L0, so it is never a `drop_terms` step. Two terms therefore produce one step, `drop_terms:75%`, which requires one of them. `applied` names the rung that first produced that count: `drop_terms:75%`, `drop_terms:50%`, or `drop_terms:1`.

The count changes what matches, not the score: a document scores the terms it actually matched. A query of fewer than two terms skips the level, since dropping its only term would match every document. Each rung keeps L0's excluded clauses and filters.

## How terms are dropped

Importance decides which terms a step reports as kept, not which documents match (ROADMAP 8.2.2.1). It compares, with nothing to tune: a term no document holds (often a typo) is less important than any other, then a stopword, then a lower IDF, then a later position in the query (ROADMAP 8.2.2.5). The last word tends to qualify the earlier ones, so `red running shoes` keeps `shoes` over `red` when the two are equally rare.

The step's `effective` text is its most important terms and every excluded clause, in text order. That string names what the step keeps; the step matches any of its count of the terms. For `red snadals`, L0 requires both words and matches nothing, since no document holds `snadals`. The one L2 step requires one of the two, so the documents that hold `red` come back, and `effective` is `red`. That step found the hits, so the cascade stops before L4, and `corrected` stays empty. Restricting `levels` to `["fuzzy"]`, as the sample above does, goes to L4 instead.

## Compiling a query

Before any relaxation, the user text `q` becomes a query (SPEC §7.1.1), and that never fails: syntax that does not parse is read as plain text. Clauses are split on whitespace and all required. A clause is an optional `-`, an optional `field:`, and a word or a `"phrase"`. `-` excludes the documents the clause would match; `field:` limits it to that `text` or `keyword` field. Text past 8 KiB is left out on a clause boundary, and the response sets `query_truncated`. With no clause left, the query matches every document.

Words are analyzed with each field's query-time analyzer ([Text analysis](/guide/text-analysis/)). A typo stays the term the analyzer produced, required at L0; the cascade's L3 and L4 are what expand it. Filters are a separate JSON object, written by programs, so a bad filter is an error and user text is not ([Filtered search](/guide/filtered-search/)).

## BM25F scoring

Ranking is BM25F (SPEC §7.2). For a term `t` and a document `d`, over fields `f` with weights `w_f`:

```text
tf~(t,d) = Σ_f  w_f · tf(t,d,f) / (1 − b_f + b_f · len_f(d) / avglen_f)
score(t,d) = idf(t) · tf~ · (k1 + 1) / (k1 + tf~)
idf(t) = ln(1 + (N − df + 0.5) / (df + 0.5))
```

`N`, `df`, and `avglen` are collection-level. Text clauses score; filters and excluded clauses add 0, so a filter never inflates BM25F. An L2 step scores the terms a document actually matched. [BM25F ranking](/guide/ranking/) is the formula, the static rank, and the proximity bonus, which a relaxed search does not apply (a request that sets `proximity` must also set `relax: false`).

## FAQ

### Does Persia DB do typo-tolerant search?

Yes, through query relaxation, which is on by default. `Collection::search` runs the exact query first and, when it has fewer than `min_results` hits, loosens it one level at a time: synonyms, dropping terms, prefixes, fuzzy edit distance, then a fallback. L4 fuzzy corrects words within edit distance 1 (4 to 7 characters) or 2 (8 or more) and reports each correction. The response's `relaxation` says which level found the hits.

### What is query relaxation?

The cascade of SPEC §7.6. A search walks the levels from the exact query and stops at the first level with enough hits (`min_results`, default 1) unless `fill` is set. Hits of a stricter step rank above hits of a looser one. The response's `Relaxation` states the level, the steps applied, the original and effective text, and the corrected words.

### Are filters relaxed when a search has zero results?

The hard `filter`, never. The `soft_filter`, only with `relax.filters` set to `"last_resort"`: then one last step drops it when every earlier level found fewer than `min_results` hits and the query searches a word. Every other level keeps both filters. Filters add nothing to the BM25F score.

### Can a search still return zero results?

Yes: with relaxation off (`relax: false`), with `fallback` set to `none` when no earlier level matches, and when no document passes the hard filter. Otherwise the L5 fallback returns the documents the filters keep, ranked by static rank, then by recency, and the response flags that order with `fallback`.

## Read next

- [Fuzzy search and did-you-mean](/guide/fuzzy-search/) — L4 edit distance, the correction returned to the caller, L5 fallback, and the filter step.
- [Synonym and prefix search](/guide/synonym-and-prefix-search/) — how L1 synonym groups and L3 prefix expansion work, and what they leave exact.
- [Filtered full-text search](/guide/filtered-search/) — operators, hard and soft filters, and `last_resort`.
- [BM25F ranking](/guide/ranking/) — field weights, collection statistics, static rank, and proximity.
- [Faceted search](/guide/faceted-search/) — facets, sort, cursor pages, and explain.
- [Getting started](/guide/getting-started/) — open a database, add documents, and search.
- [Comparison](/guide/comparison/) — next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
- [FAQ](/guide/faq/) — production readiness, the file format, crash safety, and the license.
