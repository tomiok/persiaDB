---
title: "What is an embedded search database?"
description: "What an embedded search database is: in-process full-text search, one file, and no server. When to choose it, and how Persia DB's Rust design approaches it."
head:
  - tag: title
    content: "What is an embedded search database? Embedded full-text search in Rust"
  - tag: meta
    attrs:
      property: og:title
      content: "What is an embedded search database? Embedded full-text search in Rust"
---

> **Status: pre-alpha.** Persia DB does not open a database, index a document, or answer a query.
> `Db::open` and `search` elsewhere on this site are illustrative, from
> [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md).
> Counts below are read from [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) when the site is built:
> %%PROGRESS_SUMMARY%%. That file is the live counter.
> When this page and the SPEC disagree, the SPEC wins.

An **embedded search database** is a search library that runs in the same process as the application. You link it in. Queries are function calls in that process. There is no search daemon to install, and no separate server process that has to be up before a query can run.

Some embedded libraries keep the index in one file, the way SQLite does. Others keep a directory of segment files. Persia DB's design is the single-file kind. A local database is `<name>.persia`, plus an optional `<name>.persia.blobs` pack ([SPEC §4](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#4-local-file-container), [§9](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#9-blobs)). The planned call is `Db::open("app.persia")`, with no config file for the library ([SPEC §14](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#14-configuration)). That call is not in the crate.

