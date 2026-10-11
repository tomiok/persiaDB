---
title: "Roadmap"
description: "What each milestone delivers, what it depends on, and where to read the live status."
---

Work is a tree: milestone, then section, then leaf. Every leaf is one issue and one pull request.
Status lives on the leaves only (`[ ]` todo, `[~]` in progress, `[x]` done). [`PROGRESS.md`](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md)
derives the rest, and it is the live status. The table below is the **2026-10-11** snapshot
(69 / 406 leaves, 17%). No leaf is currently marked `[~]`.

A milestone is **done** when every leaf is done, **in progress** when some leaves are done,
**ready** when its dependencies are done and none of its own leaves have started, and **blocked**
while a dependency is unfinished.

| Milestone | Delivers | Depends on | Snapshot |
|---|---|---|---|
| **M0** Bootstrap | Workspace, CI, tooling, docs scaffolding | — | Done, 26/26 |
| **M1** Format primitives | Encodings, codecs, header, frames | M0 | In progress, 43/53 |
| **M2** Storage | `Storage`, memory, local file, container | M1 | Blocked, 0/25 |
| **M3** Schema | Collections, documents, manifests | M1 | Blocked, 0/15 |
| **M4** Text analysis | Tokenizers, stemmers, analyzers | M0 | Ready, 0/22 |
| **M5** Segments | Segment writer and reader | M1, M3, M4 | Blocked, 0/36 |
| **M6** Durability | WAL, commit, merge, recovery | M2, M5 | Blocked, 0/29 |
| **M7** Query | Parser, BM25F, top-k, facets | M5 (M6 for end-to-end) | Blocked, 0/29 |
| **M8** Relaxation | The zero-results cascade | M7 | Blocked, 0/23 |
| **M9** Blobs | Pack file, remote blobs, GC | M1, M2 | Blocked, 0/19 |
| **M10** Embedded API | `Db` / `Collection`, CLI, `v0.1` | M6, M7, M8, M9 | Blocked, 0/17 |
| **M11** Server | gRPC, HTTP, image | M10 | Blocked, 0/22 |
| **M12** Cloud | S3, GCS, Azure, cache, replicate | M2, M6, M9, M11 | Blocked, 0/28 |
| **M13** Kubernetes | Lease, shards, router, Helm | M11, M12 | Blocked, 0/24 |
| **M14** SDKs | Rust and Go clients | M11; M13 for shard routing | Blocked, 0/17 |
| **M15** v1.0 | Benchmarks, hardening, format freeze | M1–M14 (benchmarks may start after M7) | Blocked, 0/21 |

Block framing (1.5), the container header and superblock (1.6), and frame encode, decode, and scan (1.7.1–1.7.4) are done.
The next format leaves are the streaming frame encoder (1.7.5), then mmap (1.8).
Text analysis (M4) may proceed alongside them.
The generated ready-next list is in [`PROGRESS.md`](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md).

## M0 — Project bootstrap

