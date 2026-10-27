---
title: "Persia DB vs SQLite FTS5, Tantivy, and Meilisearch"
description: "Persia DB versus SQLite FTS5, Tantivy, and Meilisearch: embedded or server, single file or segments, language, and maturity. Pre-alpha; it does not search yet."
---

> **Status: pre-alpha.** Persia DB does not open a database, index a document, or answer a query.
> `Db::open` and `search` elsewhere on this site are illustrative, from
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
| How you run it | Planned: in-process. `Db::open` on a file. No daemon for the library. | In-process. Link SQLite and query with SQL. | In-process. Link the crate. Indexing and serving are your code. | A separate process. Clients use an HTTP API. |
| What you store | Planned: one `<name>.persia` file, plus an optional `<name>.persia.blobs` pack. | The FTS5 index lives in the SQLite database file. | A directory of segment files. | The server's own data directory. |
| How you query | Planned: a collection API, BM25F, filters, and facets. | SQL `MATCH`, with a `bm25()` ranking function. | Queries against a schema declared in code. | JSON over HTTP. Typo tolerance is on by default. |
| Object storage | Planned: the same segment bytes locally or in S3, GCS, or Azure Blob. | A local database file. | A directory the library writes. | You run the server. |
| Today | Pre-alpha. Codecs, local storage, schema types, text analysis, and a segment writer exist as library code. It does not search. | Part of SQLite. | A released library. | A released server. |

## Persia DB

The intended product is an embedded, single-file, cloud-native search database.
Think "SQLite for search": open a file, index documents, and get ranked results that, by default,
never come back empty. That is the design in [SPEC §1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#1-goals-and-non-goals)
and on the [overview](/guide/overview/). It is not what the crate does today.
[What is an embedded search database?](/guide/embedded-search/) defines that in-process, single-file shape,
and separates it from running a search server.

What the repository implements today is on-disk codecs, local storage, schema and document types, text analysis, and a segment writer and reader.
There is no `Db::open` and no query. Building one segment is not a database.
The [getting started](/guide/getting-started/) page separates that library code from the illustrative API.
The first release a caller can use is `v0.1` (roadmap M10). The format stays unstable until `v1.0` (M15).
Counts on the [roadmap](/roadmap/) are read from PROGRESS.md when the site is built.

Specified, and not something you can search with yet:

- BM25F scoring and query execution are not implemented.
  Analysis libraries are in the tree. The project does not wrap Tantivy, Lucene, or SQLite
  ([ADR-0001](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0001-own-engine-and-allowed-dependencies.md)).
- A query with too few hits is relaxed step by step, and the response says what changed (SPEC §7.6).
  See [Features](/features/).
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
segment bytes in object storage. That search path is not implemented.

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
Tantivy is a library you can use today. Persia DB is not.

## Meilisearch

[Meilisearch](https://www.meilisearch.com/docs) is a search server, written in Rust.
You run it as its own process and send documents and queries over HTTP.
Typo tolerance is on unless you disable it. Filtering, faceted search, and ranking rules
are part of that server. Data lives in the server's directory, not in a file the application opens.

The shared idea is a search API that tolerates a small mistake in the query, and an implementation in Rust.
The mechanisms differ. Meilisearch's default is typo tolerance on the terms it searches.
Persia DB's planned default is a cascade: synonyms, dropped terms, prefix, a fuzzy step, then a fallback,
with the response naming the step (SPEC §7.6). That cascade is not implemented.
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

For a search feature in production today, use one of the established tools. Persia DB cannot answer a query.

- **SQLite FTS5**, when the data is already in SQLite, or you want SQL and full-text in one database file and one process.
- **Tantivy**, when you are writing Rust, you want a library, and you will own the schema, the index directory, and how results are served.
- **Meilisearch**, when you want a separate search service, an HTTP API, and typo tolerance without building the engine.
- **Persia DB**, when you are reading the design or following the [roadmap](/roadmap/). Not when an application needs search. The combination in the SPEC — an embedded Rust API, one local file, the same segment bytes in object storage, and relaxation on by default — is specified. It is not shipped.

## Read next

- [What is an embedded search database?](/guide/embedded-search/) — in-process full-text search, one file, and no server, and what this repository implements.
- [Overview](/guide/overview/) — what the project is, and what already exists.
- [FAQ](/guide/faq/) — short answers on status, the file format, crash safety, SDKs, and the license.
- [Getting started](/guide/getting-started/) — codecs you can call, and the API that is still illustrative.
- [Features](/features/) and the [feature reference](/guide/features/) — each capability marked planned or done.
- [How it works](/architecture/) and [file format and storage](/guide/file-format/) — segments, frames, and backends.
- [Roadmap](/roadmap/) — milestone status. The live counter is PROGRESS.md.
