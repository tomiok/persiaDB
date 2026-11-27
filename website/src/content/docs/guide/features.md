---
title: "Feature reference"
description: "Reference for Persia DB search, BM25F ranking, the zero-results cascade, schema, durability, and blobs. Each item is planned or done."
head:
  - tag: title
    content: "Reference for BM25F, schema, relaxation, blobs | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Reference for BM25F, schema, relaxation, blobs | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Reference for BM25F, schema, relaxation, blobs | PersiaDB"
---

%%FEATURES_LEAD%%
What is built, an application calls through the `persia` crate: `Db::open`, a `Collection`, and `Collection::search` (or `search_json`). [Getting started](/guide/getting-started/) is the walkthrough.

| Capability | Status | Defined in |
|---|---|---|
| Open a file and search | %%OPEN_CELL%% | SPEC §8, §10.2 |
| Query AST, user-text compiler, filter DSL, limits, BM25F | %%QUERY_CELL%% | SPEC §7.1, §7.2 |
| Matching, filters, column sort, paging, facets | %%MATCHING_CELL%% | SPEC §7.3–§7.5 |
| Zero-results relaxation | %%RELAXATION_CELL%% | SPEC §7.6 |
| Schema and documents | Done (M3): `Db::create_collection`, `Db::create_collection_json`, `Collection::put_json`, `Collection::get`. | SPEC §6 |
| Analyzers (`standard`, `keyword`, `whitespace`, `en`, `es`, `pt`, `fr`, `de`, `it`) | Done (M4). A search analyzes its user text with each field's analyzer. [Text analysis](/guide/text-analysis/). | SPEC §6.3 |
| Segment writer and reader | %%M5_CELL%% | SPEC §5 |
| Crash-safe commit and recovery | %%DURABILITY_CELL%% | SPEC §4.5, §8.6 |
| Blob storage | %%STATE_M9%% | SPEC §9 |
| gRPC and HTTP server | %%STATE_M11%% | SPEC §12 |
| S3, GCS, Azure, replication | %%STATE_M12%% | SPEC §10 |
| Kubernetes, sharding | %%STATE_M13%% | SPEC §11 |
| Embedded Go and TypeScript bindings | Planned (after M10). Design only. | SPEC §13.1, [Embedded SDKs](/guide/sdks/) |
| Rust and Go network SDKs | %%STATE_M14%% | SPEC §13.2 |
| Integer codecs and `None` / `Zstd` block codecs | %%STATE_M1%% Library primitives. | SPEC §4.1, §5.2 |

## Search and ranking

A search takes user text, a field list, filters, a sort, a page, and optional facets (SPEC §7.1).
The text is analyzed per field and compiled into a query: terms, phrases, prefixes, fuzzy clauses,
and boolean must / should / must-not. The default operator is AND. Invalid syntax degrades to plain
terms rather than failing the request (ROADMAP 7.1.2). Filters (term, range, exists, and / or / not)
are separate from the text and are not relaxed unless you opt in.

Ranking is BM25F (SPEC §7.2): each field has a weight, `k1`, and `b`, and a collection may set a
static rank (for example `log1p` of a sales field). Collection statistics (`N`, document frequency,
average field length) are summed across live segments. Scores must match the reference model
within a documented epsilon (SPEC §7.2); the engine's tests check that against a naive reference
(ROADMAP 7.3.4). `Collection::search` runs the ranked search over the collection's snapshot. The formula, the bounds, proximity, and where the static rank is applied are
[BM25F ranking](/guide/ranking/). Filters, facets, sort, cursor pages, deadlines, and explain are
[Faceted search](/guide/faceted-search/). The longer account of typos and relaxation is
[Typo-tolerant search](/guide/typo-tolerant-search/).

Over one view — a segment, or the memtable — the engine collects the top k by score, then docid,
with block-max WAND by default (MaxScore is the other strategy) (ROADMAP 7.4.1–7.4.4, SPEC §7.3).
Pruning does not change the hits: pruned and exhaustive top k are the same documents, scores bit for bit.
A segment view drops tombstoned documents in the postings iterator, before a document is scored or
offered to the collector, so a deleted document cannot raise the top-k threshold. Block bounds may
still cover deleted documents; that only prunes less (SPEC §7.3). Also in the search: filter bitmaps
(ROADMAP 7.4.5), the memtable hiding every older segment copy of an id (ROADMAP 7.4.6), searching
segments in parallel (ROADMAP 7.6.1), sort by a column and a `search_after` cursor (ROADMAP 7.5.1,
7.5.2), facets (ROADMAP 7.5.3), and `explain: true` (ROADMAP 7.7.1), all through `Collection::search`. Highlighting is post-v1.

