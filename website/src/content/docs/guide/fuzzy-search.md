---
title: "Fuzzy search and did-you-mean"
description: "Fuzzy search in Rust with Persia DB: a banded edit-distance automaton, length policy and typo tiers, did-you-mean corrections, the flagged fallback, and last-resort filter relaxation. Through Collection::search and the request JSON."
head:
  - tag: title
    content: "Fuzzy search and did-you-mean in Rust | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Fuzzy search and did-you-mean in Rust | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Fuzzy search and did-you-mean in Rust | PersiaDB"
  - tag: script
    attrs:
      type: application/ld+json
    content: |
      {
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "name": "Fuzzy search and did-you-mean",
        "url": "https://tomiok.github.io/persiaDB/guide/fuzzy-search/",
        "mainEntity": [
          {
            "@type": "Question",
            "name": "Does Persia DB do fuzzy search in Rust today?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes. Collection::search and Collection::search_json run the relaxation cascade by default, and its level L4 builds a fuzzy step from the exact query. A term of 4 to 7 characters matches within one edit, and a term of 8 or more within two. The distance is optimal string alignment, including a swap of two adjacent characters. User text never compiles to a fuzzy clause of its own: L4 is what widens a typo."
            }
          },
          {
            "@type": "Question",
            "name": "How does did-you-mean spelling correction work?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "When L4 is the step that found the hits, Relaxation::corrected lists each required one-token word that no document holds, and the indexed term the step searched instead. effective is the original text with those words replaced. A word some document already holds is widened, and is not reported as a correction. If an earlier step found the hits, corrected stays empty even when L4 was built. In the response JSON, corrected is an object from each original word to its correction."
            }
          },
          {
            "@type": "Question",
            "name": "What happens when fuzzy search still finds nothing?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "L5 fallback, unless fallback is none, returns documents the filters keep and earlier steps did not match. It ranks them by static rank, then by where the index holds them, and the response sets fallback to static_rank or recency. That order ignores the request sort. An explained hit has a root Explanation::Relaxed that names the level, the step, and what that step applied."
            }
          },
          {
            "@type": "Question",
            "name": "Does fuzzy search drop filters?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Not by default. Every level keeps the request's filters, and the hard filter is never dropped. With relax.filters set to last_resort, one last step drops the soft_filter when every earlier level found fewer than min_results hits and the query searches a word. A query that searches no word does not build the step. filters_dropped is true only when that step found the hits."
            }
          },
          {
            "@type": "Question",
            "name": "Can I call Db::open and run a fuzzy query?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes. Db::open opens or creates the file, Db::create_collection_json adds a collection, and Collection::search runs the cascade, fuzzy step included, unless the collection or the request turns relaxation off. A request narrows the levels with relax, for example relax levels fuzzy only."
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

Fuzzy search, in this engine, is one step of [query relaxation](/guide/typo-tolerant-search/#query-relaxation) (SPEC §7.6). It is not a separate fuzzy index, and the user text `q` never compiles to a `fuzzy` clause of its own. L4 builds that clause from the exact query when earlier levels found fewer than `min_results` hits (default 1). `Collection::search` and `Collection::search_json` run the cascade by default. The caller gets the corrected words only when that step is the one that found the hits.

The same cascade can then rank a [fallback](#fallback-when-nothing-matches) and, if the caller opts in, drop the soft filter [once](#filter-relaxation). Synonym groups and prefix expansion are the earlier steps, on [Synonym and prefix search](/guide/synonym-and-prefix-search/). The whole ladder, including how L2 drops a term, is [Typo-tolerant search](/guide/typo-tolerant-search/).

%%RELAXATION_NOW%%

The samples open a database in a temporary directory (the `tempfile` crate); any path works.

```rust
use persia::{Db, Explanation, RelaxLevel};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("shop.persia"))?;
    let products = db.create_collection_json(r#"{"name": "products", "fields": {"title": {"type": "text"}}}"#)?;
    products.put_json(r#"{"id": "p1", "title": "zapatillas rojas"}"#)?;
    products.put_json(r#"{"id": "p2", "title": "camisa azul"}"#)?;

    // Two typos. The cascade is narrowed to L4, so L2 does not take the hits first.
    let page = products.search_json(
        r#"{"q": "Zapatilas rojs", "relax": {"levels": ["fuzzy"]}, "explain": true}"#,
    )?;
    assert_eq!(page.hits[0].document.id(), "p1");
    assert_eq!(page.relaxation.level, RelaxLevel::Fuzzy);
    // "zapatilas" has 9 characters (distance 2), so L4 tries every word within one edit first: `fuzzy:1`.
    assert_eq!(page.relaxation.applied, ["fuzzy:1"]);

    // Did you mean: each word as written, with the term the index holds.
    let corrected: Vec<(&str, &str)> = page
        .relaxation
        .corrected()
        .iter()
        .map(|c| (c.original.as_str(), c.corrected.as_str()))
        .collect();
    assert_eq!(corrected, [("Zapatilas", "zapatillas"), ("rojs", "rojas")]);
    assert_eq!(page.relaxation.effective.as_deref(), Some("zapatillas rojas"));

    // An explained hit names its level, its step, and what that step applied.
    let Some(Explanation::Relaxed(step)) = &page.hits[0].explanation else {
        return Err("no relaxed explanation".into());
    };
    assert_eq!((step.level, step.applied.as_deref()), (RelaxLevel::Fuzzy, Some("fuzzy:1")));

    // A word some document holds is widened, not corrected: only "rojs" is reported.
    let page = products.search_json(r#"{"q": "zapatillas rojs", "relax": {"levels": ["fuzzy"]}}"#)?;
    assert_eq!(page.relaxation.corrected().len(), 1);
    assert_eq!(page.relaxation.corrected()[0].original, "rojs");

    db.close()?;
    Ok(())
}
```

## How a fuzzy step is built

L4 builds its step, listed as `fuzzy`, from L0's query (ROADMAP 8.2.4, [ADR-0042](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0042-l4-fuzzy-one-step-length-policy-and-corrections.md)). It does not continue from the terms L2 dropped. Every required word stays required, widened to the terms within its distance. Excluded words and the request's filters stay on the step.

A term here is one field's analyzed form of a word, the same terms L3 uses. Length is that analyzed term, in Unicode scalar values: `café` is 4. Under the `en` analyzer, `running` is `run`, which is 3, so L4 leaves it exact. How that stem is produced, and what the other built-in analyzers emit, is [Text analysis](/guide/text-analysis/). The last word has no special case. A short word stays exact wherever it sits. That differs from [prefix search](/guide/synonym-and-prefix-search/#which-terms-become-prefixes), which expands the last term at any length.

| Analyzed length | Distance |
|---|---|
| 1–3 | None. The term stays exact. |
| 4–7 | 1 |
| 8 or more | 2 |

Phrases stay exact, including a phrase of one token. `field:` clauses stay exact. Excluded clauses stay exact. A field whose analyzer stacks tokens stays exact. `edge_ngram` is that filter (SPEC §6.3.1): its index already holds the grams, and a further fuzzy match would add those scores together.

Each rewritten term expands to at most 50 indexed terms of its field (`max_expansions`, SPEC §7.1), the same terms in every segment and the memtable, so every part of the index scores the same terms. When more than 50 qualify, the ones kept are those with the fewest edits, then the highest collection document frequency, ties broken by bytes. Their shared idf is the document frequency of the most frequent term kept. A fuzzy match's score is then multiplied by `(distance + 1 − d) / (distance + 1)`, where `d` is that term's distance. An exact term weighs 1. Each further edit weighs strictly less. A prefix score is not multiplied that way.

The distance is optimal string alignment over Unicode scalar values (SPEC §7.1, ROADMAP 8.2.4.1). An insertion, a deletion, a substitution, or a swap of two adjacent characters counts as one edit, and no character is edited twice. A byte that is not valid UTF-8 counts as a character no query character equals. The field's terms are walked with an edit-distance automaton that keeps only the band of `2 · distance + 1` columns around the diagonal of each row. A query term may be 32 KiB (SPEC §7.1).

The step is held to the query limits beside the earlier steps it excludes (SPEC §7.1.3). If its fuzzy expansions do not fit, they are lowered, largest first, and the step is listed as `fuzzy:cut`. If one term each still does not fit, the step is listed as `fuzzy:skipped` and is not searched. A query that rewrites nothing skips the level and is not listed in `applied`.

## Typo tiers

Within a level, a hit that needs fewer edits ranks above one that needs more, before BM25F (ROADMAP 8.2.7, [ADR-0046](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0046-typo-and-exactness-tiers-as-cascade-steps.md)). The tiers are extra steps, strictest first. They do not change a score. The edit-distance weight above is still applied inside a step. The tier only decides which step a document belongs to, and a stricter step ranks above a looser one at any score.

When a term L4 rewrites has 8 or more characters, L4 builds two steps. The first is `fuzzy:1`: the same query with every rewritten term capped at distance 1. The second is `fuzzy`, at each term's own distance. The tier is the most edits any one rewritten word needs, not the sum across words. A query whose terms are all shorter has the one step. When `fuzzy:1` is the step that found the hits, `corrected` and `effective` name the terms within one edit, as in the sample above.

The two steps are nested (ROADMAP 8.2.12): `fuzzy` keeps every term `fuzzy:1` keeps, and matches every document it matches. Once `fuzzy:1` is searched, `fuzzy` excludes it and counts its expansions once, so the tier makes a query reach `fuzzy:cut` or `fuzzy:skipped` no sooner.

L3 has the same idea. When it expands a term besides the last one, and L2's one-term rung was not searched first, it builds `prefix:last` before `prefix`. That step is [prefix search](/guide/synonym-and-prefix-search/#prefix-search). L0 does not rank a word typed exactly above its stem: the index holds the stems alone (ROADMAP 8.2.10, still open). L1 is not tiered either.

## Did you mean

`Relaxation::corrected` is the did-you-mean list (ROADMAP 8.2.4.3). It is filled only when L4 is the step that found the hits (ROADMAP 8.1.22, [ADR-0043](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0043-relaxed-response-states-the-hit-step.md)). A later step that adds no hit does not replace `effective` or `corrected`.

A word is corrected when all of these hold:

- It is a required word of one token, over the default fields.
- No field of it holds that term. Collection document frequency is 0, so no document does.
- A field the step rewrites has a term within the word's distance that some document holds.

The correction is the term that field's fuzzy expansion keeps first: the fewest edits, then the highest collection document frequency, ties broken by bytes, and across fields the field searched first. The step searches that term under any cut of the expansions. `corrected` lists each such word in text order, as the user wrote it (`Zapatilas`), with the correction as the index holds it (`zapatillas`: analyzed, so lowercased, and stemmed under a stemming analyzer). Each is a `Correction`, with `original` and `corrected`.

`Relaxation::effective` is L0's text with each corrected word replaced by that index form. Other clauses, including excluded ones, stay as written. The string names the terms searched. It does not have to compile back to the step.

A word some document already holds is widened by the step and is not reported. It matched as typed. A word of several tokens (`e-mial`) is matched per token and is neither corrected nor rewritten in `effective`, which has no text per token.

L2 can take the hits before L4 runs. A term no document holds is the least important term, below a stopword (ROADMAP 8.2.2.5), so `effective` on an L2 step does not name it when another term can be kept. For `red shoez` over documents that hold `red shoes`, L2's one-term rung already matches every document L4 could add. The response then states L2's `effective` (`red`) and an empty `corrected`, even if L4 was built and `level` is `fuzzy` (SPEC §7.6). A one-term query skips L2 (ROADMAP 8.2.2.3), so `runing` can reach L4. [How terms are dropped](/guide/typo-tolerant-search/#how-terms-are-dropped) is that order.

`SearchResponse::to_json` writes the corrections as an object from each original word to its correction (SPEC §7.6):

```json
{
  "level": "fuzzy",
  "applied": ["drop_terms:75%", "fuzzy"],
  "original": "zapatilas rojs",
  "effective": "zapatillas rojas",
  "corrected": { "zapatilas": "zapatillas", "rojs": "rojas" }
}
```

## What the response reports

`SearchResponse::relaxation` is the cascade summary (ROADMAP 8.1.5). `level` is the last step built and not skipped. `applied` lists what each step after L0 did, in build order. A step cut to fit the limits ends with `:cut`. One that cannot fit ends with `:skipped` and is not searched. `original` is the user text. `effective` and `corrected()` describe the hit step, the last step that found a hit. `truncated` means the time budget passed. `fallback()` is set when L5 is that hit step. `filters_dropped()` is true when the filter step is.

Each hit carries its `level` (`Hit::level`), and for a relaxed search its step (`Hit::step`, 0 is L0) and rank score (`Hit::rank_score`). Hits from a stricter step rank above hits from a looser one, before the request's sort (ROADMAP 8.1.3).

With `explain: true`, an explained hit of a relaxed search has a root `Explanation::Relaxed` (ROADMAP 8.3.4, SPEC §7.7). Its `RelaxedExplanation` names the same `level`, `step`, and `applied`, and wraps the step's own score tree (`Explanation::scoring`). The score is the hit's. L5's inner node is `Explanation::StaticRank`. Highlighted snippets are not this tree. Highlights are post-v1. [Faceted search](/guide/faceted-search/) is the rest of the request: facets, sort, cursors, and explain.

## Fallback when nothing matches

L5 runs when `fallback` is `static_rank` (the default) and the earlier levels found fewer than `min_results` hits (ROADMAP 8.2.5, [ADR-0044](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0044-l5-fallback-static-rank-then-index-recency.md)). `fallback: "none"` builds no L5 step, and a miss can stay empty.

The step keeps L0's excluded words and filters and matches every other document, with the earlier steps excluded. Its hits are the live documents those filters keep, that the excluded words do not match, and that no earlier step matched. It has no query words to score. It ranks by the collection's static rank, then by index location: the memtable first, then segments from the last the manifest lists to the first, each by docid descending. That recency is where the index holds the document, not when it was written. A merge reorders it (ROADMAP 8.2.8, still open). Without a static rank, or with its weight 0, every score is 0 and recency alone orders the hits. `Relaxation::fallback` is then `FallbackOrder::Recency`, otherwise `FallbackOrder::StaticRank`.

The request's `sort` does not order these hits. Each hit still carries the request's sort values. `effective` is the excluded clauses, empty when there are none. There is no `corrected`.

## Filter relaxation

Filters stay on L0 through L5 unless the request or the collection sets `relax.filters` to `"last_resort"` (`FilterRelaxation::LastResort`; ROADMAP 8.2.6, [ADR-0047](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0047-filter-relaxation-as-a-last-step.md)). That plans one last step, L6 `filters`, after every enabled earlier level. `fallback: "none"` omits L5 and L6 still comes last.

The step is built only when every earlier level found fewer than `min_results` hits. It is not built to fill a page, even with `fill`. It is L0's words without the request's `soft_filter`; the hard `filter` stays (ROADMAP 8.2.13). Excluded words stay excluded. A query that searches no word does not build it: empty text, stopwords only, or exclusions alone. A request with no `soft_filter` does not build it either.

When this step supplies the hits, the response's `level` is `filters` and `filters_dropped()` is true. Those hits do not pass the `soft_filter`; they pass the hard `filter`. Facets count them, so a facet can show a value the soft filter rejects (SPEC §7.5). Put a filter that enforces tenancy or permissions in the hard `filter`, which no level drops (ROADMAP 8.2.13). The operators are [Filtered full-text search](/guide/filtered-search/).

```rust
use persia::{Db, Fallback, FallbackOrder, FilterRelaxation, RelaxLevel, RelaxOptions, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("shop.persia"))?;
    let products = db.create_collection_json(
        r#"{"name": "products", "fields": {"title": {"type": "text"}, "brand": {"type": "keyword"}}}"#,
    )?;
    products.put_json(r#"{"id": "p1", "title": "red sandals", "brand": "acme"}"#)?;

    // No document holds "purple": L5 returns the documents the filters keep, by recency (no static rank).
    let page = products.search(&SearchRequest::new("purple"))?;
    assert_eq!(page.hits[0].document.id(), "p1");
    assert_eq!(page.relaxation.level, RelaxLevel::Fallback);
    assert_eq!(page.relaxation.fallback(), Some(FallbackOrder::Recency));

    // No fallback, and the soft filter as a last resort: L6 drops the soft filter no document passes.
    let last_resort = RelaxOptions {
        fallback: Some(Fallback::None),
        filters: Some(FilterRelaxation::LastResort),
        ..RelaxOptions::default()
    };
    let nike = r#"{"term": ["brand", "nike"]}"#;
    let page = products.search(
        &SearchRequest::new("sandals").soft_filter_json(nike).relax(last_resort.clone()),
    )?;
    assert_eq!(page.hits[0].document.id(), "p1");
    assert_eq!(page.relaxation.level, RelaxLevel::Filters);
    assert!(page.relaxation.filters_dropped());

    // The hard filter is never dropped: the page stays empty.
    let page = products.search(&SearchRequest::new("sandals").filter_json(nike).relax(last_resort))?;
    assert!(page.hits.is_empty());

    db.close()?;
    Ok(())
}
```

## Relax options

A request's `relax` is `false`, `true`, or an object of the collection's relaxation keys: `enabled`, `min_results`, `levels`, `fallback`, `fill`, `filters`, and `budget_ms` (ROADMAP 8.3.1, [ADR-0045](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0045-request-relax-options-and-cursor-settings.md)). In code it is a `RelaxOptions` (`SearchRequest::relax`), whose fields are those keys, each `None` to keep the collection's setting; `RelaxOptions::from_json` reads the JSON object strictly. The search merges it over the collection's `relaxation` (`CollectionSchema::relaxation`, a `RelaxationConfig`), one key at a time. `relax: false` (`RelaxOptions::disabled()`) runs L0 only. `fill` keeps walking until the page window is full (ROADMAP 8.3.2). L6 still does not fill. A cursor records the settings, and a page under other settings is refused with `ErrorCode::InvalidArgument`.

`levels` lists levels after L0 (`Levels::Custom` of `Level::Synonyms`, `DropTerms`, `Prefix`, `Fuzzy`, in cascade order, or `Levels::Default`). L5 is `fallback`, not an entry of that list. `[]` with the default fallback runs L0 and then L5. The limits a step is held to are the database's query limits (SPEC §7.1.3), not a key of `relax`.

```rust
use persia::{Db, Fallback, Level, Levels, RelaxLevel, RelaxOptions, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("notes.persia"))?;
    let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    notes.put_json(r#"{"id": "n1", "body": "hello world"}"#)?;

    // L0, then L4 alone, no fallback: the typo is corrected.
    let fuzzy_only = RelaxOptions {
        levels: Some(Levels::Custom(vec![Level::Fuzzy])),
        fallback: Some(Fallback::None),
        ..RelaxOptions::default()
    };
    let page = notes.search(&SearchRequest::new("helo").relax(fuzzy_only))?;
    assert_eq!(page.relaxation.level, RelaxLevel::Fuzzy);
    assert_eq!(page.hits[0].document.id(), "n1");

    // The same settings as JSON.
    let json = notes.search_json(r#"{"q": "helo", "relax": {"levels": ["fuzzy"], "fallback": "none"}}"#)?;
    assert_eq!(json.to_json()?, page.to_json()?);

    // Relaxation off: L0 only, and nothing is spelled "helo".
    let exact = notes.search(&SearchRequest::new("helo").relax(RelaxOptions::disabled()))?;
    assert!(exact.hits.is_empty());

    db.close()?;
    Ok(())
}
```

## What can still come back empty

- `fallback: "none"`, when L0 through L4 did not find `min_results` hits. A term shorter than 4 characters is not a fuzzy match.
- A filter no document passes. The hard `filter` is never dropped; a `soft_filter` only by L6 with `filters: "last_resort"`, and only for a query that searches a word.
- `RelaxOptions::disabled()`, or a collection whose `relaxation.enabled` is false. L0 only.
- A fuzzy step skipped because it did not fit the query limits.

The zero-result regression gate is not written (ROADMAP 8.4). This project does not publish a zero-result rate.

[Getting started](/guide/getting-started/) opens a database and runs a first search. [What is an embedded search database?](/guide/embedded-search/) is the library shape.

## FAQ

### Does Persia DB do fuzzy search in Rust today?

Yes. `Collection::search` and `Collection::search_json` run the relaxation cascade by default, and its level L4 builds a `fuzzy` step from the exact query. A term of 4–7 characters matches within one edit, and a term of 8 or more within two. The distance is optimal string alignment, including a swap of two adjacent characters. User text never compiles to a `fuzzy` clause of its own: L4 is what widens a typo. %%RELAXATION_CELL%%

### How does did-you-mean spelling correction work?

When L4 is the step that found the hits, `Relaxation::corrected` lists each required one-token word that no document holds, and the indexed term the step searched instead. `effective` is the original text with those words replaced. A word some document already holds is widened, and is not reported as a correction. If an earlier step found the hits, `corrected` stays empty even when L4 was built. In the response JSON, `corrected` is an object from each original word to its correction.

### What happens when fuzzy search still finds nothing?

L5 fallback, unless `fallback` is `none`, returns documents the filters keep and earlier steps did not match. It ranks them by static rank, then by where the index holds them, and `Relaxation::fallback` says `static_rank` or `recency`. That order ignores the request's `sort`. An explained hit has a root `Explanation::Relaxed` that names the level, the step, and what that step applied.

### Does fuzzy search drop filters?

Not by default. Every level keeps the request's filters, and the hard `filter` is never dropped. With `relax.filters` set to `"last_resort"`, one last step drops the `soft_filter` when every earlier level found fewer than `min_results` hits and the query searches a word. A query that searches no word does not build the step. `filters_dropped` is true only when that step found the hits.

### Can I call Db::open and run a fuzzy query?

Yes. `Db::open` opens or creates the file, `Db::create_collection_json` adds a collection, and `Collection::search` runs the cascade, fuzzy step included, unless the collection or the request turns relaxation off. A request narrows the levels with `relax`, for example `{"levels": ["fuzzy"]}`.

## Read next

- [Typo-tolerant search](/guide/typo-tolerant-search/) — the whole cascade, `minimum_should_match`, and how L2 drops a term before L4 runs.
- [Synonym and prefix search](/guide/synonym-and-prefix-search/) — L1 synonym groups and L3 prefix expansion, the steps before fuzzy.
- [BM25F ranking](/guide/ranking/) — the score inside a step, the edit-distance weight, static rank on L5, and proximity off by default.
- [Filtered full-text search](/guide/filtered-search/) — the filter DSL, how a request filter joins the query, and when L6 drops it.
- [Faceted search](/guide/faceted-search/) — filters, facets, sort, cursor pages, and explain.
- [Comparison](/guide/comparison/) — next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
- [What is an embedded search database?](/guide/embedded-search/) — in-process full-text search, one file, and no server.
- [FAQ](/guide/faq/) — production readiness, the file format, and the license.
- [Getting started](/guide/getting-started/) — open a database, add documents, and search.
- [Feature reference](/guide/features/#zero-results-relaxation) — the level table and the schema sketch that carries `relaxation`.
