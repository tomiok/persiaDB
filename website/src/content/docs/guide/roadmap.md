---
title: "Milestones and dependencies"
description: "What each Persia DB milestone from M0 to v1.0 delivers, which milestones it depends on, and where the live status lives."
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

## M0 — Project bootstrap

Done. The Cargo workspace and stub crates, the pinned toolchain, CI (fmt, clippy, tests, `cargo deny`,
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

Depends on M0. Its state is the table above. `persia-analysis` implements Unicode and whitespace
tokenizers, NFKC, lowercasing, accent folding, English, Spanish, and Portuguese stopword lists and
stemmers, synonyms, edge n-grams, and the named analyzer pipelines (`standard`, `keyword`,
`whitespace`, `en`, `es`, `pt`). [`PROGRESS.md`](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md)
lists the leaves, including the ones still open. Segment writing (M5) waits on this milestone.

## M5 — Segment writer and reader

Depends on M1, M3, and M4. The in-memory index builder and every section writer and reader: term
dictionary, postings, positions, norms, columns, stored documents, ids, blob references, and the
footer. Includes `verify`, inspect and dump commands, property tests against a model, golden
segments, and fuzz targets. This is the heart of the index format. Query (M7) can start from a
reader; the full write path (M6) also needs storage.

## M6 — Write path, durability, merging

Depends on M2 and M5. The memtable, the WAL and its replay, flush and the local commit protocol,
upserts and deletes via tombstone sidecars, tiered compaction, and open/recovery. Crash-loop tests
(kill the process and reopen) land here. The embedded API waits on this milestone.

## M7 — Query engine and ranking

Depends on M5. End-to-end tests also need M6. The query AST and a parser that does not fail on
messy user text, collection statistics, BM25F checked against hand-computed examples and the
reference scorer, boolean and phrase matching, top-k with block-max WAND, filters, sort,
pagination, facets, parallel segment search, and explain. Benchmarks of this milestone may start
the v1 performance work early (M15).

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
([Embedded SDKs](/guide/sdks/)), blocked on M10, with no roadmap leaves yet.

## M15 — Benchmarks, hardening, v1.0

Depends on M1 through M14 as a release, with room to benchmark after M7. A reproducible harness and
a regression gate, SIMD and other work driven by profiles, soak and chaos tests, a security review,
and the format 1.0 freeze: open questions closed as ADRs, golden fixtures for every released
version, a documentation site (embedded, server, cloud, Kubernetes, relaxation tuning, operations),
and the `v1.0.0` release. This guide is source material for that later site (ROADMAP 15.5.4).