## Zero-results relaxation

Relaxation is on by default. If a query returns fewer hits than `min_results` (default 1), the
engine walks a cascade and stops at the first level that produces enough hits (SPEC §7.6).
With `fill` set, it keeps going until the page is full. Stricter levels always rank above looser
ones. Filters stay strict unless `relax.filters` is `last_resort`. A time budget (default 50 ms)
returns the best results so far and sets `truncated`. A query can turn the whole feature off with
`relax: false`.

| Level | Name | What it changes |
|---|---|---|
| L0 | `exact` | Every analyzed term is required. |
| L1 | `synonyms` | Terms expand through the collection's synonym groups. |
| L2 | `drop_terms` | Required-term ratio steps from 100% to 75%, 50%, then one term. Least important terms are reported dropped first. |
| L3 | `prefix` | The last term (and other terms of length at least 3) expands through a prefix scan, capped (default 50). |
| L4 | `fuzzy` | Edit distance 1 for terms of length 4–7, distance 2 for length 8 or more. Corrections are reported. |
| L5 | `fallback` | Top documents by static rank, then index-location recency. The request's `sort` is ignored for that order. The response sets `fallback`. `none` builds no step. |
| L6 | `filters` | Only with `relax.filters: "last_resort"`, as the final opt-in step: L0's words without the request's `soft_filter` (the hard `filter` stays), excluded words still excluded, when every earlier level found fewer than `min_results` hits. A query that searches no word does not build it. `"filters_dropped": true` only when this step found the hits. |

L0 through L5 build a query. L5 ranks by static rank, then index-location recency, ignores the request's `sort`, and flags that order with `fallback`. With `relax.filters: "last_resort"`, one last step (L6) drops the request's `soft_filter` (never the hard `filter`) when every earlier level found fewer than `min_results` hits. Queries that search no word do not plan this step, and `filters_dropped` is true only when L6 supplies the hits (ROADMAP 8.2.6). A query's `relax` object overrides the collection defaults, including `fill` (ROADMAP 8.3.1, 8.3.2). When a rewritten term has 8 or more characters, L4 ranks a one-edit tier ahead of distance 2 (ROADMAP 8.2.7). [Fuzzy search](/guide/fuzzy-search/) is that step, the correction, and the fallback. [Synonym and prefix search](/guide/synonym-and-prefix-search/) is how L1 and L3 expand a query. [Query relaxation](/guide/typo-tolerant-search/#query-relaxation) is the orchestrator, [minimum_should_match](/guide/typo-tolerant-search/#minimum_should_match) is the ladder, and [how terms are dropped](/guide/typo-tolerant-search/#how-terms-are-dropped) is the importance order.

The response says what happened. This JSON is the shape in SPEC §7.6. The engine produces a correction when L4 is the step that found the hits (ROADMAP 8.2.4.3, 8.1.22):

```json
"relaxation": {
  "level": "fuzzy",
  "applied": ["drop_terms:75%", "fuzzy"],
  "original": "zapatilas rojs",
  "effective": "zapatillas rojas",
  "corrected": { "zapatilas": "zapatillas", "rojs": "rojas" }
}
```

Quality of this cascade is a product feature of the same milestone (ROADMAP M8): a synthetic typo
suite, a zero-result rate before and after relaxation, and a regression gate. A tuning guide is
planned as ROADMAP 8.4.4 (`docs/relaxation.md`). %%RELAXATION_NOW%% The longer page is
[Typo-tolerant search](/guide/typo-tolerant-search/).

## Schema, documents, and analysis

A collection declares its schema up front (SPEC §6.2). Field types are `text`, `keyword`, `i64`,
`f64`, `bool`, `datetime` (microseconds UTC), and `stored` (kept, not searchable). Every document
has an `id` of at most 512 bytes, unique in the collection, and may reference one blob in v1
(several blobs per document is an open question, SPEC §18.2).

Adding a field is allowed; existing documents read that field as missing. Changing a field's type
or analyzer is rejected and needs a new collection version (reindex). Unknown JSON fields are
rejected when `strict` is true, and kept as stored-only fields when it is false (SPEC §6.4).
Documents are submitted as JSON, or as typed structs in the SDKs, and stored as CBOR.

