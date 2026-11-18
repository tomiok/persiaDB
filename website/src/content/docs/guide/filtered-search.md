---
title: "Filtered full-text search"
description: "How Persia DB filters full-text search in Rust: term, range, and exists restrict hits. They leave the BM25F score unchanged. Dropping filters is opt-in."
head:
  - tag: title
    content: "Filtered full-text search in Rust: filter syntax and relaxation | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Filtered full-text search in Rust: filter syntax and relaxation | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Filtered full-text search in Rust: filter syntax and relaxation | PersiaDB"
  - tag: script
    attrs:
      type: application/ld+json
    content: |
      {
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "name": "Filtered full-text search",
        "url": "https://tomiok.github.io/persiaDB/guide/filtered-search/",
        "mainEntity": [
          {
            "@type": "Question",
            "name": "Does Persia DB filter full-text search in Rust today?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "As library code, yes. Filter::from_json reads term, terms, range, exists, and, or, and not. Compiled::with_filter joins that filter to a compiled text query. CollectionViews::search returns the documents that pass, scored by the text. There is no search method on persia::Db or on Database. %%MATCHING_CELL%%"
            }
          },
          {
            "@type": "Question",
            "name": "Which fields can a filter use?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "term and terms take keyword, i64, f64, bool, and datetime. range takes i64, f64, and datetime, with gt, gte, lt, or lte. exists takes any field except stored. A keyword is matched as written, not analyzed. A text field is not a term or range filter. A bad filter is an error."
            }
          },
          {
            "@type": "Question",
            "name": "Do filters change the BM25F score?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. A filter selects documents and adds 0 to the score. A document that matches the text and the filter scores as the same text query without the filter. must and should clauses score. filter and must_not do not. Explain leaves filters out of the score tree."
            }
          },
          {
            "@type": "Question",
            "name": "Can a filter be dropped when a search has too few hits?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Only with relax.filters set to last_resort. The default is never. L1 through L5 keep the request filter. The opt-in adds one last step, L6, which drops that filter when every earlier level found fewer than min_results hits and the query searches a word. filters_dropped is true only when that step supplies the hits. There is no key yet for a filter that must always hold."
            }
          },
          {
            "@type": "Question",
            "name": "Can I call Db::open and pass a filter?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. persia::Db is not in the crate, and Database has no search method. The engine call is CollectionViews::search on a collection snapshot, with the filter already joined to the query. SearchRequest has no filter field. Code samples that call Db::search on this site are illustrative."
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

A filtered full-text search here is a text query plus a structured predicate. The words are analyzed and scored with BM25F. The filter decides which of those documents are hits. It does not change the score ([SPEC §7.1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#71-query-model), [§7.1.2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#712-filters), [§7.2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#72-scoring-bm25f)).

%%MATCHING_CELL%% There is no `search` on `persia::Db` or on `Database` (M10). `SearchRequest` has a sort, a page, facets, a projection, and `explain`. It has no filter field. The filter is part of the query: `Filter::from_json` parses it, and `Compiled::with_filter` joins it. The call that returns hits is `CollectionViews::search` (or `search_relaxed`, when the cascade may run). How the score is computed is [BM25F ranking](/guide/ranking/). Facets, sort, cursors, and `explain` are [Faceted search](/guide/faceted-search/).

## Operators

`filter` is one JSON object with exactly one key, the operator. Keys are not duplicated. `Filter::from_json` rejects JSON longer than 1 MiB, an unknown operator, an unknown field, and a value of the wrong type. A bad filter is an error. User text is not: the compiler reads bad syntax as plain terms ([SPEC §7.1.1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#711-user-text)).

| Form | Fields | Selects |
|---|---|---|
| `{"term": [field, value]}` | `keyword`, `i64`, `f64`, `bool`, `datetime` | Documents whose field holds `value`. |
| `{"terms": [field, [values]]}` | The same fields | Documents holding any listed value. `[]` selects none. |
| `{"range": [field, {bounds}]}` | `i64`, `f64`, `datetime` | Documents whose value is inside the bounds. |
| `{"exists": field}` | Any field except `stored` | Documents that have a value. On `text`, at least one token. |
| `{"and": [filters]}` | | Documents every filter selects. `[]` selects every document. |
| `{"or": [filters]}` | | Documents any filter selects. `[]` selects none. |
| `{"not": filter}` | | Documents the inner filter does not select. |

A range bound is `gt`, `gte`, `lt`, or `lte`. At least one bound is required, and at most one per side. A range with the lower bound above the upper is an error, and so is one whose bounds are equal with either side exclusive. `gte` 5 and `lte` 5 is that one value. `keyword` and `bool` are not range fields.

`text` is not a `term` or `range` field. A word limited to one field is query syntax, `field:word` inside `q`, and that clause scores. A `keyword` after `field:` is the text as written, unanalyzed, and it still scores. A filter `term` on the same keyword does not.

Only `text` and `keyword` hold more than one value (SPEC §6.4). A `term` matches when one of those strings is the value. `i64`, `f64`, `bool`, and `datetime` hold one value each. A document with no value in the field fails `term`, `terms`, `range`, and `exists`.

## Values

A value uses the field's JSON form (SPEC §6.4.1) and must be a valid value of that field.

| Field | JSON | Rule |
|---|---|---|
| `keyword` | string | At most 32,768 bytes. Matched as written. |
| `i64` | number | An integer in the `i64` range. A fraction or an exponent is an error. `-0.0` is 0. |
| `f64` | number | `-0.0` is 0. NaN is rejected. |
| `bool` | `true` or `false` | |
| `datetime` | number or string | Microseconds, or an RFC 3339 string, in `0000-01-01T00:00:00Z` through `9999-12-31T23:59:59.999999Z`. |

This object is what `Filter::from_json` parses. It is not a search-request parser. Nothing in the crate reads the whole SPEC §7.1 request object (`q`, `fields`, `filter`, `sort`, `limit`) as one JSON value.

```json
{"and": [{"term": ["brand", "nike"]}, {"range": ["price", {"lte": 100}]}]}
```

A filter also counts against `QueryLimits` (SPEC §7.1.3). Each value, including each `terms` entry and each range bound, is a leaf term. Each node is a clause. Defaults are 1,024 terms, 1,024 clauses, and depth 32. `Filter::from_json` and `Filter::validate` reject a filter past a limit.

## How a filter joins the text

`QueryCompiler::compile` turns `q` into a query and never fails (SPEC §7.1.1). `Compiled::with_filter` then joins the request filter as the `filter` clauses of the root `bool` (ROADMAP 8.1.17).

When that query is a `bool` with no `should` clauses and no `minimum_should_match`, the filters are appended to its `filter` list. A filter already on that root is not added twice. Any other query is wrapped: `bool { must: [query], filter }`. A query of one required part that is itself a `bool` is wrapped too, so a filter beside its `should` clauses does not make those clauses optional. One word is that one part, so a filter wraps it. Two required words are already a `bool`, so the filter is added to that root.

The compilation keeps the request filter beside the query (`Compiled::filter`). `Compiled::with_query` joins that same filter onto whatever query a caller puts in its place (ROADMAP 8.2.14). A level of the cascade cannot lose it by replacing the query.

Several filters on a root `bool` all apply. A document matches when it matches every `must` and every `filter`, and no `must_not`. `should` clauses are optional when a `must` or a `filter` is present. `minimum_should_match` of 1 or more makes that many `should` clauses required beside the filter (SPEC §7.1). The count changes which documents match. It does not change a score.

## Scoring

`must` and `should` score. `filter` and `must_not` add 0 (SPEC §7.2). A document that matches the text and the filter receives the score of that text query alone. A `bool` with filters and no `must` or `should` scores 0 for every document it selects. Matching `should` clauses still add their scores. The proximity bonus, off unless `SearchPlan::with_proximity` is called, is not applied to a filter. [BM25F ranking](/guide/ranking/#clauses) is the formula and the bonus.

`explain: true` leaves filters and `must_not` out of the per-hit tree. The root score is still the hit's score. [Faceted search](/guide/faceted-search/#explain) is that tree.

## Evaluation

Before a document is scored, a `bool` turns its filters into a bitmap of the live documents they select, per segment or memtable view (ROADMAP 7.4.5). The matcher stays inside that bitmap. Deleted documents are removed from it.

A `keyword` `term` or `terms` reads that field's postings. A `term` or `terms` on `i64`, `f64`, `datetime`, or `bool`, and every `range`, scans the column's order-preserving keys. A `range` skips FOR blocks whose keys cannot match (ROADMAP 7.4.13). `exists` reads which documents have a value. `not` is the complement among the view's documents. A selective `must` may check a column filter per candidate instead of building the whole bitmap (ROADMAP 7.4.13, 7.4.16).

## Facets

A facet counts live matching documents, over every match, not only the page. The filter has already restricted that set, so the counts are the values those hits hold. With `filters: "last_resort"`, the filter step's matches are counted too, and those documents fail the request filter (SPEC §7.5). Which matches a facet should keep once the cascade loosens a query is an interim rule (ROADMAP 8.1.9). Keyword facets, sizes, and sort are [Faceted search](/guide/faceted-search/#facets).

## Relaxation

Filters stay on every step unless the caller opts in (SPEC §7.6, ROADMAP 8.2.6). The collection default is `relaxation.filters: "never"` (`FilterRelaxation::Never`). A query overrides it with `RelaxOptions`, including `{ "filters": "last_resort" }`. `RelaxOptions::from_json` reads that object. `relax: false` runs the exact query only, so it never builds the filter step.

`last_resort` plans one step after every other enabled level, L6 `filters`. `fallback: "none"` omits L5, and L6 still comes last. The step is L0's query with the request's filters removed from the root `bool`. Excluded words stay excluded. A filter deeper in the query stays. On a programmatic query the step drops every filter of the root `bool`. On a compiled query it drops the request filter `Compiled::filter` records, including from a root the join wrapped. A filter the caller's own root holds stays, unless it is equal to a request filter: the join does not add that filter twice, so the two cannot be told apart.

The step is built only when every earlier level found fewer than `min_results` hits (default 1). `fill` does not build it to pad a page. A query that searches no word builds none: empty text, stopwords only, exclusions alone, or a filter with no word. Without its filters that query would match every live document the exclusions leave, at score 0. A compiled query with no request filter builds none either.

L1 through L5 keep the filter. An uncut L5 step returns documents those filters keep, so with L5 in the plan, L6 runs only when fewer than `min_results` documents pass the filters and the excluded clauses at all. Its hits are then the documents L0's words match that the filters reject and that no earlier step matched. They rank by the request's sort. When this step supplies the hits, the response sets `level` to `filters` and `filters_dropped` to true. Those hits do not pass the request filter. [Fuzzy search](/guide/fuzzy-search/#filter-relaxation) is the same step beside L4 and L5. [Typo-tolerant search](/guide/typo-tolerant-search/) is the rest of the cascade.

There is no request key for a filter that must always hold. `last_resort` drops every request filter of the root together. A filter that enforces tenancy or permissions must not opt in (ROADMAP 8.2.13, still open).

### Nested steps and the filter

Each step after the first excludes the earlier steps, so a document is a hit of the first step that matches it (SPEC §7.6, ROADMAP 8.2.2.4). L2's steps are nested. Each one matches every document the earlier L2 steps match, with the same `must_not` and the same `filter`, and a lower `minimum_should_match`. Excluding the latest searched L2 step excludes the earlier ones as well. L1's steps are not nested, and each of them stays excluded. L3 and L4 are not nested either.

An uncut L5 step matches every document any earlier step matches, because it is `match_all` under L0's exclusions and filters, which those steps keep. L6 then excludes that uncut L5 step alone. A step cut to fit `QueryLimits` matches less than it was built to, so the earlier steps stay excluded beside it.

## What you can call

This compiles against `persia-engine`. `SimFile` is the in-memory stand-in tests use. A real file is `Database::open` on a path, which is still not `persia::Db::open`.

```rust
use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{Database, DatabaseOptions, SearchRequest};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, Filter, QueryCompiler, QueryLimits};
use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

let schema = SchemaBuilder::new("items")
    .field("title", FieldKind::Text(TextOptions::default()))
    .field("brand", FieldKind::Keyword(KeywordOptions { facet: true, ..KeywordOptions::default() }))
    .field("price", FieldKind::I64 { stored: true })
    .build()?;
let brand = schema.field("brand").map(|f| f.id()).ok_or("no field")?;
let price = schema.field("price").map(|f| f.id()).ok_or("no field")?;
let title = schema.field("title").map(|f| f.id()).ok_or("no field")?;
let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default())?;
db.create_collection(schema.clone())?;
db.put(
    "items",
    &Document::new("a")
        .with(title, Value::String("red shoes".into()))
        .with(brand, Value::String("nike".into()))
        .with(price, Value::I64(80)),
    Durability::Fsync,
)?;
let snapshot = db.snapshot();
let items = snapshot.collection("items").ok_or("no collection")?;
let memtable = items.memtable();
let limits = QueryLimits::default();
let filter = Filter::from_json(
    r#"{"and":[{"term":["brand","nike"]},{"range":["price",{"lte":100}]}]}"#,
    items.schema(),
    &limits,
)?;
let compiled = QueryCompiler::new(items.schema(), &AnalyzerRegistry::builtin())?
    .compile("red shoes", &DefaultFields::all(items.schema()), &limits)
    .with_filter([filter]);
let plan = SearchPlan::new(compiled.query(), items.schema(), &items.search_statistics(&memtable))?;
let page = items.views(&memtable)?.search(&plan, &SearchRequest::default(), Pruning::default())?;
```

The opt-in is a `RelaxPlan`. With the collection defaults, the filter step is last:

```rust
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::RelaxationConfig;

let options = RelaxOptions::from_json(r#"{"filters": "last_resort"}"#)?;
let plan = RelaxPlan::new(&RelaxationConfig::default(), &options)?;
assert_eq!(plan.levels().last(), Some(RelaxLevel::Filters));
```

`CollectionViews::search_relaxed` runs that plan. Passing it `&compiled` keeps the user text on the response. `CollectionViews::search` runs the joined query alone, which is `relax: false`: L0, filters held.

## What is still planned

| Piece | Status |
|---|---|
| Filter DSL, schema checks, bitmap evaluation, join onto a compiled query | Library code in `persia-engine`. |
| Filters add 0 to the BM25F score | Library code (SPEC §7.2). |
| `filters: "last_resort"` (L6) and nested L2 exclusion | Library code (ROADMAP 8.2.6, 8.2.2.4). Default is `"never"`. |
| `persia::Db::search` and one JSON request object | Not in the crate (M10). `SearchRequest` has no filter field. |
| A filter that `last_resort` must not drop | Not implemented (ROADMAP 8.2.13). |
| Geo bounding box, circle, and distance sort | Not implemented (M29). |
| A document access filter the request cannot turn off | Not implemented (M30). |
| Numeric or range facets | Not in SPEC §7.5. Facets are keyword values. |

## FAQ

### Does Persia DB filter full-text search in Rust today?

As library code, yes. `Filter::from_json` reads `term`, `terms`, `range`, `exists`, `and`, `or`, and `not`. `Compiled::with_filter` joins that filter to a compiled text query. `CollectionViews::search` returns the documents that pass, scored by the text. There is no `search` on `persia::Db` or on `Database`. %%MATCHING_CELL%%

### Which fields can a filter use?

`term` and `terms` take `keyword`, `i64`, `f64`, `bool`, and `datetime`. `range` takes `i64`, `f64`, and `datetime`, with `gt`, `gte`, `lt`, or `lte`. `exists` takes any field except `stored`. A keyword is matched as written, not analyzed. A `text` field is not a `term` or `range` filter. A bad filter is an error.

### Do filters change the BM25F score?

No. A filter selects documents and adds 0 to the score. A document that matches the text and the filter scores as the same text query without the filter. `must` and `should` clauses score. `filter` and `must_not` do not. Explain leaves filters out of the score tree.

### Can a filter be dropped when a search has too few hits?

Only with `relax.filters` set to `last_resort`. The default is `never`. L1 through L5 keep the request filter. The opt-in adds one last step, L6, which drops that filter when every earlier level found fewer than `min_results` hits and the query searches a word. `filters_dropped` is true only when that step supplies the hits. There is no key yet for a filter that must always hold. %%RELAXATION_CELL%%

### Can I call Db::open and pass a filter?

No. `persia::Db` is not in the crate, and `Database` has no `search` method. The engine call is `CollectionViews::search` on a collection snapshot, with the filter already joined to the query. `SearchRequest` has no filter field. Code samples that call `Db::search` on this site are illustrative.

## Read next

- [Faceted search](/guide/faceted-search/) — keyword facets, sort, cursor pages, deadlines, and explain. Facets count the matches the filter left.
- [BM25F ranking](/guide/ranking/) — field weights, collection statistics, and why a filter adds nothing to the score.
- [Typo-tolerant search](/guide/typo-tolerant-search/) — the cascade, `minimum_should_match`, and how L2 keeps the filter on every rung.
- [Fuzzy search and did-you-mean](/guide/fuzzy-search/) — L4, L5 fallback, and the `last_resort` filter step.
- [Synonym and prefix search](/guide/synonym-and-prefix-search/) — L1 and L3. Both keep the request filter.
- [What is an embedded search database?](/guide/embedded-search/) — in-process search, one file, and no `persia::Db` yet.
- [Feature reference](/guide/features/) — the status table and the relaxation levels.
- [FAQ](/guide/faq/) — production readiness, the file format, and the license.
