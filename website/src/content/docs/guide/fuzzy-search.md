---
title: "Fuzzy search and did-you-mean"
description: "Fuzzy search in Rust with Persia DB: a banded edit-distance automaton, length policy and typo tiers, did-you-mean corrections, the flagged fallback, and last-resort filter relaxation. Library code in persia-engine."
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
              "text": "As library code, yes. L4 of CollectionViews::search_relaxed builds a fuzzy step from the exact query. A term of 4 to 7 characters matches within one edit, and a term of 8 or more within two. The distance is optimal string alignment, including a swap of two adjacent characters, walked as a banded automaton over each field's term dictionary. The compiler does not turn a typo into a fuzzy clause. There is no search method on persia::Db. %%RELAXATION_CELL%%"
            }
          },
          {
            "@type": "Question",
            "name": "How does did-you-mean spelling correction work?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "When L4 is the step that found the hits, Relaxation.corrected lists each required one-token word that no document holds, and the indexed term the step searched instead. effective is the original text with those words replaced. A word some document already holds is widened, and is not reported as a correction. If an earlier step found the hits, corrected stays empty even when L4 was built. The value is a list of original and corrected strings, not a public JSON API."
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
              "text": "Not by default. L1 through L5 keep the request filter. With relax.filters set to last_resort, one last step drops that filter when every earlier level found fewer than min_results hits and the query searches a word. A query that searches no word does not build the step. filters_dropped is true only when that step found the hits."
            }
          },
          {
            "@type": "Question",
            "name": "Can I call Db::open and run a fuzzy query?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. persia::Db is not in the crate, and Database has no search method. The engine call is CollectionViews::search_relaxed on a collection snapshot. RelaxPlan and RelaxOptions are library code. Code samples that call Db::search on this site are illustrative."
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

Fuzzy search, in this engine, is one step of [query relaxation](/guide/typo-tolerant-search/#query-relaxation) (SPEC §7.6). It is not a separate fuzzy index, and the user-text compiler does not rewrite a typo into a `fuzzy` clause. L4 of `CollectionViews::search_relaxed` builds that clause from the exact query when earlier levels found fewer than `min_results` hits (default 1). The caller gets the corrected words only when that step is the one that found the hits.

The same cascade can then rank a [fallback](#fallback-when-nothing-matches) and, if the caller opts in, drop filters [once](#filter-relaxation). Synonym groups and prefix expansion are the earlier steps, on [Synonym and prefix search](/guide/synonym-and-prefix-search/). The whole ladder, including how L2 drops a term, is [Typo-tolerant search](/guide/typo-tolerant-search/).

%%RELAXATION_NOW%% There is still no search on the `persia` database handle (M10). `persia::Db` is not in the crate. `persia_engine::db::Database` has `open`, `put`, and `commit`, and no `search`.

## How a fuzzy step is built

L4 builds one step, listed as `fuzzy`, from L0's query (ROADMAP 8.2.4, [ADR-0042](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0042-l4-fuzzy-one-step-length-policy-and-corrections.md)). It does not continue from the terms L2 dropped. Every required word stays required, widened to the terms within its distance. `must_not` and the request's filters stay on the step.

A term here is one field's analyzed form of a word, the same terms L3 uses. Length is that analyzed term, in Unicode scalar values: `café` is 4. Under the `en` analyzer, `running` is `run`, which is 3, so L4 leaves it exact. How that stem is produced, and what the other built-in analyzers emit, is [Text analysis](/guide/text-analysis/). The last word has no special case. A short word stays exact wherever it sits. That differs from [prefix search](/guide/synonym-and-prefix-search/#which-terms-become-prefixes), which expands the last term at any length.

| Analyzed length | Distance |
|---|---|
| 1–3 | None. The term stays exact. |
| 4–7 | 1 |
| 8 or more | 2 |

Phrases stay exact, including a phrase of one token. `field:` clauses stay exact. Excluded clauses stay exact. A field whose analyzer stacks tokens stays exact. `edge_ngram` is that filter (SPEC §6.3.1): its index already holds the grams, and a further fuzzy match would add those scores together. A programmatic root `bool` that has `should` clauses, or a `minimum_should_match` other than 0, is left alone, as L2 and L3 leave it.

Each rewritten term becomes a `fuzzy` query with `max_expansions` 50 (`DEFAULT_MAX_EXPANSIONS`). Planning expands it once, over the terms of that field in every view (each segment and the memtable), so every view scores the same terms (SPEC §7.1, ROADMAP 7.4.12). The walk intersects the field's term dictionary (an FST) with an edit-distance automaton. The terms kept are the ones with the highest collection document frequency, ties broken by bytes, at most 50. Their shared idf is the document frequency of the most frequent term kept. A `fuzzy` score is then multiplied by `(distance + 1 − d) / (distance + 1)`, where `d` is that term's distance. An exact term weighs 1. Each further edit weighs strictly less. A prefix score is not multiplied that way.

The distance is optimal string alignment over Unicode scalar values (SPEC §7.1, ROADMAP 8.2.4.1). An insertion, a deletion, a substitution, or a swap of two adjacent characters counts as one edit, and no character is edited twice. A byte that is not valid UTF-8 counts as a character no query character equals. The automaton keeps the band of `2 · distance + 1` columns around the diagonal. Cells farther from the diagonal are already past the distance, so a step costs work proportional to the distance, not to the length of the query term. A query term may be 32 KiB (SPEC §7.1).

An explicit `fuzzy` clause in a hand-built query is different. `FuzzyQuery.term` is raw bytes. Planning does not analyze it, so `RUNNING` is not the indexed term `running`. L4 analyzes first, through the query the compiler already built, and only then constructs the clause. `Query::validate` accepts a distance of 1 or 2. `SearchPlan::with_terms` expands an explicit clause over the collection terms (ROADMAP 7.4.12). L0 of `search_relaxed` searches the query it was given, so that explicit clause runs as the exact step. The cascade still builds L4's own step when it reaches L4.

`QueryLimits` holds the step beside the earlier steps it excludes (SPEC §7.1.3). If the fuzzy clauses do not fit, their `max_expansions` are lowered, largest first. The step is listed as `fuzzy:cut`. If one term each still does not fit, the step is listed as `fuzzy:skipped` and is not searched. A query that rewrites nothing skips the level and is not listed in `applied`.

## Typo tiers

Within a level, a hit that needs fewer edits ranks above one that needs more, before BM25F (ROADMAP 8.2.7, [ADR-0046](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0046-typo-and-exactness-tiers-as-cascade-steps.md)). The tiers are extra steps, strictest first. They do not change a score. The edit-distance weight above is still applied inside a step. The tier only decides which step a document belongs to, and a stricter step ranks above a looser one at any score.

When a term L4 rewrites has 8 or more characters, L4 builds two steps. The first is `fuzzy:1`: the same query with every rewritten term capped at distance 1. The second is `fuzzy`, at each term's own distance. Each keeps its own 50 terms. The tier is the most edits any one rewritten word needs, not the sum across words. A query whose terms are all shorter has the one step. When `fuzzy:1` is the step that found the hits, `corrected` and `effective` name the terms within one edit.

The two steps are not nested. Once `fuzzy:1` is searched, `fuzzy` excludes it, and its expansions count again against `max_expansions`. A query with many long words can therefore reach `fuzzy:cut` or `fuzzy:skipped` sooner (ROADMAP 8.2.12, still open).

L3 has the same idea. When it expands a term besides the last one, and L2's one-term rung was not searched first, it builds `prefix:last` before `prefix`. That step is [prefix search](/guide/synonym-and-prefix-search/#prefix-search). L0 does not rank a word typed exactly above its stem: the index holds the stems alone (ROADMAP 8.2.10, still open). L1 is not tiered either.

## Did you mean

`corrected` is the did-you-mean list (ROADMAP 8.2.4.3). It is filled only when L4 is the step that found the hits (ROADMAP 8.1.22, [ADR-0043](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0043-relaxed-response-states-the-hit-step.md)). A later step that adds no hit does not replace `effective` or `corrected`.

A word is corrected when all of these hold:

- It is a required word of one token, a `term` or a `combined` over the default fields.
- No field of it holds that term. Collection document frequency is 0, so no document does.
- A field the step rewrites has a term within the word's distance that some document holds.

The correction is the term that field's `fuzzy` keeps first: the highest collection document frequency, ties broken by bytes, and across fields the highest frequency, ties to the field earlier in the `combined`. The step searches that term under any cut of the expansion cap, which keeps the most frequent terms. `corrected` lists each such word in text order, as the user wrote it (`Zapatilas`), with the correction as the index holds it (`zapatillas`: analyzed, so lowercased, and stemmed under a stemming analyzer).

`effective` is L0's text with each corrected word replaced by that index form. Other clauses, including excluded ones, stay as written. The string names the terms searched. It does not have to compile back to the step.

A word some document already holds is widened by its `fuzzy` and is not reported. It matched as typed. A word of several tokens (`e-mial`) is matched per token and is neither corrected nor rewritten in `effective`, which has no text per token. A programmatic query has no user text, so it has no `original`, no `effective`, and no `corrected`.

L2 can take the hits before L4 runs. A term no document holds is the least important term, below a stopword (ROADMAP 8.2.2.5), so `effective` on an L2 step does not name it when another term can be kept. For `red shoez` over documents that hold `red shoes`, L2's one-term rung already matches every document L4 could add. The response then states L2's `effective` (`red`) and an empty `corrected`, even if L4 was built and `level` is `fuzzy` (SPEC §7.6). A one-term query skips L2 (ROADMAP 8.2.2.3), so `runing` can reach L4. [How terms are dropped](/guide/typo-tolerant-search/#how-terms-are-dropped) is that order.

The engine value is `persia_engine::relax::Correction`: `original` and `corrected`, in a `Vec` on `Relaxation.corrected`. The object below is the shape in SPEC §7.6. There is no JSON search response on `persia::Db`.

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

`CollectionViews::search_relaxed` returns `RelaxedResults`. `Relaxation` is the cascade summary (ROADMAP 8.1.5). `level` is the last step built and not skipped. `applied` lists what each step after L0 did, in build order. A step cut to fit the limits ends with `:cut`. One that cannot fit ends with `:skipped` and is not searched. `original` is the user text. `effective` and `corrected` describe the hit step, the last step that found a hit. `truncated` means the time budget passed. `fallback` is set when L5 is that hit step. `filters_dropped` is set when the filter step is.

Each `RelaxedHit` carries `level`, `step` (0 is L0), and `rank_score`. Hits from a stricter step rank above hits from a looser one, before the request's sort (ROADMAP 8.1.3).

With `explain: true`, an explained hit of this search has a root `Explanation::Relaxed` (ROADMAP 8.3.4, SPEC §7.7). `RelaxedExplanation` names the same `level`, `step`, and `applied`, and wraps the step's own score tree. The score is the hit's, bit for bit. L5's inner node is `Explanation::StaticRank`. Highlighted snippets are not this tree. Highlights are post-v1. [Faceted search](/guide/faceted-search/) is the request that sets `explain`, and the deadline, which is separate from the relaxation budget.

## Fallback when nothing matches

L5 runs when `fallback` is `static_rank` (the default) and the earlier levels found fewer than `min_results` hits (ROADMAP 8.2.5, [ADR-0044](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0044-l5-fallback-static-rank-then-index-recency.md)). `fallback: "none"` builds no L5 step, and a miss can stay empty.

The step is L0's exclusions and filters around `match_all`, with the earlier steps excluded. Its hits are the live documents those filters keep, that the excluded clauses do not match, and that no earlier step matched. It has no query words to score. It ranks by the collection's static rank, then by index location: the memtable first, then segments from the last the manifest lists to the first, each by docid descending. That recency is where the index holds the document, not when it was written. A merge reorders it (ROADMAP 8.2.8, still open). Without a static rank, or with its weight 0, every score is 0 and recency alone orders the hits. The response then sets `fallback` to `recency`. Otherwise it sets `static_rank`.

The request's `sort` does not order these hits. Each hit still carries the request's sort values. `effective` is the excluded clauses, empty when there are none. There is no `corrected`.

## Filter relaxation

Filters stay on L0 through L5 unless the caller sets `relax.filters` to `"last_resort"` (ROADMAP 8.2.6, [ADR-0047](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0047-filter-relaxation-as-a-last-step.md)). That plans one last step, L6 `filters`, after every enabled earlier level. `fallback: "none"` omits L5 and L6 still comes last.

The step is built only when every earlier level found fewer than `min_results` hits. It is not built to fill a page, even with `fill`. It is L0's words with the request's filters removed. Excluded words stay excluded. A query that searches no word does not build it: empty text, stopwords only, exclusions alone, or a filter with no word. A compiled query with no request filter does not build it either.

For a query compiled from `q`, the filter it drops is the request filter. `Compiled::filter` keeps that filter beside the query, and `Compiled::with_query` joins it to any query put in its place, so no level before L6 loses it (ROADMAP 8.2.14, [ADR-0048](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0048-request-filter-kept-beside-the-compilation.md)). A filter inside the caller's own query stays, unless it is the same filter. A programmatic query drops every filter of its root `bool`.

When this step supplies the hits, the response sets `level` to `filters` and `filters_dropped` to true. Those hits do not pass the request's filters. Facets count them, so a facet can show a value the filter rejects (SPEC §7.5, an interim rule, ROADMAP 8.1.9). Do not opt in when a filter enforces tenancy or permissions: no request key marks a filter as one that must hold (ROADMAP 8.2.13, still open). The operators and the join are [Filtered full-text search](/guide/filtered-search/).

## Relax options

A query's `relax` is `false`, `true`, or an object of the collection's relaxation keys: `enabled`, `min_results`, `levels`, `fallback`, `fill`, `filters`, and `budget_ms` (ROADMAP 8.3.1, [ADR-0045](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0045-request-relax-options-and-cursor-settings.md)). `RelaxOptions::from_json` reads that JSON strictly. `RelaxPlan::new` merges it over the collection's `RelaxationConfig`, one key at a time. `relax: false` runs L0 only. `fill` keeps walking until the page window is full (ROADMAP 8.3.2). L6 still does not fill. A cursor records the settings, and a page under other settings is `RequestError::CursorRelax`.

This plan compiles against the current crate. It does not open a database or return hits:

```rust
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{Fallback, FilterRelaxation, Level, Levels, RelaxationConfig};

let collection = RelaxationConfig::default();
let options = RelaxOptions {
    min_results: Some(5),
    levels: Some(Levels::Custom(vec![Level::Fuzzy])),
    fallback: Some(Fallback::None),
    filters: Some(FilterRelaxation::LastResort),
    ..RelaxOptions::default()
};
let plan = RelaxPlan::new(&collection, &options)?;
assert_eq!(
    plan.levels().collect::<Vec<_>>(),
    [RelaxLevel::Exact, RelaxLevel::Fuzzy, RelaxLevel::Filters],
);
```

`levels` lists levels after L0. L5 is `fallback`, not an entry of that list. `[]` with the default fallback runs L0 and then L5. The limits a step is held to are the engine's `QueryLimits`, passed with `RelaxPlan::with_limits`, not a key of `relax`.

The call below is the intended API from SPEC §7.6. It is illustrative: these methods are not in the `persia` crate. `CollectionViews::search_relaxed` is the call that runs this cascade.

```rust
// ILLUSTRATIVE — these methods are not in the persia crate.
// CollectionViews::search_relaxed runs L4, and L5 unless fallback is none.
let db = persia::Db::open("app.persia")?;
let products = db.create_collection(schema)?;
let results = products.search("runing shoes")?;
let relaxation = &results.relaxation;
```

## What can still come back empty

- `fallback: "none"`, when L0 through L4 did not find `min_results` hits. A term shorter than 4 characters is not a fuzzy match.
- A filter no document passes, while `filters` is `never` (the default). L6 is the only step that drops it, and only for a query that searches a word.
- `RelaxOptions::disabled`, or a collection whose `RelaxationConfig.enabled` is false. L0 only.
- A fuzzy step skipped because it did not fit `QueryLimits`.

The zero-result regression gate is not written (ROADMAP 8.4). This project does not publish a zero-result rate.

[Getting started](/guide/getting-started/) separates `CollectionViews::search_relaxed` from the illustrative `Db::search` samples. [What is an embedded search database?](/guide/embedded-search/) is the library shape those samples are aiming at.

## FAQ

### Does Persia DB do fuzzy search in Rust today?

As library code, yes. L4 of `CollectionViews::search_relaxed` builds a `fuzzy` step from the exact query. A term of 4–7 characters matches within one edit, and a term of 8 or more within two. The distance is optimal string alignment, including a swap of two adjacent characters, walked as a banded automaton over each field's term dictionary. The compiler does not turn a typo into a `fuzzy` clause. There is no search method on `persia::Db`. %%RELAXATION_CELL%%

### How does did-you-mean spelling correction work?

When L4 is the step that found the hits, `Relaxation.corrected` lists each required one-token word that no document holds, and the indexed term the step searched instead. `effective` is the original text with those words replaced. A word some document already holds is widened, and is not reported as a correction. If an earlier step found the hits, `corrected` stays empty even when L4 was built. The value is a list of `{ original, corrected }`, not a public JSON API.

### What happens when fuzzy search still finds nothing?

L5 fallback, unless `fallback` is `none`, returns documents the filters keep and earlier steps did not match. It ranks them by static rank, then by where the index holds them, and the response sets `fallback` to `static_rank` or `recency`. That order ignores the request's `sort`. An explained hit has a root `Explanation::Relaxed` that names the level, the step, and what that step applied.

### Does fuzzy search drop filters?

Not by default. L1 through L5 keep the request filter. With `relax.filters` set to `"last_resort"`, one last step drops that filter when every earlier level found fewer than `min_results` hits and the query searches a word. A query that searches no word does not build the step. `filters_dropped` is true only when that step found the hits.

### Can I call Db::open and run a fuzzy query?

No. `persia::Db` is not in the crate, and `Database` has no `search` method. The engine call is `CollectionViews::search_relaxed` on a collection snapshot. `RelaxPlan` and `RelaxOptions` are library code. Code samples that call `Db::search` on this site are illustrative.

## Read next

- [Typo-tolerant search](/guide/typo-tolerant-search/) — the whole cascade, `minimum_should_match`, and how L2 drops a term before L4 runs.
- [Synonym and prefix search](/guide/synonym-and-prefix-search/) — L1 synonym groups and L3 prefix expansion, the steps before fuzzy.
- [BM25F ranking](/guide/ranking/) — the score inside a step, the edit-distance weight, static rank on L5, and proximity off by default.
- [Filtered full-text search](/guide/filtered-search/) — the filter DSL, how a request filter joins the query, and when L6 drops it.
- [Faceted search](/guide/faceted-search/) — filters, facets, sort, cursor pages, deadlines, and explain. The deadline is not the relaxation budget.
- [Comparison](/guide/comparison/) — next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
- [What is an embedded search database?](/guide/embedded-search/) — in-process full-text search, one file, and no `persia::Db` yet.
- [FAQ](/guide/faq/) — production readiness, the file format, and the license.
- [Getting started](/guide/getting-started/) — crates you can call, and the API that is still illustrative.
- [Feature reference](/guide/features/#zero-results-relaxation) — the level table and the schema sketch that carries `relaxation`.
