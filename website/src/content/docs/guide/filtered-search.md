---
title: "Filtered full-text search"
description: "How Persia DB filters full-text search in Rust: term, terms, range, and exists restrict hits without changing the BM25F score. A hard filter is never dropped; a soft filter may be, as a last resort you opt into."
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
            "name": "How do I filter a full-text search in Persia DB?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Give the search request a filter. SearchRequest::filter_json takes the filter's JSON, with fields by name. SearchRequest::filter takes a typed persia::Filter. The filter key of a request JSON passed to Collection::search_json is the third form. All three select the same documents, and the search returns the ones that pass, scored by the text."
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
              "text": "No. A filter selects documents and adds 0 to the score. A document that matches the text and the filter scores as the same text query without the filter. Explain leaves filters out of the score tree."
            }
          },
          {
            "@type": "Question",
            "name": "Can a filter be dropped when a search has too few hits?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The hard filter, never. The soft filter, only when the collection or the request sets relax.filters to last_resort. The default is never. Then one last step, L6, drops the soft filter when every earlier level found fewer than min_results hits and the query searches a word. filters_dropped is true only when that step found the hits. Put tenancy and permission filters in the hard filter."
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

A filtered full-text search here is a text query plus a structured predicate. The words are analyzed and scored with BM25F. The filter decides which documents are hits. It does not change the score ([SPEC §7.1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#71-query-model), [§7.1.2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#712-filters), [§7.2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#72-scoring-bm25f)).

A search runs on a collection of an open database: `Db::open`, then `Db::collection` or `Db::create_collection_json`, then `Collection::search` with a `SearchRequest`, or `Collection::search_json` with a request JSON. A request holds two filters, each optional:

- `filter`, the **hard** filter. No hit of any relaxation level breaks it. Tenancy and permission filters go here.
- `soft_filter`, the **soft** filter. It applies like `filter`, unless the search opts into the last-resort step, which may drop it (see [Relaxation](#relaxation)).

Each comes in three forms that select the same documents:

| Form | Hard | Soft |
|---|---|---|
| The filter's JSON, fields by name | `SearchRequest::filter_json` | `SearchRequest::soft_filter_json` |
| A typed `persia::Filter`, fields by id | `SearchRequest::filter` | `SearchRequest::soft_filter` |
| A key of the request JSON | `"filter"` in `Collection::search_json` | `"soft_filter"` |

The JSON forms are parsed when the search runs, by the same parser, against the schema of the collection searched and the database's query limits. So a filter given to `filter_json` and the same filter inside a request JSON give the same response ([SPEC §7.1.4](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#714-request-json)). A request takes one hard and one soft filter: of `filter` and `filter_json`, the later call is the one kept.

## Search with a filter

The sample opens a database in a temporary directory (the `tempfile` crate); any path works.

```rust
use persia::{Db, Filter, FilterValue, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("shop.persia"))?;
    let products = db.create_collection_json(
        r#"{"name": "products", "fields": {
            "title": {"type": "text"},
            "brand": {"type": "keyword", "stored": true},
            "price": {"type": "i64", "stored": true}
        }}"#,
    )?;
    products.put_json(r#"{"id": "p1", "title": "red running shoes", "brand": "nike", "price": 80}"#)?;
    products.put_json(r#"{"id": "p2", "title": "red running shoes", "brand": "nike", "price": 140}"#)?;
    products.put_json(r#"{"id": "p3", "title": "red rain coat", "brand": "acme", "price": 60}"#)?;

    // The filter's JSON: nike, at most 100.
    let filter = r#"{"and": [{"term": ["brand", "nike"]}, {"range": ["price", {"lte": 100}]}]}"#;
    let page = products.search(&SearchRequest::new("red shoes").filter_json(filter))?;
    let ids: Vec<&str> = page.hits.iter().map(|h| h.document.id()).collect();
    assert_eq!(ids, ["p1"]);

    // The same filter inside a request JSON.
    let json = format!(r#"{{"q": "red shoes", "filter": {filter}}}"#);
    let same = products.search_json(&json)?;
    assert_eq!(same.to_json()?, page.to_json()?);

    // The same filter built in code: fields by id, from the schema.
    let schema = products.schema()?;
    let brand = schema.field("brand").map(|f| f.id()).ok_or("no brand field")?;
    let price = schema.field("price").map(|f| f.id()).ok_or("no price field")?;
    let typed = Filter::And(vec![
        Filter::Term { field: brand, value: FilterValue::Keyword("nike".into()) },
        Filter::Range {
            field: price,
            lower: std::ops::Bound::Unbounded,
            upper: std::ops::Bound::Included(FilterValue::I64(100)),
        },
    ]);
    let built = products.search(&SearchRequest::new("red shoes").filter(typed))?;
    assert_eq!(built.hits[0].document.id(), "p1");
    assert_eq!(built.hits[0].score, page.hits[0].score);

    db.close()?;
    Ok(())
}
```

## Operators

A filter is one JSON object with exactly one key, the operator. Keys are never repeated. An unknown operator, an unknown field, a value of the wrong type, and filter JSON longer than 1 MiB are errors. A bad filter is an error. User text is not: `q` never fails, and bad syntax in it is read as plain terms ([SPEC §7.1.1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#711-user-text)).

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

`text` is not a `term` or `range` field. A word limited to one field is query syntax, `field:word` inside `q`, and that clause scores. A filter `term` on a `keyword` does not.

Only `text` and `keyword` hold more than one value (SPEC §6.4). A `term` matches when one of those values is the value. `i64`, `f64`, `bool`, and `datetime` hold one value each. A document with no value in the field fails `term`, `terms`, `range`, and `exists`.

## Values

A value uses the field's JSON form (SPEC §6.4.1) and must be a valid value of that field.

| Field | JSON | Rule |
|---|---|---|
| `keyword` | string | At most 32,768 bytes. Matched as written. |
| `i64` | number | An integer in the `i64` range. A fraction or an exponent is an error. |
| `f64` | number | `-0.0` is 0. |
| `bool` | `true` or `false` | |
| `datetime` | number or string | Microseconds, or an RFC 3339 string, in `0000-01-01T00:00:00Z` through `9999-12-31T23:59:59.999999Z`. |

```json
{"and": [{"term": ["brand", "nike"]}, {"range": ["price", {"lte": 100}]}]}
```

## Errors and limits

A filter counts against the query limits (SPEC §7.1.3). Each value, including each `terms` entry and each range bound, is a leaf term. Each node is a clause. The limits are 1,024 terms, 1,024 clauses, and depth 32, together with the query the filter joins. The embedded API uses these defaults and does not change them.

A search reports a bad filter by `ErrorCode`, the same for every form:

- Text that is not a filter (malformed JSON, a repeated key, an unknown operator or field, a value of another type, more than 1 MiB): `ErrorCode::InvalidArgument`.
- A filter past a limit: `ErrorCode::Limit`.

```rust
use persia::{Db, ErrorCode, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("shop.persia"))?;
    let products = db.create_collection_json(
        r#"{"name": "products", "fields": {"title": {"type": "text"}, "brand": {"type": "keyword"}}}"#,
    )?;
    products.put_json(r#"{"id": "p1", "title": "red shoes", "brand": "nike"}"#)?;

    // A field the schema does not have.
    let unknown = SearchRequest::new("shoes").filter_json(r#"{"term": ["colour", "red"]}"#);
    let err = products.search(&unknown).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument);

    // 1,025 values: past the 1,024 terms a query may hold.
    let values: Vec<String> = (0..1025).map(|i| format!(r#""brand{i}""#)).collect();
    let past = format!(r#"{{"terms": ["brand", [{}]]}}"#, values.join(", "));
    let err = products.search(&SearchRequest::new("shoes").filter_json(past)).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Limit);

    db.close()?;
    Ok(())
}
```

## How a filter joins the text

`q` compiles to a query and never fails (SPEC §7.1.1). The request's filters join it as the filter clauses of its root `bool`, so a document is a hit when it matches the text and every filter (ROADMAP 8.1.17). A query that is not such a `bool` is wrapped: `bool { must: [query], filter }`. Every relaxation level keeps that join: a level that rewrites the words still carries the filters.

`q` with no words (`""`) matches every document, so a request of `""` and a filter lists the documents the filter selects.

## Scoring

`must` and `should` clauses score. `filter` and `must_not` add 0 (SPEC §7.2). A document that matches the text and the filter receives the score of that text query alone. The proximity bonus, off unless a request without relaxation sets `proximity`, is not applied to a filter. [BM25F ranking](/guide/ranking/#clauses) is the formula and the bonus.

`SearchRequest::explain(true)` leaves filters and `must_not` out of each hit's score tree. The root score is still the hit's score. [Faceted search](/guide/faceted-search/#explain) is that tree.

## Evaluation

Before a document is scored, the search turns the filters into a set of the live documents they select, per segment and per in-memory view (ROADMAP 7.4.5). The matcher stays inside that set. Deleted documents are not in it.

A `keyword` `term` or `terms` reads that field's postings. A `term` or `terms` on `i64`, `f64`, `datetime`, or `bool`, and every `range`, reads the field's column of order-preserving keys, and a `range` skips the blocks whose keys cannot match (ROADMAP 7.4.13). `exists` reads which documents have a value. `not` is the complement among the view's documents. A selective text query may check a column filter per candidate instead of building the whole set (ROADMAP 7.4.13, 7.4.16).

## Facets

A facet counts live matching documents, over every match, not only the page. The filters have already restricted that set, so the counts are the values those hits hold (SPEC §7.5). Keyword facets, sizes, and sort are [Faceted search](/guide/faceted-search/#facets).

## Relaxation

Relaxation is on by default: a query with too few hits is loosened one level at a time ([Typo-tolerant search](/guide/typo-tolerant-search/)). Every level keeps both filters. The hard `filter` is never dropped (ROADMAP 8.2.13). The soft `soft_filter` is dropped only by the last-resort step, L6 `filters`, and only when the search opts in with `filters: "last_resort"`: in the collection's `relaxation` settings, or in the request's `relax` (SPEC §7.6, ROADMAP 8.2.6). The default is `"never"`.

L6 comes after every other enabled level. It is L0's query without the soft filter; excluded words stay excluded, and the hard filter stays. It is built only when every earlier level found fewer than `min_results` hits (default 1), when the request has a soft filter, and when the query searches a word: empty text, stopwords only, or exclusions alone build none. `fill` does not build it to pad a page. When L6 found the hits, the response's `relaxation.level` is `filters`, `Relaxation::filters_dropped()` is true, and each of those hits has level `filters`. They do not pass the soft filter; they all pass the hard one.

```rust
use persia::{Db, RelaxLevel, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("shop.persia"))?;
    // This collection lets the last resort drop a soft filter.
    let products = db.create_collection_json(
        r#"{"name": "products",
            "fields": {
                "title": {"type": "text"},
                "brand": {"type": "keyword"},
                "price": {"type": "i64"}
            },
            "relaxation": {"filters": "last_resort"}}"#,
    )?;
    products.put_json(r#"{"id": "p1", "title": "red running shoes", "brand": "nike", "price": 80}"#)?;
    products.put_json(r#"{"id": "p2", "title": "red running shoes", "brand": "acme", "price": 60}"#)?;

    // Nothing of nike is under 50: the soft filter is dropped, the hard one is kept.
    let request = SearchRequest::new("running shoes")
        .filter_json(r#"{"term": ["brand", "nike"]}"#)
        .soft_filter_json(r#"{"range": ["price", {"lte": 50}]}"#);
    let page = products.search(&request)?;
    let ids: Vec<&str> = page.hits.iter().map(|h| h.document.id()).collect();
    assert_eq!(ids, ["p1"]);
    assert!(page.relaxation.filters_dropped());
    assert_eq!(page.hits[0].level, RelaxLevel::Filters);

    // `relax: false` runs the exact query alone: no hit.
    let exact = products.search_json(
        r#"{"q": "running shoes", "filter": {"term": ["brand", "nike"]},
            "soft_filter": {"range": ["price", {"lte": 50}]}, "relax": false}"#,
    )?;
    assert!(exact.hits.is_empty());

    db.close()?;
    Ok(())
}
```

## What is still planned

| Piece | Status |
|---|---|
| Geo bounding box, circle, and distance sort | Not implemented (M29). |
| A document access filter the request cannot turn off | Not implemented (M30). |
| Numeric or range facets | Not in SPEC §7.5. Facets are keyword values. |
| A parser that returns a typed `Filter` from JSON, to combine with one built in code | Not in the API: write the whole filter in one form (SPEC §7.1.4). |

## FAQ

### How do I filter a full-text search in Persia DB?

Give the search request a filter. `SearchRequest::filter_json` takes the filter's JSON, with fields by name. `SearchRequest::filter` takes a typed `persia::Filter`. The `filter` key of a request JSON passed to `Collection::search_json` is the third form. All three select the same documents, and the search returns the ones that pass, scored by the text.

### Which fields can a filter use?

`term` and `terms` take `keyword`, `i64`, `f64`, `bool`, and `datetime`. `range` takes `i64`, `f64`, and `datetime`, with `gt`, `gte`, `lt`, or `lte`. `exists` takes any field except `stored`. A keyword is matched as written, not analyzed. A `text` field is not a `term` or `range` filter. A bad filter is an error.

### Do filters change the BM25F score?

No. A filter selects documents and adds 0 to the score. A document that matches the text and the filter scores as the same text query without the filter. Explain leaves filters out of the score tree.

### Can a filter be dropped when a search has too few hits?

The hard `filter`, never. The `soft_filter`, only when the collection or the request sets `relax.filters` to `"last_resort"`. The default is `"never"`. Then one last step, L6, drops the soft filter when every earlier level found fewer than `min_results` hits and the query searches a word. `filters_dropped` is true only when that step found the hits. Put tenancy and permission filters in the hard filter.

## Read next

- [Faceted search](/guide/faceted-search/) — keyword facets, sort, cursor pages, and explain. Facets count the matches the filters left.
- [BM25F ranking](/guide/ranking/) — field weights, collection statistics, and why a filter adds nothing to the score.
- [Typo-tolerant search](/guide/typo-tolerant-search/) — the relaxation cascade, and how each level keeps the filters.
- [Fuzzy search and did-you-mean](/guide/fuzzy-search/) — L4, L5 fallback, and the last-resort filter step.
- [Synonym and prefix search](/guide/synonym-and-prefix-search/) — L1 and L3. Both keep the filters.
- [Getting started](/guide/getting-started/) — open a database, add documents, and search.
- [FAQ](/guide/faq/) — production readiness, the file format, and the license.
