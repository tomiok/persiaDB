---
title: "Milestones and dependencies"
description: "What each Persia DB milestone from M0 to v1.0 delivers, which milestones it depends on, and the post-MVP milestones through v3.0."
head:
  - tag: title
    content: "Milestones M0-M15 and later releases to v3.0 | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Milestones M0-M15 and later releases to v3.0 | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Milestones M0-M15 and later releases to v3.0 | PersiaDB"
---

Work is a tree: milestone, then section, then leaf. Every leaf is one issue and one pull request.
Status lives on the leaves only (`[ ]` todo, `[~]` in progress, `[x]` done). [`PROGRESS.md`](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md)
derives the rest, and it is the live status. The table below is that file, read when the site is built:
%%PROGRESS_SUMMARY%%.

A milestone is **done** when every leaf is done, **in progress** when some leaves are done,
**ready** when its dependencies are done and none of its own leaves have started, and **blocked**
while a dependency is unfinished. The paragraphs under each heading say what the milestone is for.
The Snapshot column is the status.

%%MILESTONE_TABLE%%

The generated ready-next list is in [`PROGRESS.md`](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md).
That percentage is M0–M15 only. Milestones after v1.0 are at the end of this page.

## M0 — Project bootstrap

Done. The Cargo workspace and its crates, the pinned toolchain, CI (fmt, clippy, tests, `cargo deny`,
Miri on `persia-format`), the `justfile`, repo tooling in Rust (`cargo xtask`, ADR-0003), issue
templates, and the documentation scaffolding: this repository's rules, the ADR process, ADR-0001 and
ADR-0002, and [`docs/architecture.md`](https://github.com/tomiok/persiaDB/blob/main/docs/architecture.md). No dependencies.

## M1 — Format primitives

Done. Depends on M0. Pure functions in `persia-format`, no I/O, which is why the
bar for corruption handling is high: hostile bytes return an error.

It covers little-endian cursors, varints, zigzag, checked offsets, integer codecs, CRC32C, xxh3,
checksummed structs, the `None` / `Zstd` codecs, block framing, the container header and
double-buffered superblock, frames (including the streaming encoder), the memory-map reader,
golden container fixtures, and `BlobId` / `BlobRef`. The leaf list is
[`ROADMAP.md`](https://github.com/tomiok/persiaDB/blob/main/ROADMAP.md).

## M2 — Storage abstraction and local backends

Done. Depends on M1. The `Storage` trait and its error types, an in-memory backend, a deterministic
failure injector for crash tests, a local object-per-file backend, and the single-file container
adapter: create, append, the three-step commit, recovery, and compaction, each with crash tests.
Also the first docker-compose file for later cloud emulators.

## M3 — Schema, documents, collections

Done. Depends on M1. Field types and collection schema (including a stable schema hash and evolution
rules), document validation, JSON and CBOR, and the manifest model. Segment writing (M5) needs this and M4.

## M4 — Text analysis

Done. Depends on M0. `persia-analysis` implements Unicode and whitespace
tokenizers, NFKC, lowercasing, accent folding, English, Spanish, Portuguese, French, German, and Italian stopword lists and
stemmers, synonyms, edge n-grams, and the named analyzer pipelines (`standard`, `keyword`,
`whitespace`, `en`, `es`, `pt`, `fr`, `de`, `it`). Segment writing (M5) needs this milestone.
Its state is the table above.

## M5 — Segment writer and reader

Depends on M1, M3, and M4. Its state is the table above. The in-memory index builder and every section writer and reader: term
dictionary, postings, positions, norms, columns, stored documents, ids, blob references, and the
footer. Includes `verify`, inspect and dump commands, property tests against a model, golden
segments, and fuzz targets. This is the heart of the index format. Query (M7) can start from a
reader; the full write path (M6) also needs storage.

## M6 — Write path, durability, merging

Depends on M2 and M5. The memtable, the WAL and its replay, flush and the local commit protocol,
upserts and deletes via tombstone sidecars, tiered compaction, and open/recovery. Crash-loop tests
(kill the process and reopen) land here. The embedded API waits on this milestone.
Its state is the table above.

%%WRITE_PATH_NOW%%

## M7 — Query engine and ranking

Depends on M5. End-to-end tests also need M6. The query AST and a parser that does not fail on
messy user text, collection statistics, BM25F checked against hand-computed examples and the
reference scorer, boolean and phrase matching, top-k with block-max WAND, filters, sort,
pagination, facets, parallel segment search, and explain. Benchmarks of this milestone may start
the v1 performance work early (M15).

In the tree today: the query AST and compiler, the filter DSL, query limits, collection statistics,
the BM25F scorer (ROADMAP 7.1–7.3), and matching with score top-k over one view, including block-max
WAND (ROADMAP 7.4.1–7.4.4). Filter evaluation, facets, sort, parallel segment search, and explain are not.
[Typo-tolerant search](/guide/typo-tolerant-search/) is that split. The relaxation milestone below has not been started.

## M8 — Zero-results avoidance

Depends on M7. The cascade in [Feature reference](/guide/features/): orchestration, per-level score offsets, the
time budget, synonyms, dropping terms, prefix, fuzzy (Levenshtein automaton over the term
dictionary), the fallback, and the response metadata. Plus a synthetic typo suite and a regression
gate on the zero-result rate. The embedded `v0.1` release depends on this milestone (M10 depends on M8).

## M9 — Blob store

Depends on M1 and M2. An ADR for upload staging first (SPEC §18.8), then hashing and chunking,
the local pack file, the remote key layout, ordering a blob write before the document commit, and
garbage collection that never deletes a blob a live snapshot still references.

## M10 — Embedded API

Depends on M6, M7, M8, and M9. The first user-facing release. Public `Db` and `Collection`,
concurrency and shutdown semantics, rustdoc and examples, a README quickstart that CI actually
runs, the `persia` CLI, and a `v0.1.0-alpha` tag that states the format is unstable.
See [Embedded API](/guide/embedded-api/).

## M11 — Server

Depends on M10. `persia.proto`, committed generated code, the gRPC service, the HTTP/JSON gateway
and its OpenAPI spec, configuration, health checks, metrics and tracing, authentication and TLS,
and the container image. See [Server, cloud, and Kubernetes](/guide/server/).

## M12 — Cloud storage backends

Depends on M2, M6, M9, and M11. S3, GCS, and Azure through a shared async layer, the remote commit
protocol, a block cache, replicated mode, and conformance tests against MinIO, fake-gcs-server,
and Azurite. Documentation of IAM and request cost is part of the milestone.

## M13 — Distribution and Kubernetes

Depends on M11 and M12. Writer leases and fencing, reader refresh, hash sharding and the router,
role flags and discovery, the Helm chart, and a `kind` end-to-end test (ingest, query, kill the
writer, fail over, roll). Resharding is a design record only for v1, with an optional prototype
behind a flag.

## M14 — SDKs

Depends on M11 for a stable protocol, and on M13 for shard-aware routing. A language-neutral
conformance suite, then the Rust client and the Go client, then a compatibility matrix and release
automation. These are network clients. In-process Go and TypeScript bindings are a separate design
([Embedded SDKs](/guide/sdks/)). The checkboxes are M16 (target v1.1), blocked on v1.0.

## M15 — Benchmarks, hardening, v1.0

Depends on M1 through M14 as a release, with room to benchmark after M7. A reproducible harness and
a regression gate, SIMD and other work driven by profiles, soak and chaos tests, a security review,
and the format 1.0 freeze: open questions closed as ADRs, golden fixtures for every released
version, a documentation site (embedded, server, cloud, Kubernetes, relaxation tuning, operations),
and the `v1.0.0` release. This guide is source material for that later site (ROADMAP 15.5.4).

M0–M15 are the MVP. The live percentage in [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) counts only those leaves. Milestones after M15 are post-MVP and are not in the snapshot table above.

## After v1.0

| Target | Milestones | Depends on |
|---|---|---|
| v1.1 | **M16** Embedded SDKs, **M17** Highlighting and snippets, **M18** Autocomplete, **M19** Import, **M31** Persia Studio | M15, plus the feature's own deps |
| v1.2 | **M20** Encryption at rest, **M21** Python SDK | M20 on storage (M2, M12) and v1.0 (M15); M21 on the network clients (M14) and the C ABI (M16) |
| v2.0 | **M22** Multi-tenant, **M23** Persia Cloud, **M24** CDC connectors, **M25** Web dashboard | M22 before M23; M23 also needs Kubernetes (M13) and encryption (M20) |
| v2.1 | **M26** UI components | Server HTTP, highlighting, autocomplete |
| v2.2 | **M27** AI search | Format 1.0, blobs, snippets |
| v2.3 | **M28** On-device | The C ABI, and a storage backend without mmap |
| v3.0 | **M29** Geo, **M30** Enterprise | M29 needs query (M7) and the format freeze (M15); M30 needs Kubernetes (M13), encryption (M20), tenancy (M22), and the dashboard (M25) |

Multi-tenant is numbered before Persia Cloud because the hosted service is one file per tenant. v2.1, v2.2, and v2.3 are separate targets so a UI kit does not wait on embeddings, and embeddings do not wait on phones. Geo has no dependency on those three; it stays on v3.0 with the enterprise format work. The leaf list is [ROADMAP.md](https://github.com/tomiok/persiaDB/blob/main/ROADMAP.md).

## M16 — Embedded SDKs (v1.1)

Blocked on M15. Go via cgo and `@persiadb/node` via N-API, both through `persia-ffi`, as [ADR-0018](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0018-embedded-sdk-bindings.md) and [`docs/sdk/`](https://github.com/tomiok/persiaDB/blob/main/docs/sdk/README.md) already specify. This page does not repeat that design. The network clients stay M14. The browser build is M28.

## M17 — Highlighting and snippets (v1.1)

Blocked on M7 and M15. SPEC §7.7 already plans `explain` inside M7. Highlights are the post-v1 half: fragments sliced from the stored field, off unless the query asks.

## M18 — Autocomplete (v1.1)

Blocked on analysis (M4), the prefix walk (M8), the embedded API (M10), and v1.0. A `suggest` call returns completions from edge n-grams or from the term dictionary. It is not a search, and it does not run relaxation.

## M19 — Import (v1.1)

Blocked on the CLI (M10) and v1.0. `persia import` gains Elasticsearch, Meilisearch, and a SQLite FTS5 dump. JSONL import is already leaf 10.4.2. The SQLite path does not link `rusqlite`.

## M20 — Encryption at rest (v1.2)

Blocked on local and cloud storage and on v1.0. SPEC §1.2 leaves encryption to the disk for v1. This milestone encrypts bytes Persia writes, behind an incompatible format flag, including objects in the bucket. The key is not in the file.

## M21 — Python SDK (v1.2)

Blocked on the C ABI (M16) and on the network-client conformance suite (M14). The embedded package calls `persia.h`. A smaller HTTP client covers programs that will not load a native library. Neither one reimplements the engine.

## M22 — Multi-tenant mode (v2.0)

Blocked on cloud storage and v1.0. One `.persia` file per tenant, backups to S3, and a server route that cannot see another tenant's file. Per-document principals are M30.

## M23 — Persia Cloud (v2.0)

Blocked on Kubernetes (M13), encryption (M20), and multi-tenant files (M22). A hosted control plane with a free tier. Local reads stay local. The cloud project is the source of truth after sync. The operator UI is the M25 dashboard, not a second app.

## M24 — CDC connectors (v2.0)

Blocked on the server and v1.0. A separate process tails Postgres, MySQL, or Supabase (Supabase is the Postgres connector) and writes through the public API. Nothing in the query path talks to the database.

## M25 — Web dashboard (v2.0)

Blocked on the server and v1.0. An index explorer, a query playground, editors for boosts, synonyms, and pin/hide rules, and a list of queries that stayed empty. Auth is the server's API keys.

## M26 — UI components (v2.1)

Blocked on the HTTP API, highlighting, and autocomplete. Search box, suggestions, hits, and facets for React, Vue, and Svelte, sharing one HTTP client. These packages do not embed the engine.

## M27 — AI search (v2.2)

Blocked on blobs, v1.0, and snippets. A vector field (SPEC §18.6), hybrid ranking, an embedding model shipped in the repo, a read-only MCP server, and retrieval of passages from one file. Writing the answer stays outside the process.

## M28 — On-device (v2.3)

Blocked on the storage trait and the C ABI. iOS and Android link `libpersia`. WASM waits on a storage backend with no mmap and no POSIX fsync, which ADR-0018 already requires and which this milestone builds.

## M29 — Geo filters and sorting (v3.0)

Blocked on query and v1.0. A `geo_point` field, a bounding box, a circle, and sort by distance. Nothing here depends on M26–M28. The filters are not relaxed.

## M30 — Enterprise (v3.0)

Blocked on Kubernetes (M13), encryption (M20), multi-tenant mode (M22), and the dashboard (M25). Per-document allow and deny rules, an audit log, OpenID Connect login, and an on-premises bundle that boots without calling Persia Cloud.

## M31 — Persia Studio (v1.1)

Blocked on the TypeScript SDK (M16). A read-only client that runs in the browser and does not talk to a server. The core is the WASM build that SDK work shares. The user picks the `database.persia/` directory with the File System Access API, which Chromium-based browsers provide. Firefox and Safari do not, and this version does not add a server for them. The page browses segments and the manifest, shows documents and tombstones, runs queries with the BM25 score breakdown, and shows WAL state. Write mode and a local `persia studio` server are out of scope. It is the seed of the v2.0 dashboard (M25).