Done. The Cargo workspace and stub crates, the pinned toolchain, CI (fmt, clippy, tests, `cargo deny`,
Miri on `persia-format`), the `justfile`, repo tooling in Rust (`cargo xtask`, ADR-0003), issue
templates, and the documentation scaffolding: this repository's rules, the ADR process, ADR-0001 and
ADR-0002, and [`docs/architecture.md`](https://github.com/tomiok/persiaDB/blob/main/docs/architecture.md). No dependencies.

## M1 — Format primitives

In progress (43 of 53). Depends on M0. Pure functions in `persia-format`, no I/O, which is why the
bar for corruption handling is high: hostile bytes return an error.

Done: little-endian cursors, varints, zigzag, checked offsets, delta coding, bit-packing, postings
blocks, frame-of-reference, CRC32C, xxh3, checksummed structs, the `None` / `Zstd` codecs with the
bomb guard and a separate out-of-memory error (1.4), block framing with a block table that opens from
a section's tail (1.5), and the container header and double-buffered superblock with slot selection
(1.6; ADR-0007, ADR-0008).

Not started: frames and the frame scanner (1.7), the memory-map reader and its safety ADR (1.8, the
only planned `unsafe`), golden container fixtures (1.9), `BlobId` / `BlobRef` (1.10), and CI jobs
for fuzz, benchmark history, and coverage (1.11). M2 and M3 wait on this milestone.

## M2 — Storage abstraction and local backends

Blocked on M1. The `Storage` trait and its error types, an in-memory backend, a deterministic
failure injector for crash tests, a local object-per-file backend, and the single-file container
adapter: create, append, the three-step commit, recovery, and compaction, each with crash tests.
Also the first docker-compose file for later cloud emulators. After M1 this can proceed in parallel
with schema and analysis.

## M3 — Schema, documents, collections

Blocked on M1. Field types and collection schema (including a stable schema hash and evolution
rules), document validation, JSON and CBOR, and the manifest model. Can proceed in parallel with
M2. Segment writing (M5) needs this and M4.

## M4 — Text analysis

Ready: its only dependency, M0, is done. Not started (0 of 22). Unicode and whitespace tokenizers,
NFKC, lowercasing, accent folding, English, Spanish, and Portuguese stopword lists and stemmers,
synonyms and edge n-grams for later relaxation, and the named analyzer pipelines. Fuzzing arbitrary
UTF-8 is part of the milestone. This is the one milestone that can start before M1 finishes.

## M5 — Segment writer and reader

Blocked on M1, M3, and M4. The in-memory index builder and every section writer and reader: term
dictionary, postings, positions, norms, columns, stored documents, ids, blob references, and the
footer. Includes `verify`, inspect and dump commands, property tests against a model, golden
segments, and fuzz targets. This is the heart of the index format. Query (M7) can start from a
reader; the full write path (M6) also needs storage.

## M6 — Write path, durability, merging

Blocked on M2 and M5. The memtable, the WAL and its replay, flush and the local commit protocol,
upserts and deletes via tombstone sidecars, tiered compaction, and open/recovery. Crash-loop tests
(kill the process and reopen) land here. The embedded API waits on this milestone.

## M7 — Query engine and ranking

Blocked on M5. End-to-end tests also need M6. The query AST and a parser that does not fail on
messy user text, collection statistics, BM25F checked against hand-computed examples and the
reference scorer, boolean and phrase matching, top-k with block-max WAND, filters, sort,
pagination, facets, parallel segment search, and explain. Benchmarks of this milestone may start
the v1 performance work early (M15).

## M8 — Zero-results avoidance

Blocked on M7. The cascade in [Features](/guide/features/): orchestration, per-level score offsets, the
time budget, synonyms, dropping terms, prefix, fuzzy (Levenshtein automaton over the term
dictionary), the fallback, and the response metadata. Plus a synthetic typo suite and a regression
gate on the zero-result rate. The embedded `v0.1` release depends on this milestone (M10 depends on M8).

## M9 — Blob store

Blocked on M1 and M2. An ADR for upload staging first (SPEC §18.8), then hashing and chunking,
the local pack file, the remote key layout, ordering a blob write before the document commit, and
garbage collection that never deletes a blob a live snapshot still references.

## M10 — Embedded API

Blocked on M6, M7, M8, and M9. The first user-facing release. Public `Db` and `Collection`,
concurrency and shutdown semantics, rustdoc and examples, a README quickstart that CI actually
runs, the `persia` CLI, and a `v0.1.0-alpha` tag that states the format is unstable.
See [Embedded API](/guide/embedded-api/).

## M11 — Server

Blocked on M10. `persia.proto`, committed generated code, the gRPC service, the HTTP/JSON gateway
and its OpenAPI spec, configuration, health checks, metrics and tracing, authentication and TLS,
and the container image. See [Server, cloud, and Kubernetes](/guide/server/).

## M12 — Cloud storage backends

Blocked on M2, M6, M9, and M11. S3, GCS, and Azure through a shared async layer, the remote commit
protocol, a block cache, replicated mode, and conformance tests against MinIO, fake-gcs-server,
and Azurite. Documentation of IAM and request cost is part of the milestone.

## M13 — Distribution and Kubernetes

Blocked on M11 and M12. Writer leases and fencing, reader refresh, hash sharding and the router,
role flags and discovery, the Helm chart, and a `kind` end-to-end test (ingest, query, kill the
writer, fail over, roll). Resharding is a design record only for v1, with an optional prototype
behind a flag.

## M14 — SDKs

Blocked on M11 for a stable protocol, and on M13 for shard-aware routing. A language-neutral
conformance suite, then the Rust client and the Go client, then a compatibility matrix and release
automation.

## M15 — Benchmarks, hardening, v1.0

Blocked on M1 through M14 as a release, with room to benchmark after M7. A reproducible harness and
a regression gate, SIMD and other work driven by profiles, soak and chaos tests, a security review,
and the format 1.0 freeze: open questions closed as ADRs, golden fixtures for every released
version, a documentation site (embedded, server, cloud, Kubernetes, relaxation tuning, operations),
and the `v1.0.0` release. This guide is source material for that later site (ROADMAP 15.5.4).