**Embedded full-text search in Rust** is that in-process shape, as a Rust crate. [Tantivy](https://github.com/quickwit-oss/tantivy) is a released library of that kind, and its index is a directory. Persia DB is a pre-alpha project aimed at one file, an engine written in this repository, and the same segment bytes in object storage. The design line is "SQLite for search": open a file, index documents, and get ranked results that, by default, never come back empty ([SPEC §1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#1-goals-and-non-goals)). The repository does not search yet.

A search server is a different way to run search. You start it as its own process, send documents and queries over the network, and operate that service. [Meilisearch](https://www.meilisearch.com/docs) and [Elasticsearch](https://www.elastic.co/elasticsearch) are servers. [SQLite FTS5](https://sqlite.org/fts5.html) and [Tantivy](https://github.com/quickwit-oss/tantivy) are libraries. Where Persia DB sits next to SQLite FTS5, Tantivy, and Meilisearch is the [comparison](/guide/comparison/). That page is not a benchmark.

## When an embedded library fits

The choice is about where the index lives and who operates it.

An embedded search database fits when search belongs in the same program as the data:

- The program is one process (a CLI, a desktop tool, a single service, a test) and can open a file or a directory.
- The same binary that writes documents runs the query.
- For the single-file kind, the database you copy is that file. In the Persia DB design it is `<name>.persia`, plus `<name>.persia.blobs` when blobs are used (SPEC §4, §9).
- You do not want a search cluster for that data.

A search server fits when many applications share one index, when callers should use HTTP without linking a library, or when a team already runs that service.

The Persia DB design is the library, with a server only as a later option. The planned gRPC and HTTP server embeds the same engine as the library ([SPEC §3](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#3-architecture-overview), [§12](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#12-server-and-api)). That server is milestone M11. `persia-server` is an empty `main` function. See [Server, cloud, and Kubernetes](/guide/server/).

For a search feature in production today, use an engine that already answers queries. The [comparison](/guide/comparison/) says which established tool fits which job. Persia DB cannot answer a query.

## How the Persia DB design approaches it

The design contract is [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) (draft v0.1). The table is that design. The right-hand column is what the repository contains.

| Design choice | Specified in | In the repository |
|---|---|---|
| In-process library. No daemon and no config file for the embedded API. Options are builders, with defaults. | [SPEC §14](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#14-configuration) | Planned (M10). The `persia` crate exports no `Db` and no `Collection`. |
| One local file, `<name>.persia`, plus an optional `<name>.persia.blobs` pack for client bytes such as PDF or images. | SPEC §4, §9 | The container format and the single-file container (open, append, commit, recover, compact) are implemented (M1, M2). Segment sections — the term dictionary, postings, stored documents — have a writer and a reader in `persia-engine` (M5). The leaf count is on the [roadmap](/roadmap/). Nothing searches the segment. The blob store has not been started (M9). `BlobId` and `BlobRef` exist as types in `persia-format`. |
| The engine is written in this repository: analysis, BM25F, filters, facets, and query execution. | SPEC §6, §7. [ADR-0001](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0001-own-engine-and-allowed-dependencies.md) forbids depending on Tantivy, Lucene, or SQLite. | Tokenizers and built-in analyzers are in `persia-analysis`. A segment writer and reader exist. BM25F, filters, facets, and query execution are not implemented. Nothing runs a query. |
| A query with too few hits is relaxed step by step (synonyms, dropped terms, prefix, a fuzzy step, then a fallback). The response names the step. On by default. | [SPEC §7.6](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#76-zero-results-avoidance-default-on) | Planned (M8). Ranking and relaxation settings exist as schema config structs. The cascade does not run. |
| Segments are immutable. The same segment bytes live in a local file or in S3, GCS, or Azure Blob. Query nodes are stateless. | SPEC §3, §5, §10, §11 | A segment writer and reader exist (M5). Memory and local-file storage exist (M2). Object-storage backends and Kubernetes are planned (M12, M13). |
| A power loss leaves the previous commit or the new one. | [SPEC §4.5](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#45-commit-protocol-local), §8 | The container commit is implemented and crash-tested (M2). There is no document write path: no memtable, no engine WAL replay, and no index commit (M6). A searchable document cannot be stored and recovered yet. |
| Optional gRPC and HTTP server. Embedded mode has no authentication. | SPEC §12, §14 | Planned (M11). |
| Embedded Go and TypeScript bindings open the same file in-process. Rust and Go network clients talk to the server. | SPEC §13, [ADR-0018](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0018-embedded-sdk-bindings.md) | Design only. No Go module and no npm package. Network clients are milestone M14. |

The first release a caller is supposed to use is `v0.1` (roadmap M10). That milestone is blocked on the write path, query, relaxation, and blobs. The on-disk format stays unstable until `v1.0` (M15, [SPEC §15](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#15-versioning-and-compatibility)). The crates are unpublished (`publish = false`, version 0.0.0). There is no crates.io release.

[SPEC §16.2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#16-quality-requirements) lists latency and throughput figures and labels them as targets, not promises. This project does not publish search benchmarks. The query engine is not built.

Out of scope for v1 ([SPEC §1.2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#12-non-goals-v1)): text extraction, OCR, and transcription (clients supply text; blob bytes stay opaque); vector or semantic search; SQL, joins, multi-document transactions, and cross-collection queries; more than one writer on a shard; encryption at rest inside the file.

## What the code can do today

[PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) is the live counter: %%PROGRESS_SUMMARY%%. Milestone states are on the [roadmap](/roadmap/) page, generated from that file. What you can call is library code: `persia-format` (on-disk codecs, including the streaming frame encoder), `persia-storage` (the `Storage` trait, memory and local-file backends, and the single-file container), `persia-engine` (collection schema, documents, manifest types, and a segment builder, writer, and reader), and `persia-analysis` (tokenizers and built-in analyzers; it does not search). `persia-blob` exports no API. The document write path, query, and relaxation are not implemented. Building one segment is not `Db::open`.

A git dependency can use:

- `persia-format`, which encodes and decodes on-disk bytes: integer codecs, checksums, `None` and `Zstd` block compression, block framing, the container header, the superblock, and frames.
- `persia-storage`, which implements the `Storage` trait, a memory backend, a local-file backend, and the single-file container.
- `persia-engine` schema types, which validate a collection schema, and `SegmentBuilder` / `SegmentWriter` / `Segment`, which build and read one segment. They do not search.

The `persia` command lists and removes temporary and quarantine files next to a container or a local store (`persia container files`, `persia container sweep`, `persia store files`, `persia store sweep`). It can also inspect and dump one segment file (`persia inspect segment`, `persia dump segment`, with `--verify` on inspect). It does not import documents or search. There is no `Db::open`. The intended API, marked as illustrative, is the [embedded API](/guide/embedded-api/) page. The file layout is [File format and storage](/guide/file-format/).

## Read next

- [Comparison](/guide/comparison/) — Persia DB next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
- [FAQ](/guide/faq/) — short answers on status, the file format, crash safety, SDKs, and the license.
- [How it works](/architecture/) — crate layers, and the planned write path and query path.
- [Roadmap](/roadmap/) — milestone status. The live counter is PROGRESS.md. The longer dependency notes are [Milestones](/guide/roadmap/).
- [Overview](/guide/overview/) — the project narrative, including goals and non-goals.
- [Crate architecture](/guide/architecture/) — how the Rust crates are layered.
