---
title: "Faceted search, sort, and cursor pagination"
description: "Keyword facets, column sort, search_after cursors, deadlines, and score explain in Persia DB. What runs as library code on a collection snapshot, and what is still planned."
head:
  - tag: title
    content: "Faceted search in Rust: facets, sort, and cursor pagination | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Faceted search in Rust: facets, sort, and cursor pagination | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Faceted search in Rust: facets, sort, and cursor pagination | PersiaDB"
  - tag: script
    attrs:
      type: application/ld+json
    content: |
      {
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "name": "Faceted search, sort, and cursor pagination",
        "url": "https://tomiok.github.io/persiaDB/guide/faceted-search/",
        "mainEntity": [
          {
            "@type": "Question",
            "name": "Does Persia DB do faceted search today?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes, as library code over a collection snapshot. %%MATCHING_CELL%% There is no search method on persia::Db or on Database. Facets count keyword fields marked facet true, over every match, not only the page."
            }
          },
          {
            "@type": "Question",
            "name": "Which fields can be facets?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "A keyword field with facet true. A request lists at most 16 facets. Each returns the 1 to 1000 most common values, 10 unless you ask otherwise. Text, numbers, and bool are not facet fields. Counts are exact and merged by the value bytes."
            }
          },
          {
            "@type": "Question",
            "name": "How do sort and cursor pagination work?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The default sort is score descending, then segment order with the memtable last, then docid. A sort has at most 16 keys: score, or a keyword, i64, f64, datetime, or bool column. A page is offset and limit, at most 10000 together, limit 20 by default. search_after is the cursor of a previous hit and is valid only for that snapshot, collection, and memtable view."
            }
          },
          {
            "@type": "Question",
            "name": "Can a search set a deadline?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes. Execution::deadline stops the search once a clock passes an instant, and a CancelToken cancels it. That is separate from the relaxation budget, which starts after the exact level and does not loosen a query until later cascade levels exist."
            }
          },
          {
            "@type": "Question",
            "name": "Does explain return highlighted snippets?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. explain true returns a per-hit score tree for the hits on the page. Highlighted fragments are post-v1. The tree's root score is the hit's score."
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

Faceted search here means a ranked page plus counts of the keyword values those matches hold. Sort, `offset` / `limit`, a `search_after` cursor, a deadline, and `explain` are the same request ([SPEC §7.4](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#74-sorting-pagination), [§7.5](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#75-facets), [§7.7](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#77-highlighting--explain)).

%%MATCHING_CELL%% There is no `search` on `persia::Db` or on `Database` (M10). The call is `CollectionViews::search` (or `search_with`, when you pass threads, a deadline, or cancellation). How a typo is supposed to avoid an empty page is a different feature: %%RELAXATION_CELL%% See [Typo-tolerant search](/guide/typo-tolerant-search/).

## Filters

A filter is not part of the score. The DSL is `term`, `terms`, `range`, `exists`, `and`, `or`, and `not` (SPEC §7.1.2). `Filter::from_json` parses them and checks them against the schema. A bad filter is an error. User text is not: the compiler degrades bad syntax to plain terms.

Evaluation is library code (ROADMAP 7.4.5). A `bool` keeps a bitmap of the documents that pass, per segment or memtable view, and the matcher stays inside it. Deleted documents are already left out of the view. Filters stay strict when a query has too few hits, unless the caller sets `relax.filters` to `last_resort`. That opt-in does not run yet: levels after exact build no query. %%RELAXATION_NOW%%

## Facets

A facet is a `keyword` field with `facet: true` (SPEC §6.2.2, §7.5). `FacetRequest::new` rejects any other field, and a size outside 1 to 1,000 (`MAX_FACET_SIZE`). The default size is 10. A request lists at most 16 facets (`MAX_FACETS`). The same field may be listed twice.

Each facet counts live matching documents that hold the value, once per distinct value, over every match, not only the page. Per segment the count reads the keyword column for the matching bitmap. In the memtable it reads postings. Views merge by the value's bytes, so the counts are exact. The response keeps the `size` values with the highest count, then ascending bytes. A value no match holds is absent.

With a facet, every match is visited. Without one, a score sort may skip documents that cannot enter the page (SPEC §7.3, §7.4).

`CollectionViews::search_relaxed` counts the matches of every cascade step it built, each document once, at its first step. SPEC §7.5 marks that as an interim rule (ROADMAP 8.1.9). Today only the exact level builds a query, so the count is the exact query's matches.

## Sort

The default is score descending. A `Sort` has at most 16 keys (`MAX_SORT_KEYS`). A key is the score, or a field that has a column: `keyword`, `i64`, `f64`, `datetime`, or `bool`. `text` and `stored` have no column, and `Sort::new` rejects them. `order` is ascending or descending. `missing` is `last` (the default) or `first`, whichever way the values go.

Numbers and datetimes compare by value. `false` is before `true`. Keywords compare by bytes. A document with several keyword values sorts by its smallest ascending and by its largest descending. Every hit still carries its score.

After the keys, ties break by segment order (the manifest's), the memtable last, then docid (SPEC §7.3). A collection-wide default tie-break would be a schema option and a new manifest version. It is not part of v1 (ROADMAP 32.5.1).

Pruning applies when the first key is the score descending: a document that cannot beat the worst kept hit is skipped. Further keys, such as score then a timestamp, skip only a document that scores strictly below that hit. A numeric or datetime key with `missing: last` can skip FOR blocks whose keys all rank after the worst hit the view keeps. A sort led by score ascending, by a keyword or bool, or by a column with `missing: first` visits every match. Either way the hits are the ones a full scan would keep.

## Pages and cursors

`offset` defaults to 0 and `limit` to 20 (`DEFAULT_LIMIT`). `offset + limit` is at most 10,000 (`MAX_WINDOW`). Each view keeps its best `offset + limit` hits. They merge by the sort, then by location. The page is that merge, then each hit's stored fields, after the projection: every stored field, none, or a list of names.

`search_after` is the cursor of a hit already returned (`SearchHit::cursor`). It holds one sort value per key, the hit's location, the snapshot `seq`, the collection name, and the memtable view's version. A search on another snapshot, collection, or memtable view is `RequestError::CursorView`: locations are indexes into the view that produced the cursor, so reusing it would repeat a tied hit or skip one. A cursor whose values do not match the sort is an error. `offset` still applies after the cursor.

A cursor from `search_relaxed` also records the hit's cascade step. `CollectionViews::search` rejects a cursor from a step after exact (`RequestError::CursorStep`).

This is the shape the engine runs. It is not the public `persia::Db` API:

```rust
use persia_engine::db::{Database, DatabaseOptions, SearchRequest};
use persia_engine::document::{Document, Value};
use persia_engine::query::Query;
use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder};
use persia_engine::search::{FacetRequest, Order, Pruning, SearchPlan, Sort, SortKey};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

let schema = SchemaBuilder::new("items")
    .field("brand", FieldKind::Keyword(KeywordOptions { facet: true, ..KeywordOptions::default() }))
    .field("price", FieldKind::I64 { stored: true })
    .build()?;
let brand = schema.field("brand").map(|f| f.id()).ok_or("no field")?;
let price = schema.field("price").map(|f| f.id()).ok_or("no field")?;
let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default())?;
db.create_collection(schema)?;
db.put("items", &Document::new("a").with(price, Value::I64(30)), Durability::Fsync)?;
let snapshot = db.snapshot();
let items = snapshot.collection("items").ok_or("no collection")?;
let memtable = items.memtable();
let plan = SearchPlan::new(&Query::MatchAll, items.schema(), &items.search_statistics(&memtable))?;
let request = SearchRequest {
    sort: Sort::new([SortKey::field(price, Order::Asc)], items.schema())?,
    facets: vec![FacetRequest::new(brand, 5, items.schema())?],
    limit: 20,
    ..SearchRequest::default()
};
let page = items.views(&memtable)?.search(&plan, &request, Pruning::default())?;
let next = SearchRequest {
    search_after: page.hits.last().map(|hit| hit.cursor()),
    ..request
};
```

`SimFile` is an in-memory stand-in used by tests. A real file is `Database::open` on a path, which exists on `persia_engine::db::Database` and still is not `persia::Db::open`.

## Deadlines

`CollectionViews::search_with` takes an `Execution` (SPEC §7.3, ROADMAP 7.6). `Execution::deadline` stops the search with `Interrupt::DeadlineExceeded` once the clock reads the instant or later. `Execution::cancel` stops it with `Interrupt::Cancelled` once the token is cancelled. The check runs before each view, every 256 matches of a view, and every 1,024 steps of its readers (postings, terms, columns). The error is `RequestError::Interrupted`, before the page's documents are read. The default execution has neither a deadline nor a token, and it prunes on the global thread pool. The merged page does not depend on which view finishes first.

That deadline is not the relaxation budget. `budget_ms` (default 50) is read after the exact level finishes, so the exact level is never cut by it (SPEC §7.6, ROADMAP 8.1.4). The request deadline still applies to every step, including exact. Because later levels build no query, a typo does not spend that budget on a looser query.

## Explain

`SearchRequest.explain` defaults to false. Set it and each hit on the page carries an `Explanation`: a tree of the clauses that matched that document (SPEC §7.7, ROADMAP 7.7). A word reports `idf`, `k1`, `tf~`, and per field the frequency, norm, and contribution. A phrase reports its terms and phrase frequency. A `bool` lists `must` clauses and the `should` clauses that matched. Filters and `must_not` score nothing and are left out. The root score is the hit's score, bit for bit.

Explain covers the page's hits only, one pass per view. It costs about one unpruned search of each view that holds a hit on the page. A deadline is read during that pass too.

Highlighted snippets are not this tree. Highlights are post-v1 (SPEC §7.7). Which relaxation level a hit matched joins the explanation later (ROADMAP 8.3.4). Today a hit of `search_relaxed` already states its level on the hit, and that level is exact until later levels build a query.

## What is still planned

| Piece | Status |
|---|---|
| Filters, facets, sort, cursor pages, deadlines, explain | %%MATCHING_CELL%% |
| `persia::Db::search` | Not in the crate (M10). |
| Cascade levels after exact | %%RELAXATION_CELL%% |
| Facet counts once the cascade loosens a query | Interim rule in the orchestrator (ROADMAP 8.1.9). Only exact runs today. |
| Highlighted fragments | Post-v1. |
| A collection default for tie-break | Post-v1 (ROADMAP 32.5.1). |
| Numeric or range facets | Not in SPEC §7.5. Facets are keyword values. |

## FAQ

### Does Persia DB do faceted search today?

Yes, as library code over a collection snapshot. %%MATCHING_CELL%% There is no `search` on `persia::Db` or on `Database`. Facets count `keyword` fields marked `facet: true`, over every match, not only the page.

### Which fields can be facets?

A `keyword` field with `facet: true`. A request lists at most 16. Each returns the 1 to 1,000 most common values, 10 unless you ask otherwise. Text, numbers, and bool are not facet fields. Counts are exact and merged by the value's bytes.

### How do sort and cursor pagination work?

The default sort is score descending, then segment order with the memtable last, then docid. A sort has at most 16 keys: score, or a `keyword`, `i64`, `f64`, `datetime`, or `bool` column. A page is `offset` and `limit`, at most 10,000 together, `limit` 20 by default. `search_after` is the cursor of a previous hit and is valid only for that snapshot, collection, and memtable view.

### Can a search set a deadline?

Yes. `Execution::deadline` stops the search once a clock passes an instant, and a `CancelToken` cancels it. That is separate from the relaxation budget, which starts after the exact level. %%RELAXATION_CELL%%

### Does explain return highlighted snippets?

No. `explain: true` returns a per-hit score tree for the hits on the page. Highlighted fragments are post-v1. The tree's root score is the hit's score.

## Read next

- [Typo-tolerant search](/guide/typo-tolerant-search/) — the cascade that would loosen a query with too few hits, and what of it runs.
- [Feature reference](/guide/features/) — the status table, the schema sketch, and the relaxation levels.
- [Features](/features/) — the short status table.
- [Getting started](/guide/getting-started/) — crates you can call, and the `Db` API that is still illustrative.
- [Comparison](/guide/comparison/) — next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
- [What is an embedded search database?](/guide/embedded-search/) — in-process search, one file, and no server.
- [FAQ](/guide/faq/) — production readiness, the file format, and the license.
