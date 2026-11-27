---
title: "Persia DB vs SQLite FTS5, Tantivy, and Meilisearch"
description: "Persia DB versus SQLite FTS5, Tantivy, and Meilisearch: embedded or server, single file or segments, language, and maturity. Pre-alpha: an embedded Rust API on one file, unpublished."
---

> **Status: pre-alpha.** %%PRE_ALPHA%%
> The embedded API (`persia::Db::open`, `Collection::search`) runs as a Git dependency; the design contract is
> [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md).
> Live status is [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md).

This page places the Persia DB design next to [SQLite FTS5](https://sqlite.org/fts5.html), [Tantivy](https://github.com/quickwit-oss/tantivy), and [Meilisearch](https://www.meilisearch.com/docs).
It is not a benchmark. It does not say that any of them is faster.
Facts about those projects are the stable ones in their own documentation.
Anything Persia DB does not implement yet is marked planned.
When this page and the SPEC disagree, the SPEC wins.

## At a glance

| | Persia DB | SQLite FTS5 | Tantivy | Meilisearch |
|---|---|---|---|---|
| What it is | Embedded search database, written in Rust. An optional server is planned. | Full-text virtual-table module inside SQLite, a C library. | Full-text search library, written in Rust. Its docs describe it as Lucene, in Rust. | Standalone search server, written in Rust. |
| How you run it | In-process. `Db::open` on a file. No daemon for the library. | In-process. Link SQLite and query with SQL. | In-process. Link the crate. Indexing and serving are your code. | A separate process. Clients use an HTTP API. |
| What you store | A `<name>.persia` container and an optional `<name>.persia.blobs` pack for files. | The FTS5 index lives in the SQLite database file. | A directory of segment files. | The server's own data directory. |
| How you query | `Collection::search` with a `SearchRequest` or a request JSON: BM25F, filters, facets, and relaxation on by default. | SQL `MATCH`, with a `bm25()` ranking function. | Queries against a schema declared in code. | JSON over HTTP. Typo tolerance is on by default. |
| Object storage | Planned (M12): the same segment bytes locally or in S3, GCS, or Azure Blob. | A local database file. | A directory the library writes. | You run the server. |
| Today | %%PRE_ALPHA%% | Part of SQLite. | A released library. | A released server. |

## Persia DB

The intended product is an embedded, single-file, cloud-native search database.
Think "SQLite for search": open a file, index documents, and get ranked results that, by default,
never come back empty. That is the design in [SPEC §1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#1-goals-and-non-goals)
and on the [overview](/guide/overview/). The local half of it runs today, pre-alpha; object storage and the server are not built.
[What is an embedded search database?](/guide/embedded-search/) defines that in-process, single-file shape,
and separates it from running a search server.

%%LIBRARY_NOW%%
The API an application calls is the `persia` crate (`Db`, `Collection`, `SearchRequest`), a Git dependency. %%ENGINE_CALLS%%
[Getting started](/guide/getting-started/) is the walkthrough. There is no crates.io release yet. The format stays unstable until `v1.0` (M15).
Counts on the [roadmap](/roadmap/) are read from PROGRESS.md when the site is built.

What the design specifies, and what runs today:

- The query compiler, the filter DSL, and a BM25F scorer run behind `Collection::search` (ROADMAP 7.1–7.3).
  %%MATCHING_CELL%%
  How a hit is scored is [BM25F ranking](/guide/ranking/).
  Analysis libraries are in the tree. The project does not wrap Tantivy, Lucene, or SQLite
  ([ADR-0001](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0001-own-engine-and-allowed-dependencies.md)).
  Filters, facets, sort, and paging are [Faceted search](/guide/faceted-search/).
- A query with too few hits is relaxed step by step, and the response says what changed (SPEC §7.6).
  %%RELAXATION_CELL%% See [Typo-tolerant search](/guide/typo-tolerant-search/) and [Features](/features/).
- Segments are immutable. A local commit and an object-storage commit publish the same segment bytes.
  Query nodes are stateless. See [How it works](/architecture/) and [File format and storage](/guide/file-format/).
- A local database is two files: `<name>.persia` and `<name>.persia.blobs` (SPEC §4, §9).
- Out of scope for v1: SQL, joins, vector search, more than one writer on a shard, and encryption inside the file (SPEC §1.2).

## SQLite FTS5

[FTS5](https://sqlite.org/fts5.html) is an SQLite virtual table module for full-text search.
You create a table with `CREATE VIRTUAL TABLE … USING fts5(…)` and search it with SQL `MATCH`.
The built-in `bm25()` function ranks matches. The index is stored in that same SQLite database file:
FTS5 keeps it in shadow tables, which applications do not query directly.

SQLite is a C library. The database and the full-text index run in your process.
There is no search server to deploy. The rest of the application's rows can live in the same file.

FTS5 is not an object-storage index, and it is not a Rust API.
The Persia DB design aims at the same "open a file" step, then specifies a search database
rather than a SQL engine: no SQL, immutable segments, and a planned path that stores those
segment bytes in object storage. Object storage for those bytes is not implemented (M12).

## Tantivy

[Tantivy](https://github.com/quickwit-oss/tantivy) is a Rust library for full-text search.
The project describes itself as Lucene, in Rust. You declare a schema, create an index in a
directory, add documents, commit, and run queries you compose. The index is a directory of
segments, not one database file. An HTTP API or a cluster is something you build, or another
project; Tantivy itself is the library.

The language and the embedded shape are the nearest match to Persia DB.
The design then diverges. The local container is one `.persia` file (SPEC §4).
The same segment bytes are specified for object storage (SPEC §10).
Relaxation is specified to be on by default (SPEC §7.6).
The engine is not Tantivy: ADR-0001 forbids depending on another search engine.
Tantivy is a released library. Persia DB is pre-alpha, with no compatibility promise.

## Meilisearch

[Meilisearch](https://www.meilisearch.com/docs) is a search server, written in Rust.
You run it as its own process and send documents and queries over HTTP.
Typo tolerance is on unless you disable it. Filtering, faceted search, and ranking rules
are part of that server. Data lives in the server's directory, not in a file the application opens.

The shared idea is a search API that tolerates a small mistake in the query, and an implementation in Rust.
The mechanisms differ. Meilisearch's default is typo tolerance on the terms it searches.
Persia DB's specified default is a cascade: synonyms, dropped terms, prefix, a fuzzy step, then a fallback,
with the response naming the step (SPEC §7.6). %%RELAXATION_CELL%%
[Typo-tolerant search](/guide/typo-tolerant-search/) is which of those levels run, and how a multi-term query drops terms.
[Fuzzy search and did-you-mean](/guide/fuzzy-search/) is the L4 step, the correction, and the L5 fallback. Filter relaxation runs only with the `last_resort` opt-in, and drops only the `soft_filter`, never the hard `filter` (ROADMAP 8.2.6).
[Synonym and prefix search](/guide/synonym-and-prefix-search/) is the L1 and L3 steps.
[BM25F ranking](/guide/ranking/) is how a hit is scored. [Faceted search](/guide/faceted-search/) is filters, facets, sort, and paging, on the same `SearchRequest`.
The deployment is different as well. Meilisearch is a service you operate.
Persia DB is specified as a library you open on a file, with a server only as a later option
([Server, cloud, and Kubernetes](/guide/server/), M11).
Meilisearch is a released server. The Persia DB server does not exist.

## Lucene and Elasticsearch

[Apache Lucene](https://lucene.apache.org/) is the Java library Tantivy is modeled on:
an embedded index of segments in a directory.
[Elasticsearch](https://www.elastic.co/elasticsearch) is a distributed server built on Lucene.
Running it means operating a cluster, which is a different job from linking a library or opening one file.
The planned Persia DB server would embed the same engine as the library, and segment bytes
would stay the same in object storage. That server is not written.

## When to pick which today

For a search feature in production today, use one of the established tools. Persia DB has a working embedded API (`Db::open`, `Collection::search`), but it is pre-alpha: unpublished, with an unstable format and no compatibility promise.

- **SQLite FTS5**, when the data is already in SQLite, or you want SQL and full-text in one database file and one process.
- **Tantivy**, when you are writing Rust, you want a library, and you will own the schema, the index directory, and how results are served.
- **Meilisearch**, when you want a separate search service, an HTTP API, and typo tolerance without building the engine.
- **Persia DB**, when you want to try an embedded Rust search API on one local file, with relaxation on by default, and can follow a pre-alpha [roadmap](/roadmap/). Not when an application needs a stable release. The object-storage half of the SPEC (the same segment bytes in S3, GCS, or Azure Blob) is not built.

## Read next

- [Typo-tolerant search](/guide/typo-tolerant-search/) — query relaxation, zero results, and `minimum_should_match`.
- [Fuzzy search and did-you-mean](/guide/fuzzy-search/) — L4 edit distance, the correction returned to the caller, and the L5 fallback.
- [Synonym and prefix search](/guide/synonym-and-prefix-search/) — L1 synonym groups and L3 prefix expansion.
- [BM25F ranking](/guide/ranking/) — field weights, collection statistics, static rank, and proximity off by default.
- [Faceted search](/guide/faceted-search/) — facets, sort, cursor pages, deadlines, and explain.
- [What is an embedded search database?](/guide/embedded-search/) — in-process full-text search, one file, and no server.
- [Overview](/guide/overview/) — what the project is, and what already exists.
- [FAQ](/guide/faq/) — short answers on status, the file format, crash safety, SDKs, and the license.
- [Getting started](/guide/getting-started/) — add the crate, open a file, index, and search.
- [Features](/features/) and the [feature reference](/guide/features/) — each capability marked planned or done.
- [How it works](/architecture/) and [file format and storage](/guide/file-format/) — segments, frames, and backends.
- [Roadmap](/roadmap/) — milestone status. The live counter is PROGRESS.md.