The schema of SPEC §6.2, as `Db::create_collection_json` takes it:

```json
{
  "name": "products",
  "fields": {
    "title": { "type": "text", "analyzer": "es", "boost": 3.0, "positions": true, "k1": 1.2, "b": 0.75 },
    "body":  { "type": "text", "analyzer": "es", "boost": 1.0 },
    "brand": { "type": "keyword", "facet": true },
    "price": { "type": "f64" },
    "sales": { "type": "i64" }
  },
  "ranking": { "static_rank": { "field": "sales", "weight": 0.1, "fn": "log1p" } },
  "relaxation": { "enabled": true, "min_results": 1, "levels": "default", "fallback": "static_rank" },
  "synonyms": [["tv", "television"]]
}
```

Analyzers are a pipeline: character filters, then a tokenizer, then token filters (SPEC §6.3).
Built-ins implemented (ROADMAP 4.4.2 and 4.4.10) are `standard` (Unicode word boundaries, NFKC,
lowercase), `keyword`, `whitespace`, and language analyzers `en`, `es`, `pt`, `fr`, `de`, and `it`
(stopwords, a Snowball-style stemmer, accent folding). Positions, used for phrases, are opt-in per field
(SPEC §18.4). The analyzer's name and version are stored in the schema so a later change is visible.

A `text` field names its analyzer in the schema. What each built-in emits, and how a search analyzes its user text, is [Text analysis](/guide/text-analysis/).

## Durability

Writes are `Collection::put` (upsert by id), `delete`, `put_batch`, `get`, and `commit`, plus an auto-commit policy
(SPEC §8.1). The path (SPEC §8.2):

1. The operation is appended to the WAL. Durability of a call is `Durability::Buffered`, `Fsync`, or `Async`
   (`OpenOptions::durability`, `Collection::with_durability`). The default is `Fsync`.
2. The operation is applied to the memtable and becomes searchable.
3. When the memtable crosses a size or idle threshold (defaults 64 MiB and 10 seconds), it is flushed
   to a new immutable segment.
4. A commit publishes a new manifest.
5. WAL that is fully covered by committed segments is dropped.

Local commit order is data frames, `fdatasync`, the manifest frame, `fdatasync`, then the inactive
superblock slot, `fdatasync` (SPEC §4.5). A crash before the superblock write leaves the previous
commit in place. Recovery reloads that manifest and replays the valid WAL prefix (SPEC §8.6).

There is a single writer per database or shard (a file lock locally, a lease remotely). Any number
of readers can be open; each reader sees one manifest snapshot (SPEC §8.4). Merges are tiered,
run in the background, and publish through the same commit protocol (SPEC §8.5). Dead space in the
local file is reclaimed by rewriting live data into a new file and atomically replacing the old one
(SPEC §4.6).

The numbered steps above are the path in SPEC §8.2. %%WRITE_PATH_NOW%% [Durability and crash recovery](/guide/durability/) is that path: the WAL levels, what a reopen replays, the writer lock, and `persia verify`.

## Blobs

The index stores a reference, never the file bytes (SPEC §9). A `blob_id` is the SHA-256 of the
blob (ADR-0002). Persia hashes the bytes it receives; a caller-supplied id that does not match is
an error. Chunks default to 4 MiB, each with a CRC32C. Reads can request a byte range.
Blob bytes are stored as the client sent them (SPEC §9.2). The same content stored twice is one blob.

Write order is deliberate: the blob is durable first, then the document that references it is
logged (SPEC §9.5). A crash in between leaves an orphan. Garbage collection keeps anything a live
snapshot, the WAL, or a pin still references. Remote orphans wait out a grace period (default 24
hours). The local pack is rewritten with no grace period once enough of it is unreferenced.
`persia verify` fails if a referenced blob is missing. `Db::put_blob` stores a file and `Collection::put_with_blob` a file with its document; `PutBlobOptions::resume` takes the `ResumeToken` of an interrupted upload; the local pack cannot resume (`ErrorCode::Unsupported`), and a remote store is not open through `Db` yet (M12).

%%BLOB_NOW%% `BlobId` and `BlobRef` are re-exported under `persia::`: their limits and binary encoding are in SPEC §9.1 and ADR-0011.
[Blob storage](/guide/blobs/) is the pack file, the remote layout, resume, reader leases, and garbage collection.
