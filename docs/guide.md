# Persia DB

> **Status: pre-alpha.** Nothing works end to end yet, and the on-disk format is unstable until v1.0.
> There is no compatibility promise (SPEC §15). Counts in [Roadmap](#roadmap) are the 2026-10-10 snapshot
> in [`PROGRESS.md`](../PROGRESS.md): **69 of 406 leaves done (17%)** on 2026-10-11. That file is regenerated from
> [`ROADMAP.md`](../ROADMAP.md) and is the live status.

This guide is the narrative source for the project website. Each second-level heading is a page.
It describes the product as specified, and it marks every feature **done** or **planned**.

[`SPEC.md`](../SPEC.md) is the design contract (draft v0.1, format version 1.0, unstable).
When this guide and the SPEC disagree, the SPEC wins and this guide is a bug.
[`docs/architecture.md`](./architecture.md) is the map of the code for contributors.

## Overview

Persia DB is an embedded, single-file, cloud-native search database, written in Rust.
Think "SQLite for search": open a file, index documents, and get ranked results that, by default,
never come back empty. The same segment bytes can live in a local file or in object storage
(S3, GCS, Azure Blob) and can be served by stateless query nodes on Kubernetes.

A local database is two files (SPEC §4, §9):

- `<name>.persia` — the index container (header and checksummed frames).
- `<name>.persia.blobs` — a content-addressed pack of client files (PDF, images, video), referenced from documents.

The engine (index format, analysis, scoring, query execution) is written in this repository.
[ADR-0001](./adr/0001-own-engine-and-allowed-dependencies.md) records that decision and the closed list of
allowed libraries.

### What works today

The workspace exists, CI runs, and `persia-format` implements the byte-level primitives that later
layers will build on (ROADMAP M0, and most of M1):

- Bounds-checked little-endian readers and writers, canonical LEB128 varints, zigzag signed integers,
  and checked offset and length types (SPEC §4.1).
- Integer codecs used inside future segment sections: delta coding, 128-value bit-packing, postings
  blocks, and frame-of-reference blocks for numeric columns (SPEC §5.2.1, [ADR-0004](./adr/0004-integer-block-codecs.md)).
- CRC32C, xxh3-64/128, and a helper for CRC-protected fixed-size structures (SPEC §4.1–§4.3).
- Block compression codecs `None` and `Zstd`, with a decompression-bomb guard on every decode
  (SPEC §5.2, [ADR-0005](./adr/0005-block-compression-codecs.md), ROADMAP 1.4).
- Block framing of segment sections: per-block CRC and codec, and a block table that locates every
  block, so one block can be fetched and decoded on its own (SPEC §5.2, [ADR-0006](./adr/0006-block-framing.md), ROADMAP 1.5).
- The container header (magic, versions, feature flags, page size) and the two superblock slots, with
  the rules for picking the active slot (SPEC §4.2–§4.3, [ADR-0007](./adr/0007-container-header-and-superblock.md), ROADMAP 1.6).

Those pieces are library code. They do not open a database, index a document, or answer a query.
Frames, segments, storage, the write path, and search are still ahead (see [Roadmap](#roadmap)).

The first release a caller can use, `v0.1`, is ROADMAP M10. The format freezes at `v1.0` (M15).

### Why

Five properties the design does not trade away:

1. **Zero friction.** `Db::open("app.persia")` just works. No daemon and no config file for the embedded
   library. Server configuration, when it exists, is flags, then `PERSIA_*` environment variables, then
   an optional TOML file, with production-safe defaults (SPEC §14). Planned: M10, M11.
2. **Own engine.** Index format, analysis, BM25F scoring, and query execution live in this repository,
   behind a small allowed-dependency list (ADR-0001).
3. **Never zero results by default.** A query that matches nothing is relaxed step by step, and the
   response says exactly what changed (SPEC §7.6). Planned: M8. See [Features](#features).
4. **Cloud-native.** Segments are immutable. A local commit and an object-storage commit publish the
   same segment bytes. Query nodes are stateless. Planned: M12, M13.
5. **Correctness, then speed.** Checksums, crash-safe commits, and fuzzed parsers come before
   performance claims. SPEC §16.2 lists benchmark targets and labels them as targets, not promises.
   Scalar codec baselines already recorded live in [`docs/benchmarks.md`](./benchmarks.md).

### Goals and non-goals

Goals (SPEC §1.1): embedded search with an optional server; BM25F plus filters, facets, and
zero-results avoidance; the same segment bytes locally or in object storage; horizontal scale by
shard; content-addressed blobs; the embedded Rust API, embedded Go and TypeScript bindings (ADR-0018), and
Rust and Go network clients; a power loss at any point leaves committed data intact.

Out of scope for v1 (SPEC §1.2):

- Text extraction, OCR, and transcription. Clients supply text; blob bytes stay opaque.
- Vector or semantic search. The format reserves a feature flag for a later layout (SPEC §18.6).
- SQL, joins, multi-document transactions, and cross-collection queries.
- More than one writer on the same shard. A lease enforces a single writer (SPEC §11.3).
- Encryption at rest inside the file format. That stays with the disk or the storage service.

## Key concepts

Names below are the ones used in code and in the SPEC. The short definitions are
[SPEC §2](../SPEC.md#2-terminology).

| Term | Meaning |
|---|---|
| **Database** | One Persia DB: collections plus blobs. Opened by a filesystem path or a URL. |
| **Collection** | Documents that share a schema, analyzers, and ranking config. |
| **Document** | A record with a string `id`, typed fields, and an optional blob reference. |
| **Segment** | An immutable, self-contained index for one collection (SPEC §5). Once written, it is never edited. |
| **Memtable** | The in-memory buffer of recent writes. Searches see it, so a `put` is visible before the next flush. |
| **WAL** | Write-ahead log of operations not yet flushed into a segment. |
| **Manifest** | A versioned snapshot: live segments per collection, schemas, and the WAL position still needed. |
| **Commit** | Atomic publication of a new manifest. Readers move from one manifest to the next; they never see a mix. |
| **Frame** | One checksummed record in the local container (SPEC §4.4): WAL chunk, segment, manifest, padding, or tombstone sidecar. |
| **Blob** | An opaque client file, addressed by the SHA-256 of its bytes and stored in chunks (SPEC §9, ADR-0002). |
| **Shard** | An independent database that owns one hash range of document ids (SPEC §11). |
| **Relaxation** | A controlled loosening of a query so the default experience is not an empty page (SPEC §7.6). |

A few more words that show up in the roadmap and in contributor docs
([`docs/architecture.md`](./architecture.md)):

| Term | Meaning |
|---|---|
| **Leaf** | One ROADMAP checkbox. One leaf is one issue and one pull request. |
| **Milestone** | `M0` through `M15`. A milestone starts only after the milestones it depends on are done, unless the roadmap says otherwise. |
| **ADR** | An architecture decision record in [`docs/adr/`](./adr/). The SPEC says what the system does; an ADR says why. |
| **Relaxation level** | One step of the cascade, `L0` (exact) through `L5` (fallback). |

Internal document ids are segment-local. Deletions never rewrite a segment: they live in a tombstone
sidecar referenced by the manifest (SPEC §8.3). The exact sidecar bytes are still an ADR (ROADMAP 6.4.2,
SPEC §18.3).

## Features

Everything in this section is **planned** unless a paragraph says a primitive is already in
`persia-format`. You cannot call these APIs today.

| Capability | Status | Defined in |
|---|---|---|
| Open a file and search | Planned (M10) | SPEC §8, §10.2 |
| Full-text search, filters, facets, sort | Planned (M7) | SPEC §7 |
| Zero-results relaxation | Planned (M8) | SPEC §7.6 |
| Schema and documents | Planned (M3) | SPEC §6 |
| Analyzers (`standard`, `en`, `es`, `pt`, …) | Planned (M4); milestone is ready to start | SPEC §6.3 |
| Crash-safe commit and recovery | Planned (M2, M6) | SPEC §4.5, §8.6 |
| Blob storage | Planned (M9) | SPEC §9 |
| gRPC and HTTP server | Planned (M11) | SPEC §12 |
| S3, GCS, Azure, replication | Planned (M12) | SPEC §10 |
| Kubernetes, sharding | Planned (M13) | SPEC §11 |
| Embedded Go and TypeScript bindings | Planned (after M10). Design: ADR-0018, `docs/sdk/`. | SPEC §13.1 |
| Rust and Go network SDKs | Planned (M14) | SPEC §13.2 |
| Integer codecs and `None` / `Zstd` block codecs | Done, as library primitives (M1, except 1.4.4) | SPEC §4.1, §5.2 |

### Search and ranking

A search takes user text, a field list, filters, a sort, a page, and optional facets (SPEC §7.1).
The text is analyzed per field and compiled into a query: terms, phrases, prefixes, fuzzy clauses,
and boolean must / should / must-not. The default operator is AND. Invalid syntax degrades to plain
terms rather than failing the request (ROADMAP 7.1.2). Filters (term, range, exists, and / or / not)
are separate from the text and are not relaxed unless you opt in.

Ranking is BM25F (SPEC §7.2): each field has a weight, `k1`, and `b`, and a collection may add a
static rank (for example `log1p` of a sales field). Collection statistics (`N`, document frequency,
average field length) are summed across live segments. Scores must match the reference model
within a documented epsilon (SPEC §7.2). That model is part of the query milestone; it is not in the tree yet.

Execution, per segment and over the memtable, is block-max WAND (MAXSCORE as a fallback), with
filters as bitmaps and tombstones applied last (SPEC §7.3). Segments are searched in parallel.
Sort is by score or by a column. Deep paging prefers a `search_after` cursor over a large offset
(SPEC §7.4). Facets count a keyword column and merge across segments (SPEC §7.5).
`explain: true` returns a per-term score breakdown (SPEC §7.7). Highlighting is post-v1.

### Zero-results relaxation

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
| L2 | `drop_terms` | Required-term ratio steps from 100% to 75%, 50%, then one term. Low-value terms go first. |
| L3 | `prefix` | The last term (and other terms of length at least 3) expands through a prefix scan, capped (default 50). |
| L4 | `fuzzy` | Edit distance 1 for terms of length 4–7, distance 2 for length 8 or more. Corrections are reported. |
| L5 | `fallback` | A collection-defined last resort: top documents by static rank or recency, or nothing. |

The response says what happened. This JSON is the shape in SPEC §7.6. It is illustrative:

```jsonc
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
planned as ROADMAP 8.4.4 (`docs/relaxation.md`).

### Schema, documents, and analysis

A collection declares its schema up front (SPEC §6.2). Field types are `text`, `keyword`, `i64`,
`f64`, `bool`, `datetime` (microseconds UTC), and `stored` (kept, not searchable). Every document
has an `id` of at most 512 bytes, unique in the collection, and may reference one blob in v1
(several blobs per document is an open question, SPEC §18.2).

Adding a field is allowed; existing documents read that field as missing. Changing a field's type
or analyzer is rejected and needs a new collection version (reindex). Unknown JSON fields are
rejected when `strict` is true, and kept as stored-only fields when it is false (SPEC §6.4).
Documents are submitted as JSON, or as typed structs in the SDKs, and stored as CBOR.

The schema sketch in SPEC §6.2, illustrative:

```jsonc
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
Built-ins planned for the first cut (ROADMAP 4.4.2) are `standard` (Unicode word boundaries, NFKC,
lowercase), `keyword`, `whitespace`, and language analyzers `en`, `es`, and `pt` (stopwords, a
Snowball-style stemmer, accent folding). SPEC §6.3 also names `fr`, `de`, and `it`; whether those
three ship in v1 is open (SPEC §18.7). Positions, used for phrases, are opt-in per field
(SPEC §18.4). The analyzer's name and version are stored in the schema so a later change is visible.

Text analysis depends only on the finished bootstrap milestone, so it can start in parallel with the
rest of the format work (ROADMAP M4). It has not started.

### Durability

Writes are `put` (upsert by id), `delete`, batches, `get`, and `commit`, plus an auto-commit policy
(SPEC §8.1). The intended path (SPEC §8.2):

1. The operation is appended to the WAL. Durability of a call is `Buffered`, `Fsync`, or `Async`.
   The default at commit is `Fsync`.
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

None of this I/O exists yet. The rules above are the contract the storage and engine milestones
(M2, M6) have to meet, including crash injection at every step of the commit.

### Blobs

The index stores a reference, never the file bytes (SPEC §9). A `blob_id` is the SHA-256 of the
blob (ADR-0002). Persia hashes the bytes it receives; a caller-supplied id that does not match is
an error. Chunks default to 4 MiB, each with a CRC32C, so uploads can resume and reads can be
ranged. Blob bytes are stored as the client sent them (SPEC §9.2). The same content stored twice is one blob.

Write order is deliberate: the blob is durable first, then the document that references it is
committed (SPEC §9.5). A crash in between leaves an orphan, which is harmless. Garbage collection
keeps anything a live manifest still references, and deletes orphans older than a grace period
(default 24 hours). `verify` must fail if a referenced blob is missing.

How chunks are staged before the hash is known is an open question (SPEC §18.8) and has to be
decided before the pack-file work (ROADMAP 9.1.0). The `BlobId` and `BlobRef` value types
exist in `persia-format` (ROADMAP 1.10.1): limits and binary encoding are in SPEC §9.1 and ADR-0011.

## Architecture

The picture below is the intended layering in [SPEC §3](../SPEC.md#3-architecture-overview).
`persia-format` is the only crate with real code,
and it currently covers encodings, checksums, and block codecs. Frames and the superblock are still
ROADMAP 1.6–1.7. Every other crate is a workspace stub.
[`docs/architecture.md`](./architecture.md) is the contributor map (crate graph, write path, query
path) and stays the place for that detail.

```
 ┌───────────────── persia (embedded facade) ─────────────────┐
 │  Db ── Collection ── put/get/delete/search/blob             │
 ├──────────────┬──────────────┬───────────────┬──────────────┤
 │ persia-engine│ persia-blob  │persia-analysis│              │
 │ memtable/WAL │ chunks/dedup │ analyzers     │              │
 │ segments     │              │               │              │
 │ query+rank   │              │               │              │
 ├──────────────┴──────────────┴───────────────┴──────────────┤
 │ persia-storage:  Storage trait                              │
 │   memory │ local file container │ s3 │ gcs │ azure          │
 ├─────────────────────────────────────────────────────────────┤
 │ persia-format: encodings, frames, superblock, checksums     │
 └─────────────────────────────────────────────────────────────┘
      ▲ persia-server (gRPC/HTTP) embeds the same engine ◀── network SDKs (SPEC §13.2)
      ▲ embedded bindings (Go via cgo, TypeScript via N-API) call persia through persia-ffi (SPEC §13.1, ADR-0018)
```

Three rules shape the rest of the design:

1. **Immutable segments and append-only metadata.** Crash recovery, caching, and replication all
   read a snapshot instead of locking a mutable index.
2. **The same logical objects everywhere.** A segment, manifest, WAL chunk, or blob is a named
   object. Locally they are frames in one file; remotely they are one object each. The segment
   bytes do not change between the two.
3. **The engine is synchronous.** It talks to I/O only through the `Storage` trait. Async belongs
   in the cloud backends and in the server.

| Crate | Role | Status |
|---|---|---|
| `persia-format` | Encodings, checksums, block codecs. No I/O policy. | In progress (M1) |
| `persia-storage` | `Storage` trait, memory, local file, later S3 / GCS / Azure. | Planned (M2, M12) |
| `persia-analysis` | Tokenizers, normalizers, stemmers. | Planned (M4); ready to start |
| `persia-engine` | Schema, segments, WAL, commit, merge, query, BM25F, relaxation. | Planned (M3, M5–M8) |
| `persia-blob` | Content-addressed chunks, dedup, garbage collection. | Planned (M9) |
| `persia` | The embedded API: `Db` and `Collection`. | Planned (M10) |
| `persia-proto` | `persia.proto` and committed generated code. | Planned (M11) |
| `persia-server` | gRPC and HTTP. Roles: writer, reader, router. | Planned (M11, M13) |
| `persia-cli` | `persia inspect`, `dump`, `verify`, `compact`, `bench`. | Planned (M10) |

Dependencies point downward only (`persia-format` at the bottom). `cargo xtask check-deps` enforces
the graph. The public facade orders blob writes before the document commit so the engine never has
to know about blob bytes (SPEC §9.5).

On a query, once the engine exists: analyze the text, search each segment and the memtable, merge
the per-segment top-k, and if there are too few hits run the relaxation cascade. The response
includes which level matched.

## File format and storage

This page describes how the files are organized. Offsets, magic numbers, and CRC coverage are
[SPEC §4](../SPEC.md#4-local-file-container) and [SPEC §5](../SPEC.md#5-segment-format).

### Local container

`<name>.persia` starts with a 12 KiB prefix: a 4 KiB header page (magic, format versions, compatibility
flags, a database UUID) and two 4 KiB superblock pages. The slots are a double buffer on separate
pages, so one bad sector cannot lose both. Open reads both, throws away a slot whose CRC fails, and
uses the higher commit sequence (SPEC §4.2–§4.3). From byte 12288 the file is
append-only frames. Each frame has a kind, a length, a payload padded to 8 bytes, and a CRC32C.
A torn or corrupt frame ends recovery; the file is logically truncated there (SPEC §4.4).

Integers are little-endian. Headers and frames use CRC32C. Internal content hashes and shard routing
use xxh3 with seed 0. Blob ids are SHA-256 (ADR-0002), a content address that stays stable for the
life of the blob.

The compatibility rules (SPEC §15) are part of the design, and the bytes may still change before v1.0.
An unknown major version or an unknown incompatible flag is a hard reject. An unknown
read-only-compatible flag opens the file read-only. Every format change is a SPEC update, a version
or flag bump, an ADR, and a golden fixture (see [Contributing](#contributing)).

Header, superblock, and frame codecs are still ahead (ROADMAP 1.6 and 1.7).
`persia-format` already has the encodings those structures will use, plus the compression codecs
below.

### Segments and blocks

A segment is a self-contained byte string: sections, then a footer with statistics, a section table,
and a CRC (SPEC §5.1). It is a frame payload locally and a standalone object remotely, and the bytes
match. Sections hold the term dictionary, postings, frequencies, skip lists, optional positions,
length norms, columnar values, stored documents, the id map, and blob references (SPEC §5.3).
Deleted ids are not a section.

Large sections are fixed-size blocks, 16 KiB uncompressed by default. Each block carries its own
CRC, codec, and uncompressed length, so object storage can fetch one block with a range request
(SPEC §5.2). Codecs:

| Codec byte | Name | Meaning |
|---|---|---|
| 0 | `None` | The payload is the raw bytes. Its length equals `uncompressed_len`. |
| 1 | `Zstd` | Exactly one zstd frame whose declared content size equals `uncompressed_len`. |

Any other codec byte is corruption. The reader refuses a declared length above the caller's maximum
before it allocates (the decompression-bomb guard, ADR-0005). Writers may store `None` when zstd
does not shrink the block. The zstd level is a writer choice and is not stored.

`compress` / `decompress` for those two codecs is done (ROADMAP 1.4.1–1.4.3). The on-disk block
header, the block table, and random-access reads are not (ROADMAP 1.5). A few layout questions are
explicitly still open there: where a section records its block size, how a stored document larger
than one block is split, and what the section table's own codec field means next to the per-block
codec (ROADMAP 1.5.5, SPEC §5.2).

Integer codecs that will sit *inside* blocks (delta-coded doc ids, bit-packing, postings blocks,
frame-of-reference for numbers) are implemented and tested (ROADMAP 1.2, ADR-0004). They are not
wired into a segment writer yet (that is M5).

### One storage interface, several backends

The engine will see every backend as the same trait: get, ranged get, put, put-if-absent, list,
delete (SPEC §10.1). The trait sketch in the SPEC is illustrative; the Rust types are ROADMAP 2.1.

| Backend | How a database is addressed | Milestone |
|---|---|---|
| Local file | `app.persia`, or `file:///…` | M2 |
| Memory | `memory://` | M2 |
| S3 (and MinIO) | `s3://bucket/prefix` | M12 |
| GCS | `gs://bucket/prefix` | M12 |
| Azure Blob | `az://account/container/prefix` | M12 |

Cloud credentials use each provider's normal chain (environment, config files, instance metadata).
Persia-specific configuration is an override, not a requirement. An endpoint override exists for
the development emulators (MinIO, fake-gcs-server, Azurite).

A remote commit uploads new segments, then creates `manifests/<seq+1>` with put-if-absent
(SPEC §10.4). Conflict means another writer won. Readers discover the newest manifest by listing,
with an optional `CURRENT` hint. Remote reads go through a block cache (memory, and optionally local
disk) keyed by object and block (SPEC §10.6).

`replicate` mode keeps the local file as the primary and uploads committed manifests in order,
without blocking local writes on the network (SPEC §10.5). Remote readers of that replica are
eventually consistent; lag is a metric.

## Getting started

Nothing opens a database yet. `persia` exports no types, the crates are not published, and
`persia-cli` / `persia-server` have empty `main` functions. `persia-format` is the crate with
code: header, superblock, checksums, integer codecs, block compression, block framing, and frames
(encode, decode, and scan; the streaming encoder, 1.7.5, is not written yet).
A git dependency on `persia-format` does not provide `Db::open`.

The calls in [Embedded API](#embedded-api) (`Db::open`, `create_collection`, `put`, `commit`,
`search`) are illustrative. They are the SPEC §6.2 / §7.6 / §8.1 shape, not functions in the tree.
A search response is specified to include a `relaxation` object (`level`, `applied`, `original`,
`effective`, `corrected`). No function returns it. The same walkthrough, with a FAQ, is the
website page [Getting started](https://tomiok.github.io/persiaDB/guide/getting-started/).
Agents should start from [llms.txt](https://tomiok.github.io/persiaDB/llms.txt) and the
[For AI agents](https://tomiok.github.io/persiaDB/guide/for-agents/) page.

## Embedded API

**Planned.** The public crate `persia` is a stub. The first usable release is `v0.1` (ROADMAP M10),
which depends on the write path, query, relaxation, and blobs. Method names below are illustrative
and not final. This is the sketch already in the README:

```rust,ignore
let db = persia::Db::open("app.persia")?;
// Collections declare their schema up front (SPEC §6.2).
let products = db.create_collection(schema)?;
products.put(doc)?;
products.commit()?;

// No exact match for the typo? The query is relaxed (here: fuzzy, SPEC §7.6),
// and the response says what changed: "runing" -> "running".
let results = products.search("runing")?;
```

The intended surface (SPEC §8, §9.6, ROADMAP 10.1):

- `Db::open` on a path or URL, `OpenOptions`, and `close`.
- Create, open, and drop a collection, and read its schema back.
- `put`, `put_batch`, `delete`, `get`, `commit`, and `search`.
- Documents as a dynamic map, or as a `serde` struct (`put_struct`).
- `put_blob` / `get_blob` / ranged reads. A convenience `put` that takes a document and a blob writes
  the blob first, then commits the document.
- A stable error type. Library code returns `Result`; corrupted bytes are an error, never a panic.

Handles are `Send` and `Sync`. Many readers share a snapshot. A second writer fails immediately.
A `put` is visible to later searches on that database (read-your-writes, SPEC §8.2–§8.4).
An optional `tokio` feature will offer a thin `spawn_blocking` wrapper (ROADMAP 10.1.6); the core
API stays synchronous.

Embedded mode has no authentication and no config file. Knobs, once they exist, are builder options
with defaults: memtable size, commit interval, fsync policy, merge threads, search threads, cache
sizes, block size, blob chunk size, and the `relax.*` settings (SPEC §14).

The `persia` command-line tool (`inspect`, `dump`, `verify`, `compact`, `stats`, JSONL import,
a one-shot search, and a small bench) is part of the same milestone (ROADMAP 10.4). The binary in
the tree today is a stub. The `v0.1.0-alpha` tag is specified to say, in the release itself, that
the format is still unstable (ROADMAP 10.5.2).

## Server, cloud, and Kubernetes

**Planned.** Nothing here is running. The server milestone (M11) starts after the embedded API.
Cloud backends (M12) and distribution (M13) come after the server. Network SDKs (M14) follow the protocol.
Embedded Go and TypeScript bindings are a separate design (ADR-0018, [`docs/sdk/`](./sdk/README.md)) and
wait on the embedded API (M10), not on the server.

### Server

gRPC is the primary API; HTTP/JSON mirrors it for tooling. Both track `persia.proto`
(SPEC §12.1–§12.2). Generated Rust code is committed; building the project does not need `protoc`
(ADR-0003). The service sketch in the SPEC is illustrative:

```proto
service Persia {
  rpc CreateCollection(CreateCollectionRequest) returns (Collection);
  rpc Put(stream PutRequest) returns (PutResponse);        // docs, optional blob chunks
  rpc Delete(DeleteRequest) returns (DeleteResponse);
  rpc Get(GetRequest) returns (Document);
  rpc Search(SearchRequest) returns (SearchResponse);      // includes relaxation info
  rpc GetBlob(GetBlobRequest) returns (stream BlobChunk);  // supports ranges
  rpc Commit(CommitRequest) returns (CommitResponse);
  rpc Stats(StatsRequest) returns (StatsResponse);
}
```

Responses carry the manifest sequence they were served from, so a client can ask for
read-your-writes with `min_seq`. Errors map to canonical gRPC codes.

TLS is optional and on by configuration, with API keys or bearer tokens, and optional mutual TLS
(SPEC §12.3). Embedded mode stays unauthenticated. Health checks are `/livez` and `/readyz`
(ready means recovery has finished). Shutdown drains, flushes, and drops the writer lock or lease.

Observability is `tracing` spans, OpenTelemetry export, and a Prometheus `/metrics` endpoint:
query latency by relaxation level, cache hit ratio, segment counts, merge backlog, WAL bytes,
replica lag, and blob throughput (SPEC §12.4). Logs are structured JSON, and secrets are not logged
(SPEC §17).

The container image is planned as distroless, non-root, with a read-only root filesystem, built in
CI for amd64 and arm64 with an SBOM (ROADMAP 11.5, SPEC §11.6).

### Cloud

Opening `s3://…`, `gs://…`, or `az://…` selects the backend (SPEC §10.2). The remote layout under
a prefix is immutable manifests, segments, tombstone sidecars, WAL chunks, blobs, and a `LEASE`
object (SPEC §10.4). Commit is put-if-absent of the next manifest. Each provider's conditional
create is part of the work (MinIO `If-None-Match`, GCS generation match, Azure `If-None-Match`),
and a backend that cannot do it fails clearly (ROADMAP 12.1.4).

Provider quickstarts and a note on request cost are ROADMAP 12.6, written when the backends exist.
Development emulators are wired in the integration environment at M2 and filled in at M12
(ROADMAP 2.5, 12.2.4).

### Kubernetes and sharding

A cluster is a set of shards. Each shard is its own database under `<prefix>/shard-<n>/`.
Documents route by `hash(id) mod shard_count` using xxh3. The shard count is fixed when the
collection set is created; splitting shards is a post-v1 design that takes advantage of immutable
segments (SPEC §11.1, §18.5, ROADMAP 13.6).

Roles, all in `persia-server` (SPEC §11.2):

- **Writer** — holds one shard's lease, accepts writes, compacts, uploads segments.
- **Reader** — serves queries from committed manifests, holds no shard lease, scales horizontally.
- **Router** — scatters a query, merges top-k and facets. It may run in the same process.

The lease carries a fencing token. Every manifest embeds that token. A writer whose lease has
lapsed stops writing, and readers reject a manifest whose token went backwards (SPEC §11.3).
Readers poll for the next manifest (default 1 second, with jitter) and swap snapshots
atomically. Lag is `persia_replica_lag_seconds` (SPEC §11.4).

Cross-shard scoring uses each shard's own statistics by default. `global_stats: true` makes the
router collect statistics before scoring (SPEC §11.5).

The Helm chart planned in SPEC §11.6: a StatefulSet for writers (one pod per shard, a volume for
cache and WAL), a Deployment plus HPA for readers, a headless Service for discovery, pod disruption
budgets, readiness and liveness probes, and a ServiceMonitor. Workload-identity examples for AWS,
GCP, and Azure are part of that milestone (ROADMAP 13.5).

### SDKs

Two families, neither implemented (SPEC §13).

**Embedded bindings** sit in-process beside the Rust `Db` API: Go via cgo and TypeScript via N-API, both
through a C ABI crate `persia-ffi` (ADR-0018, [`docs/sdk/`](./sdk/README.md)). They are blocked on the
embedded API (M10). A browser/edge WASM build is a later package, not the first one.

**Network clients** are Rust (`sdk/rust`, crate `persia-client`) and Go (`sdk/go`, module `persia-go`).
They sit on the gRPC protocol (SPEC §13.2). Each adds connection pooling, retries with backoff on
idempotent calls, deadlines, batching, blob streaming, and shard-aware writes. A shared, language-neutral
conformance suite runs against both SDKs and the server (ROADMAP 14.1). A compatibility matrix is part of
the SDK milestone (ROADMAP 14.4). Both clients are part of that milestone, after the protocol exists.

## Roadmap

Work is a tree: milestone, then section, then leaf. Every leaf is one issue and one pull request.
Status lives on the leaves only (`[ ]` todo, `[~]` in progress, `[x]` done). [`PROGRESS.md`](../PROGRESS.md)
derives the rest, and it is the live status. The table below is the **2026-10-10** snapshot
(51 / 400 leaves, 13%). No leaf is currently marked `[~]`.

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
| **M5** Segments | Segment writer and reader | M1, M3, M4 | Blocked, 0/35 |
| **M6** Durability | WAL, commit, merge, recovery | M2, M5 | Blocked, 0/29 |
| **M7** Query | Parser, BM25F, top-k, facets | M5 (M6 for end-to-end) | Blocked, 0/29 |
| **M8** Relaxation | The zero-results cascade | M7 | Blocked, 0/23 |
| **M9** Blobs | Pack file, remote blobs, GC | M1, M2 | Blocked, 0/19 |
| **M10** Embedded API | `Db` / `Collection`, CLI, `v0.1` | M6, M7, M8, M9 | Blocked, 0/17 |
| **M11** Server | gRPC, HTTP, image | M10 | Blocked, 0/22 |
| **M12** Cloud | S3, GCS, Azure, cache, replicate | M2, M6, M9, M11 | Blocked, 0/28 |
| **M13** Kubernetes | Lease, shards, router, Helm | M11, M12 | Blocked, 0/24 |
| **M14** SDKs | Rust and Go network clients | M11; M13 for shard routing | Blocked, 0/17 |
| **M15** v1.0 | Benchmarks, hardening, format freeze | M1–M14 (benchmarks may start after M7) | Blocked, 0/21 |

Block framing (1.5), the container header and superblock (1.6), and frame encode, decode, and scan
(1.7.1–1.7.4) are done. The next format leaves are the streaming frame encoder (1.7.5), then mmap (1.8).
Text analysis (M4) may proceed alongside them. The generated ready-next list is in [`PROGRESS.md`](../PROGRESS.md).

### M0 — Project bootstrap

Done. The Cargo workspace and stub crates, the pinned toolchain, CI (fmt, clippy, tests, `cargo deny`,
Miri on `persia-format`), the `justfile`, repo tooling in Rust (`cargo xtask`, ADR-0003), issue
templates, and the documentation scaffolding: this repository's rules, the ADR process, ADR-0001 and
ADR-0002, and [`docs/architecture.md`](./architecture.md). No dependencies.

### M1 — Format primitives

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

### M2 — Storage abstraction and local backends

Blocked on M1. The `Storage` trait and its error types, an in-memory backend, a deterministic
failure injector for crash tests, a local object-per-file backend, and the single-file container
adapter: create, append, the three-step commit, recovery, and compaction, each with crash tests.
Also the first docker-compose file for later cloud emulators. After M1 this can proceed in parallel
with schema and analysis.

### M3 — Schema, documents, collections

Blocked on M1. Field types and collection schema (including a stable schema hash and evolution
rules), document validation, JSON and CBOR, and the manifest model. Can proceed in parallel with
M2. Segment writing (M5) needs this and M4.

### M4 — Text analysis

Ready: its only dependency, M0, is done. Not started (0 of 22). Unicode and whitespace tokenizers,
NFKC, lowercasing, accent folding, English, Spanish, and Portuguese stopword lists and stemmers,
synonyms and edge n-grams for later relaxation, and the named analyzer pipelines. Fuzzing arbitrary
UTF-8 is part of the milestone. This is the one milestone that can start before M1 finishes.

### M5 — Segment writer and reader

Blocked on M1, M3, and M4. The in-memory index builder and every section writer and reader: term
dictionary, postings, positions, norms, columns, stored documents, ids, blob references, and the
footer. Includes `verify`, inspect and dump commands, property tests against a model, golden
segments, and fuzz targets. This is the heart of the index format. Query (M7) can start from a
reader; the full write path (M6) also needs storage.

### M6 — Write path, durability, merging

Blocked on M2 and M5. The memtable, the WAL and its replay, flush and the local commit protocol,
upserts and deletes via tombstone sidecars, tiered compaction, and open/recovery. Crash-loop tests
(kill the process and reopen) land here. The embedded API waits on this milestone.

### M7 — Query engine and ranking

Blocked on M5. End-to-end tests also need M6. The query AST and a parser that does not fail on
messy user text, collection statistics, BM25F checked against hand-computed examples and the
reference scorer, boolean and phrase matching, top-k with block-max WAND, filters, sort,
pagination, facets, parallel segment search, and explain. Benchmarks of this milestone may start
the v1 performance work early (M15).

### M8 — Zero-results avoidance

Blocked on M7. The cascade in [Features](#features): orchestration, per-level score offsets, the
time budget, synonyms, dropping terms, prefix, fuzzy (Levenshtein automaton over the term
dictionary), the fallback, and the response metadata. Plus a synthetic typo suite and a regression
gate on the zero-result rate. The embedded `v0.1` release depends on this milestone (M10 depends on M8).

### M9 — Blob store

Blocked on M1 and M2. An ADR for upload staging first (SPEC §18.8), then hashing and chunking,
the local pack file, the remote key layout, ordering a blob write before the document commit, and
garbage collection that never deletes a blob a live snapshot still references.

### M10 — Embedded API

Blocked on M6, M7, M8, and M9. The first user-facing release. Public `Db` and `Collection`,
concurrency and shutdown semantics, rustdoc and examples, a README quickstart that CI actually
runs, the `persia` CLI, and a `v0.1.0-alpha` tag that states the format is unstable.
See [Embedded API](#embedded-api).

### M11 — Server

Blocked on M10. `persia.proto`, committed generated code, the gRPC service, the HTTP/JSON gateway
and its OpenAPI spec, configuration, health checks, metrics and tracing, authentication and TLS,
and the container image. See [Server, cloud, and Kubernetes](#server-cloud-and-kubernetes).

### M12 — Cloud storage backends

Blocked on M2, M6, M9, and M11. S3, GCS, and Azure through a shared async layer, the remote commit
protocol, a block cache, replicated mode, and conformance tests against MinIO, fake-gcs-server,
and Azurite. Documentation of IAM and request cost is part of the milestone.

### M13 — Distribution and Kubernetes

Blocked on M11 and M12. Writer leases and fencing, reader refresh, hash sharding and the router,
role flags and discovery, the Helm chart, and a `kind` end-to-end test (ingest, query, kill the
writer, fail over, roll). Resharding is a design record only for v1, with an optional prototype
behind a flag.

### M14 — SDKs

Blocked on M11 for a stable protocol, and on M13 for shard-aware routing. A language-neutral
conformance suite, then the Rust client and the Go client, then a compatibility matrix and release
automation. These are network clients. In-process Go and TypeScript bindings are ADR-0018 and
[`docs/sdk/`](./sdk/README.md), blocked on M10, with no roadmap leaves yet.

### M15 — Benchmarks, hardening, v1.0

Blocked on M1 through M14 as a release, with room to benchmark after M7. A reproducible harness and
a regression gate, SIMD and other work driven by profiles, soak and chaos tests, a security review,
and the format 1.0 freeze: open questions closed as ADRs, golden fixtures for every released
version, a documentation site (embedded, server, cloud, Kubernetes, relaxation tuning, operations),
and the `v1.0.0` release. This guide is source material for that later site (ROADMAP 15.5.4).

## Contributing

Persia DB is a database, so correctness comes first and speed comes after, with measurements.

1. Read [`SPEC.md`](../SPEC.md) and [`CLAUDE.md`](../CLAUDE.md). The rules apply to human and
   AI-assisted changes.
2. Pick a leaf from [`ROADMAP.md`](../ROADMAP.md). [`PROGRESS.md`](../PROGRESS.md) lists what is
   ready next. One leaf, one pull request.
3. If the SPEC is unclear, or you want to change a decision, open an issue before writing the code.
   The code and the SPEC are not allowed to diverge silently.

Setup is the pinned toolchain (rustup reads `rust-toolchain.toml`) and `cargo test --workspace`.
Before a pull request: format, clippy with warnings denied, tests, `cargo xtask check-deps`, and
`cargo xtask progress --check`. The long form is [`CONTRIBUTING.md`](../CONTRIBUTING.md).

On-disk format changes need all four of: a SPEC update, a version or flag bump, an ADR under
[`docs/adr/`](./adr/), and a golden test. Bytes from disk, the network, or a user are never
`unwrap`ped; corruption is `Err`. `unsafe` is confined to the memory-map module in `persia-format`,
and any other use needs an ADR.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):
`feat(engine): ...`, `fix(format): ...`, `docs: ...`.

By participating you agree to the [Code of Conduct](../CODE_OF_CONDUCT.md).
Report security issues privately, as described in [`SECURITY.md`](../SECURITY.md). The project is
pre-alpha: only the latest commit on `main` receives fixes. Treat a panic or unbounded memory use
on a crafted database file as a security issue (SPEC §17).

## License

Persia DB is licensed under the [Apache License, Version 2.0](../LICENSE). Copyright and attribution
live in [`NOTICE`](../NOTICE). Contributions are licensed under the same terms, as stated in
section 5 of the license.
