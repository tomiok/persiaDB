---
title: "Overview"
description: "What Persia DB is: an embedded, single-file search database written in Rust, what already works in pre-alpha, and why it is built this way."
---

> **Status: pre-alpha.** Nothing works end to end yet, and the on-disk format is unstable until v1.0.
> There is no compatibility promise. Counts on the [milestones page](/guide/roadmap/) are the 2026-10-10 snapshot
> in [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md): **69 of 406 leaves done (17%)** on 2026-10-11.

[SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) is the design contract (draft v0.1, format version 1.0, unstable).
When these pages and the SPEC disagree, the SPEC wins.
[docs/architecture.md](https://github.com/tomiok/persiaDB/blob/main/docs/architecture.md) is the map of the code for contributors.

Persia DB is an embedded, single-file, cloud-native search database, written in Rust.
Think "SQLite for search": open a file, index documents, and get ranked results that, by default,
never come back empty. The same segment bytes can live in a local file or in object storage
(S3, GCS, Azure Blob) and can be served by stateless query nodes on Kubernetes.

Where that design sits next to SQLite FTS5, Tantivy, and Meilisearch is
[Comparison](/guide/comparison/). That page is not a benchmark. Persia DB does not search yet.

A local database is two files (SPEC §4, §9):

- `<name>.persia` — the index container (header and checksummed frames).
- `<name>.persia.blobs` — a content-addressed pack of client files (PDF, images, video), referenced from documents.

The engine (index format, analysis, scoring, query execution) is written in this repository.
[ADR-0001](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0001-own-engine-and-allowed-dependencies.md) records that decision and the closed list of
allowed libraries.

## What works today

The workspace exists, CI runs, and `persia-format` implements the byte-level primitives that later
layers will build on (ROADMAP M0, and most of M1):

- Bounds-checked little-endian readers and writers, canonical LEB128 varints, zigzag signed integers,
  and checked offset and length types (SPEC §4.1).
- Integer codecs used inside future segment sections: delta coding, 128-value bit-packing, postings
  blocks, and frame-of-reference blocks for numeric columns (SPEC §5.2.1, [ADR-0004](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0004-integer-block-codecs.md)).
- CRC32C, xxh3-64/128, and a helper for CRC-protected fixed-size structures (SPEC §4.1–§4.3).
- Block compression codecs `None` and `Zstd`, with a decompression-bomb guard on every decode
  (SPEC §5.2, [ADR-0005](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0005-block-compression-codecs.md), ROADMAP 1.4).
- Block framing of segment sections: per-block CRC and codec, and a block table that locates every
  block, so one block can be fetched and decoded on its own (SPEC §5.2, [ADR-0006](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0006-block-framing.md), ROADMAP 1.5).
- The container header (magic, versions, feature flags, page size) and the two superblock slots, with
  the rules for picking the active slot (SPEC §4.2–§4.3, [ADR-0007](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0007-container-header-and-superblock.md), ROADMAP 1.6).
- Frames: encode, decode, and scan, including a zstd payload flag and the classes of scan end
  (SPEC §4.4, [ADR-0009](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0009-frame-validation-and-scan-end.md), ROADMAP 1.7.1–1.7.4).

Those pieces are library code. They do not open a database, index a document, or answer a query.
The streaming frame encoder (1.7.5), segments, storage, the write path, and search are still ahead (see [Milestones](/guide/roadmap/)).

The first release a caller can use, `v0.1`, is ROADMAP M10. The format freezes at `v1.0` (M15).

## Why

Five properties the design does not trade away:

1. **Zero friction.** `Db::open("app.persia")` just works. No daemon and no config file for the embedded
   library. Server configuration, when it exists, is flags, then `PERSIA_*` environment variables, then
   an optional TOML file, with production-safe defaults (SPEC §14). Planned: M10, M11.
2. **Own engine.** Index format, analysis, BM25F scoring, and query execution live in this repository,
   behind a small allowed-dependency list (ADR-0001).
3. **Never zero results by default.** A query that matches nothing is relaxed step by step, and the
   response says exactly what changed (SPEC §7.6). Planned: M8. See [Feature reference](/guide/features/).
4. **Cloud-native.** Segments are immutable. A local commit and an object-storage commit publish the
   same segment bytes. Query nodes are stateless. Planned: M12, M13.
5. **Correctness, then speed.** Checksums, crash-safe commits, and fuzzed parsers come before
   performance claims. SPEC §16.2 lists benchmark targets and labels them as targets, not promises.
   Scalar codec baselines already recorded live in [`docs/benchmarks.md`](https://github.com/tomiok/persiaDB/blob/main/docs/benchmarks.md).

## Goals and non-goals

Goals (SPEC §1.1): embedded search with an optional server; BM25F plus filters, facets, and
zero-results avoidance; the same segment bytes locally or in object storage; horizontal scale by
shard; content-addressed blobs; the embedded Rust API, embedded Go and TypeScript bindings, and Rust and Go network clients; a power loss at any point leaves committed data intact.

Out of scope for v1 (SPEC §1.2):

- Text extraction, OCR, and transcription. Clients supply text; blob bytes stay opaque.
- Vector or semantic search. The format reserves a feature flag for a later layout (SPEC §18.6).
- SQL, joins, multi-document transactions, and cross-collection queries.
- More than one writer on the same shard. A lease enforces a single writer (SPEC §11.3).
- Encryption at rest inside the file format. That stays with the disk or the storage service.

The types that already exist are quoted on the [Code](/guide/code/) page.
