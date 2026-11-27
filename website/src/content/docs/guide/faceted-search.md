---
title: "Faceted search, sort, and cursor pagination"
description: "Keyword facets, column sort, cursor pages, and score explain in Persia DB, through Collection::search and the request JSON."
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
              "text": "Yes. Collection::search takes facets on a SearchRequest, and Collection::search_json takes them in the request JSON. Facets count keyword fields marked facet true, over every match, not only the page."
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
              "text": "The default sort is score descending, then segment order with the memtable last, then docid. A sort has at most 16 keys: score, or a keyword, i64, f64, datetime, or bool column. A page is offset and limit, at most 10000 together, limit 20 by default. A cursor is the token of a hit already returned, passed to SearchRequest::after or search_after, and is valid only for the same request on the same snapshot, collection, and memtable view."
            }
          },
          {
            "@type": "Question",
            "name": "Can a search set a deadline?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Not through the embedded API yet: a SearchRequest has no deadline. An async search (AsyncCollection::search) whose future is dropped stops at the engine's next check and returns nothing. The relaxation budget, budget_ms, is a different thing: it starts after the exact level and cuts only the looser levels."
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

Faceted search here means a ranked page plus counts of the keyword values those matches hold. Sort, `offset` / `limit`, a cursor, and `explain` are parts of the same request ([SPEC §7.4](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#74-sorting-pagination), [§7.5](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#75-facets), [§7.7](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#77-highlighting--explain)).

A search is `Collection::search`, which takes a `SearchRequest` built in code, or `Collection::search_json`, which takes the same request as JSON (SPEC §7.1.4). Both run the relaxation cascade unless the collection or the request turns it off, so a typo does not empty the page: see [Typo-tolerant search](/guide/typo-tolerant-search/). %%MATCHING_CELL%%

The samples open a database in a temporary directory (the `tempfile` crate); any path works.

```rust
use persia::{Db, FacetRequest, Order, SearchRequest, Sort, SortKey, SortValue};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("shop.persia"))?;
    let products = db.create_collection_json(
        r#"{"name": "products", "fields": {
            "title": {"type": "text"},
            "brand": {"type": "keyword", "facet": true},
            "price": {"type": "i64", "stored": true}
        }}"#,
    )?;
    products.put_json(r#"{"id": "p1", "title": "running shoes", "brand": "nike", "price": 140}"#)?;
    products.put_json(r#"{"id": "p2", "title": "running shoes", "brand": "acme", "price": 60}"#)?;
    products.put_json(r#"{"id": "p3", "title": "trail running shoes", "brand": "nike", "price": 90}"#)?;
    products.put_json(r#"{"id": "p4", "title": "rain coat", "brand": "acme", "price": 50}"#)?;

    // Sort keys and facets name fields by id, from the schema.
    let schema = products.schema()?;
    let brand = schema.field("brand").map(|f| f.id()).ok_or("no brand field")?;
    let price = schema.field("price").map(|f| f.id()).ok_or("no price field")?;
    let request = SearchRequest::new("running shoes")
        .sort(Sort::new([SortKey::field(price, Order::Asc)], &schema)?)
        .facets([FacetRequest::new(brand, 5, &schema)?])
        .limit(2);

    let first = products.search(&request)?;
    let ids: Vec<&str> = first.hits.iter().map(|h| h.document.id()).collect();
    assert_eq!(ids, ["p2", "p3"]); // the cheapest shoes first; the rain coat does not match
    assert_eq!(first.hits[0].sort_values(), [SortValue::I64(60)]);

    // The facet counts every match (three pairs of shoes), not only the page of two.
    let brands: Vec<(&[u8], u64)> =
        first.facets[0].values.iter().map(|v| (v.value.as_slice(), v.count)).collect();
    assert_eq!(brands, [(&b"nike"[..], 2), (&b"acme"[..], 1)]);

    // The next page starts after the last hit, for the same request.
    let cursor = first.next_cursor().ok_or("no cursor")?;
    let second = products.search(&request.clone().after(cursor))?;
    let ids: Vec<&str> = second.hits.iter().map(|h| h.document.id()).collect();
    assert_eq!(ids, ["p1"]);

    // The same request as JSON: fields by name.
    let json = products.search_json(
        r#"{"q": "running shoes", "sort": [{"field": "price", "order": "asc"}],
            "facets": [{"field": "brand", "size": 5}], "limit": 2}"#,
    )?;
    assert_eq!(json.to_json()?, first.to_json()?);

    db.close()?;
    Ok(())
}
```

## Filters

A filter is not part of the score. The DSL is `term`, `terms`, `range`, `exists`, `and`, `or`, and `not` (SPEC §7.1.2). A request takes them as JSON (`SearchRequest::filter_json`, `soft_filter_json`, or the `filter` and `soft_filter` keys of a request JSON) or as a typed `persia::Filter`, and the search checks them against the schema. A bad filter is an error. User text is not: bad syntax in `q` is read as plain terms.

The search keeps the documents that pass a filter and matches only inside them. Deleted documents are already left out. Filters stay strict when a query has too few hits, unless the request (or the collection) sets `relax.filters` to `"last_resort"`. L1 through L5 keep both filters on every step. The opt-in adds one last step, L6, which drops the request's `soft_filter` only when every earlier level found fewer than `min_results` hits, the query searches a word, and the request has a `soft_filter` (ROADMAP 8.2.6). The hard `filter` stays on every level, L6 included (ROADMAP 8.2.13). L6's matches count in the facets, so a facet then shows values the `soft_filter` rejects, and when this step supplies the hits `Relaxation::filters_dropped` is true. %%RELAXATION_NOW%% The operators and `last_resort` are [Filtered full-text search](/guide/filtered-search/). [How terms are dropped](/guide/typo-tolerant-search/#how-terms-are-dropped) is the L2 step. [Synonym and prefix search](/guide/synonym-and-prefix-search/) is L1 and L3.

## Facets

A facet is a `keyword` field with `facet: true` (SPEC §6.2.2, §7.5). `FacetRequest::new(field, size, &schema)` refuses any other field, and a size outside 1 to 1,000; in a request JSON a facet is a field name or `{"field": name, "size": n}`. The default size is 10. A request lists at most 16 facets. The same field may be listed twice.

Each facet counts the live matching documents that hold the value, once per distinct value, over every match, not only the page. The counts are exact. `SearchResponse::facets` holds one `FacetCounts` per facet asked for, in the request's order: the `size` values (`FacetValue`, its `value` bytes and `count`) with the highest count, then ascending bytes. A value no match holds is absent.

With a facet, every match is visited. Without one, a score sort may skip documents that cannot enter the page (SPEC §7.3, §7.4).

When the relaxation cascade runs, a facet counts the matches of every step up to the step whose hits the page holds, each document once, at its first step (SPEC §7.5, ROADMAP 8.1.9). So a loosened page can count matches the exact query does not have, and every page of one query on one snapshot counts the same, by offset or by cursor. A page the time budget cuts short counts the steps it completed; with `fill`, a budget that passes while the page counts its later steps leaves the counts partial, and the response does not say so yet (ROADMAP 8.1.31).

## Sort

The default is score descending (`Sort::score()`). A `Sort` has at most 16 keys. A key is the score (`SortKey::score()`), or a field that has a column (`SortKey::field(field, order)`): `keyword`, `i64`, `f64`, `datetime`, or `bool`. `text` and `stored` have no column, and `Sort::new` refuses them. `order` is `Order::Asc` or `Order::Desc`. `missing` (`SortKey::Field`'s `missing`) is `Missing::Last` (the default) or `Missing::First`, whichever way the values go. In a request JSON, `sort` is `"score"` or a list of `"score"`, `{"score": {"order": ...}}`, and `{"field", "order", "missing"}` keys; a field's order defaults to ascending.

Numbers and datetimes compare by value. `false` is before `true`. Keywords compare by bytes. A document with several keyword values sorts by its smallest ascending and by its largest descending. Every hit still carries its score, and `Hit::sort_values` its values for the keys, one per key (`SortValue`).

After the keys, ties break by segment order (the manifest's), the memtable last, then docid (SPEC §7.3). A collection-wide default tie-break would be a schema option and a new manifest version. It is not part of v1 (ROADMAP 32.5.1). How the score itself is computed, and when a later sort key can still reorder a tie, is [BM25F ranking](/guide/ranking/#sort-and-tie-break).

A relaxed search ranks by step first, then by the sort: every hit of a stricter step ranks above every hit of a looser one, whatever its sort values (SPEC §7.6). L5's fallback hits are the exception, ordered by static rank or recency, not by the sort.

When the first key is the score descending, the search skips a document that cannot beat the worst hit it keeps; a sort led by another key visits more matches. Either way the hits are the ones a full scan would keep (SPEC §7.3, §7.4).

## Pages and cursors

`offset` defaults to 0 and `limit` to 20. `offset + limit` is at most 10,000. The page is the top `offset + limit` hits by the sort, then each hit's stored fields, after the projection (request JSON `projection`: every stored field, none, or a list of names).

`Hit::cursor` is an opaque token (SPEC §7.4) for `SearchRequest::after` (`search_after` in a request JSON). `SearchResponse::next_cursor` is the last hit's. The next page starts after that hit, and `offset` still applies after it. A token records the sort, the snapshot's commit, the collection, the memtable view's version, the relaxation settings and step, and a fingerprint of the request's text, fields, filters and limits. Given to another request, another sort or other relaxation settings, or after a write moved the snapshot or the memtable view on, it is refused with `ErrorCode::InvalidArgument`: its location would repeat a tied hit or skip one there. The page size, the facets and `explain` may change between pages.

## Deadlines

A `SearchRequest` has no deadline in the embedded API yet. The engine checks for a deadline and a cancellation as it searches (SPEC §7.3), and the embedded API uses that check for its async form: an `AsyncCollection::search` whose future is dropped stops at the next check and returns nothing (feature `async`).

That is not the relaxation budget. `budget_ms` (default 50) is read after the exact level finishes, so the exact level is never cut by it (SPEC §7.6, ROADMAP 8.1.4). L1 synonyms, L2 `drop_terms`, L3 prefix, L4 fuzzy, and L5 fallback can spend the budget on a looser query; past it, the response sets `Relaxation::truncated`. [Query relaxation](/guide/typo-tolerant-search/#query-relaxation) is the budget.

## Explain

`SearchRequest::explain(true)` (`"explain": true`) asks for an explanation of each hit on the page: `Hit::explanation` is an `Explanation`, a tree of the clauses that matched that document (SPEC §7.7, ROADMAP 7.7). The formula those nodes add up is [BM25F ranking](/guide/ranking/#explain). A word (`Explanation::Word`) reports `idf`, `k1`, `tf`, and per field the frequency, norm, and contribution. A phrase reports its terms and phrase frequency. A `Bool` lists its `must` clauses and the `should` clauses that matched. Filters and excluded clauses score nothing and are left out. The root score (`Explanation::score`) is the hit's score.

A hit of a relaxed search has a root `Explanation::Relaxed` with its level, step, and that step's `applied` around the step's own tree; `Explanation::scoring` is that tree. Every hit also states its level, `Hit::level`: `exact`, `synonyms`, `drop_terms`, `prefix`, `fuzzy`, `fallback`, or `filters` (ROADMAP 8.3.4).

```rust
use persia::{Db, Explanation, RelaxLevel, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("notes.persia"))?;
    let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    notes.put_json(r#"{"id": "n1", "body": "hello world"}"#)?;
    notes.put_json(r#"{"id": "n2", "body": "goodbye"}"#)?;

    let page = notes.search(&SearchRequest::new("hello").explain(true))?;
    let hit = &page.hits[0];
    let tree = hit.explanation.as_ref().ok_or("no explanation")?;
    // A relaxed search (the default) wraps the step's tree; L0 found this hit.
    let Explanation::Relaxed(relaxed) = tree else { return Err("not a relaxed hit".into()) };
    assert_eq!((relaxed.level, relaxed.step), (RelaxLevel::Exact, 0));
    assert!(matches!(tree.scoring(), Explanation::Word(_)));
    assert_eq!(tree.score(), hit.score);

    db.close()?;
    Ok(())
}
```

Explain covers the page's hits only, in one more pass over the views that hold them, so it costs about one more unpruned search of those views.

Highlighted snippets are not this tree. Highlights are post-v1 (SPEC §7.7).

## What is still planned

| Piece | Status |
|---|---|
| Filters, facets, sort, cursor pages, explain | %%MATCHING_CELL%% |
| A deadline on `SearchRequest` | Not in the embedded API. The engine has one (SPEC §7.3); the async search's cancellation uses it. |
| Cascade levels after exact | %%RELAXATION_CELL%% |
| A flag for partial facet counts under the budget | ROADMAP 8.1.31. |
| Highlighted fragments | Post-v1. |
| A collection default for tie-break | Post-v1 (ROADMAP 32.5.1). |
| Numeric or range facets | Not in SPEC §7.5. Facets are keyword values. |

## FAQ

### Does Persia DB do faceted search today?

Yes. `Collection::search` takes facets on a `SearchRequest` (`SearchRequest::facets`), and `Collection::search_json` takes them in the request JSON. Facets count `keyword` fields marked `facet: true`, over every match, not only the page.

### Which fields can be facets?

A `keyword` field with `facet: true`. A request lists at most 16. Each returns the 1 to 1,000 most common values, 10 unless you ask otherwise. Text, numbers, and bool are not facet fields. Counts are exact and merged by the value's bytes.

### How do sort and cursor pagination work?

The default sort is score descending, then segment order with the memtable last, then docid. A sort has at most 16 keys: score, or a `keyword`, `i64`, `f64`, `datetime`, or `bool` column. A page is `offset` and `limit`, at most 10,000 together, `limit` 20 by default. A cursor is a hit's token (`Hit::cursor`, `SearchResponse::next_cursor`) passed to `SearchRequest::after`, and is valid only for the same request on the same snapshot, collection, and memtable view.

### Can a search set a deadline?

Not through the embedded API yet: a `SearchRequest` has no deadline. An `AsyncCollection::search` whose future is dropped stops at the engine's next check and returns nothing. The relaxation budget, `budget_ms`, is a different thing: it starts after the exact level and cuts only the looser levels. %%RELAXATION_CELL%%

### Does explain return highlighted snippets?

No. `explain: true` returns a per-hit score tree for the hits on the page. Highlighted fragments are post-v1. The tree's root score is the hit's score.

## Read next

- [Filtered full-text search](/guide/filtered-search/) — the filter DSL, how it joins a text query, and `last_resort`.
- [BM25F ranking](/guide/ranking/) — field weights, collection statistics, static rank, score-led pruning, and proximity off by default.
- [Typo-tolerant search](/guide/typo-tolerant-search/) — query relaxation, the empty-page rule, `minimum_should_match`, and how terms are dropped.
- [Fuzzy search and did-you-mean](/guide/fuzzy-search/) — L4 edit distance, the correction, L5 fallback, and the filter step. An explained hit names its step.
- [Synonym and prefix search](/guide/synonym-and-prefix-search/) — L1 synonym groups and L3 prefix expansion. Filters stay on both steps.
- [Feature reference](/guide/features/) — the status table, the schema sketch, and the relaxation levels.
- [Features](/features/) — the short status table.
- [Getting started](/guide/getting-started/) — open a database, add documents, and search.
- [Comparison](/guide/comparison/) — next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
- [What is an embedded search database?](/guide/embedded-search/) — in-process search, one file, and no server.
- [FAQ](/guide/faq/) — production readiness, the file format, and the license.
