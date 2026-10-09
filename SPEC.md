# Persia DB — Specification

Status: **Draft v0.1** · Format version: **1.0 (unstable until v1.0 release)**

Keywords: MUST, SHOULD, MAY follow RFC 2119.

---

## 1. Goals and non-goals

### 1.1 Goals
- Embedded, single-file search database ("SQLite for search") with an optional server mode.
- Full-text search with BM25F ranking, filters, facets, and **zero-results avoidance by default**.
- Storable in the local filesystem **or** object storage (S3, GCS, Azure Blob) using the same segment bytes.
- Horizontal scalability: stateless query nodes, immutable segments, shard-per-writer.
- Content-addressed blob storage for client files (pdf, jpg, mp4...) referenced from documents.
- First-class SDKs: Rust and Go.
- Crash-safe: a power loss at any instruction never corrupts committed data.

### 1.2 Non-goals (v1)
- Text extraction, OCR, transcription (clients supply metadata/text; blobs are opaque).
- Vector / semantic search (planned post-v1, format reserves feature flags).
- SQL, joins, multi-document transactions, cross-collection queries.
- Multi-writer on the same shard (single writer per shard, enforced by lease).
- Encryption at rest inside the format (delegated to storage layer / disk in v1).

---

## 2. Terminology

| Term | Meaning |
|---|---|
| **Database** | One logical Persia DB: a set of collections + blobs. Opened via path or URL. |
| **Collection** | Named set of documents sharing a schema, analyzers and ranking config. |
| **Document** | A record: external `id` (string), typed fields, optional `blob_ref`. |
| **Segment** | Immutable, self-contained index unit for one collection (SPEC §5). |
| **Memtable** | In-memory mutable buffer of recent writes; searchable (near-real-time). |
| **WAL** | Write-ahead log of operations not yet flushed to a segment. |
| **Manifest** | Versioned snapshot listing live segments per collection + schemas. |
| **Commit** | Atomic publication of a new manifest. |
| **Frame** | Checksummed record in the single-file container (SPEC §4). |
| **Blob** | Opaque client file, content-addressed, stored by chunks (SPEC §9). |
| **Shard** | An independent Database that owns a hash range of document ids (SPEC §11). |
| **Relaxation** | Controlled loosening of a query to avoid empty results (SPEC §7.6). |

---

## 3. Architecture overview

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
      ▲ same engine embedded in:  persia-server (gRPC/HTTP) ◀── SDKs (Rust, Go)
```

Principles:
1. **Immutable segments** + **append-only metadata** ⇒ trivial crash recovery, caching and replication.
2. **Logical objects**: Segment, Manifest, WalChunk, Blob. Backends decide physical layout
   (single file locally, one object per entity remotely). Segment bytes are identical in both.
3. **Engine is synchronous and storage-agnostic**; async exists only at the cloud I/O boundary and server.

---

## 4. Local file container

A local database is two files:
- `<name>.persia` — index container (header + frames).
- `<name>.persia.blobs` — blob pack file (SPEC §9.3).

### 4.1 Conventions
- All integers little-endian. All structures 8-byte aligned. Offsets are absolute `u64`.
- Checksum: **CRC32C** for headers/frames; **xxh3-64** for internal content hashes where noted.
  Blob ids are **SHA-256** (SPEC §9.1, ADR-0002).
- Compression: **zstd** (level configurable); every compressed unit declares its codec.
- Varints: unsigned LEB128, least significant 7-bit group first, high bit = "more bytes follow". Only the shortest
  encoding is valid; an overlong encoding, or a value wider than its declared type (`u32`: at most 5 bytes, the 5th
  holding 4 bits; `u64`: at most 10 bytes, the 10th exactly `0x01`), is corruption.

### 4.2 Header (first 8192 bytes)
```
0      8   magic          "PERSIA\0\1"
8      2   format_major   u16   (reader MUST reject unknown major)
10     2   format_minor   u16
12     4   header_crc     u32   (over bytes 0..12 + 16..4096)
16     8   incompat_flags u64   (reader MUST reject unknown bits)
24     8   rocompat_flags u64   (unknown bits => open read-only)
32    16   db_uuid
48     4   page_size      u32   (default 4096)
52     4   reserved
4096  2×2048  superblock slots A and B (see 4.3)
```

### 4.3 Superblock (double-buffered, LMDB-style)
Each 2048-byte slot:
```
0   8  seq            u64  (monotonic commit counter)
8   8  manifest_off   u64  (offset of the Manifest frame)
16  8  manifest_len   u64
24  8  log_end        u64  (end of last durable frame)
32  8  wal_start      u64  (first WAL frame still needed)
40  8  fence_token    u64  (writer fencing, SPEC §11.3)
48  ..  reserved
2044 4 crc            u32  (CRC32C over bytes 0..2044 of the slot)
```
Open: read both slots, discard invalid CRC, use the highest `seq`. Commit alternates slots.

### 4.4 Frames
After the header the file is an append-only sequence of frames:
```
0   4  frame_magic   bytes "PFR1" (0x50 0x46 0x52 0x31)
4   1  kind          1=WalChunk 2=Segment 3=Manifest 4=Padding 5=TombstoneSidecar
5   3  flags         (bit0: zstd-compressed payload)
8   8  payload_len   u64  (unpadded length)
16  8  frame_seq     u64
24  ..  payload       zero-padded to the next multiple of 8 bytes
..  4  crc32c        u32  over bytes 4..(24 + padded payload), i.e. kind..padding
..  4  reserved      u32  MUST be zero
```
Total frame length is `24 + pad8(payload_len) + 8`, so every frame starts 8-byte aligned (SPEC §4.1).
Non-zero padding or `reserved` bytes are corruption.

Unknown `kind` values with a valid checksum are skipped (forward compatibility); a reader MUST NOT
interpret them.

Recovery: starting at `log_end` from the chosen superblock, scan forward; any frame
failing checksum or length validation terminates the scan, and the file is logically truncated there.

### 4.5 Commit protocol (local)
1. Append Segment / WalChunk frames. `fdatasync`.
2. Append Manifest frame referencing live segments. `fdatasync`.
3. Write the *inactive* superblock slot with `seq+1`. `fdatasync`.
4. Publish in-memory state to readers.

A crash before step 3 completes leaves the previous superblock valid ⇒ the prior commit.

### 4.6 Compaction of the container
Dead frames (replaced segments, superseded tombstone sidecars, old manifests, consumed WAL chunks) are reclaimed by **rewriting live segments into a new file**
and atomically `rename`-ing it over the old one (fsync file, then directory). Triggered by dead-space ratio (default 40%).

---

## 5. Segment format

A segment is a **self-contained byte string** with its own footer, usable as a frame payload (local)
or as an independent object (remote). Segments belong to exactly one collection and never change.

### 5.1 Layout
```
[ section 0 ][ section 1 ] ... [ section N ][ segment footer ]
```
Sections are block-oriented (SPEC §5.2). The footer (last 4096 bytes max, located by a trailing
`footer_len u32 + magic`) contains:
- segment_id (u128), collection schema hash, format version, doc count
  (no deleted count: deletions live outside the segment, SPEC §8.3)
- per-field statistics: `sum_field_len`, `docs_with_field`
- section table: `{section_kind, field_id, offset, len, codec}`
- min/max `doc_id_ext` (for routing/pruning), creation time, merge generation
- footer CRC32C

### 5.2 Blocks
All large sections are split into **fixed-size logical blocks (default 16 KiB uncompressed)**, each
with `{crc32c, codec, uncompressed_len}` header. Blocks are independently decodable ⇒ range requests
and per-block caching work on object storage.

### 5.3 Sections (per segment)
| Section | Per | Content | Encoding |
|---|---|---|---|
| `TermDict` | text/keyword field | term → postings pointer + df + total_tf | FST (key=term bytes, value=packed u64 index into term-info array) |
| `TermInfo` | field | `{postings_off, df, ttf, skip_off, positions_off}` | fixed-width records |
| `Postings` | field | doc ids | blocks of **128** delta-coded, bit-packed; tail block varint |
| `Freqs` | field | term frequencies | blocks of 128 bit-packed, aligned with Postings |
| `Skip` | field | per block: `last_docid`, `block_offset`, `max_tf`, `min_norm` | enables skipping and block-max WAND |
| `Positions` | field (opt-in) | token positions | delta + bit-packed; only if field has `positions: true` |
| `Norms` | text field | quantized field length per doc (1 byte, Lucene-style) | array |
| `Columns` | numeric/keyword/date/bool field | doc values for filter/sort/facet | columnar; low-card keyword = dictionary + bit-packed ords; numerics = FOR/bit-pack; roaring for bool/sparse |
| `DocStore` | segment | original documents (CBOR) | row blocks, zstd |
| `IdMap` | segment | external id ↔ internal `docid` | FST (id→docid) + array (docid→id offset) |
| `BlobRefs` | segment | docid → blob_ref | column |

Deleted docids are **not** a segment section: segments are immutable, so deletions are stored in
tombstone sidecars (SPEC §8.3).

### 5.4 Internal doc ids
`docid` is a segment-local `u32`, dense `0..doc_count`. Segments MUST NOT exceed `2^31` docs.
Doc order within a segment is insertion order unless the collection declares `index_sort` (optional, post-v1).

### 5.5 Merging
Merging reads N segments, drops tombstoned docs, remaps docids, and writes one new segment
(fresh statistics). Merge policy: tiered (see 8.5).

---

## 6. Schema, collections, documents

### 6.1 Field types
| Type | Indexed as | Notes |
|---|---|---|
| `text` | analyzed terms + norms (+positions opt-in) | BM25F candidate |
| `keyword` | exact term, doc values | filters, facets, sort |
| `i64`, `f64` | doc values (+ range via column scan/skip) | |
| `bool` | doc values | |
| `datetime` | `i64` microseconds UTC | |
| `stored` | DocStore only | not searchable |

Every document additionally has: `id` (string, ≤ 512 bytes, unique per collection) and optional `blob` (SPEC §9).

### 6.2 Collection definition
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
  "synonyms": [["tv","television"]]
}
```
- Schema is **declared at creation**. Adding fields is allowed (existing docs read as missing); changing a field type is NOT.
- Schema hash is recorded in every segment; the manifest holds the current schema.

### 6.3 Analyzers
Pipeline: `char_filter* → tokenizer → token_filter*`.
Built-ins: `standard` (Unicode UAX#29 words, NFKC, lowercase), `keyword`, `whitespace`, language analyzers
(`en`, `es`, `pt`, `fr`, `de`, `it`: stopwords + snowball-style stemmer + accent folding).
Analyzer name+version is recorded in the schema; changing an analyzer requires reindex (new collection version).

### 6.4 Document encoding
Documents are submitted as JSON (or typed structs in SDKs) and stored as CBOR in `DocStore`.
Unknown fields (not in schema): rejected by default (`strict: true`), or kept as `stored` if `strict: false`.

---

## 7. Search

### 7.1 Query model
```jsonc
{
  "q": "zapatillas rojas nike",          // user text, analyzed per field
  "fields": ["title^3", "body"],          // default: all text fields with schema boosts
  "filter": { "and": [ {"term": ["brand","nike"]}, {"range": ["price", {"lte": 100}]} ] },
  "sort": "score",                        // or [{"field":"price","order":"asc"}]
  "limit": 20, "offset": 0,               // or "search_after": cursor
  "facets": ["brand"],
  "relax": { "min_results": 1 },          // overrides collection default
  "explain": false
}
```
Programmatic query AST (term, phrase, prefix, fuzzy, bool must/should/must_not, filter, boost, constant_score)
is the internal form; `q` text is compiled to it (SPEC §7.6 / default operator AND).

### 7.2 Scoring: BM25F
Per term `t` and doc `d` over fields `f` with weights `w_f`:
```
tf~(t,d) = Σ_f  w_f · tf(t,d,f) / (1 − b_f + b_f · len_f(d)/avglen_f)
score(t,d) = idf(t) · tf~ · (k1+1) / (k1 + tf~)
idf(t) = ln(1 + (N − df + 0.5)/(df + 0.5))
```
Total = Σ_t score + static_rank contribution. `N`, `df`, `avglen` are **collection-level**, summed across live segments
(and across shards when `global_stats: true`, SPEC §11.5). Norms are quantized; implementation MUST match the reference
(`persia-engine/tests/reference`) within a documented epsilon.

### 7.3 Execution
- Per segment: build term iterators → boolean combination → collect top-k with **block-max WAND** (MAXSCORE as fallback).
- Filters evaluate to roaring bitmaps (or column predicates) intersected with candidates; tombstones applied last.
- Segments are searched in parallel (thread pool, configurable); per-segment top-k heaps merge into the global top-k.
- Memtable is searched like a tiny segment (near-real-time visibility after `put` returns).

### 7.4 Sorting, pagination
Sort by score or by a column. `offset/limit` supported; `search_after` cursor `(score, segment_gen, docid)` preferred for deep paging.

### 7.5 Facets
Count over a keyword column for the matching bitmap (exact, per segment, then merged). Top-N values returned.

### 7.6 Zero-results avoidance (default on)
If the number of hits `< relax.min_results` (default **1**), the engine runs the **relaxation cascade**,
stopping at the first level that satisfies `min_results` (or continuing to fill `limit` if `relax.fill = true`):

| Level | Name | Behavior |
|---|---|---|
| L0 | `exact` | All query terms required (AND), analyzed |
| L1 | `synonyms` | Expand terms with collection synonyms (OR within group) |
| L2 | `drop_terms` | `minimum_should_match` steps over terms ordered by ascending IDF-importance: 100% → 75% → 50% → 1 term. Stopwords/low-IDF terms dropped first |
| L3 | `prefix` | Prefix-expand the last term (and others if len ≥ 3) via FST range scan (cap expansions, default 50) |
| L4 | `fuzzy` | Edit distance 1 (len 4–7) / 2 (len ≥ 8) via Levenshtein automaton ∩ FST; corrected terms reported |
| L5 | `fallback` | Per collection: `static_rank` (top by static rank / recency), `none` |

Rules:
- Results from a stricter level always rank above results from a looser level (score offset per level).
- The response MUST state what happened:
```jsonc
"relaxation": { "level": "fuzzy", "applied": ["drop_terms:75%","fuzzy"],
                "original": "zapatilas rojs", "effective": "zapatillas rojas",
                "corrected": {"zapatilas":"zapatillas","rojs":"rojas"} }
```
- Filters are **never** relaxed unless `relax.filters = "last_resort"` is explicitly set (default: never).
- Relaxation has a time/work budget (`relax.budget_ms`, default 50 ms); exceeding it returns best-so-far with `truncated: true`.
- Disable per query with `relax: false`.

### 7.7 Highlighting / explain (optional, post-MVP)
`explain: true` returns per-term BM25 contributions and which relaxation level matched. Highlights are post-v1.

---

## 8. Write path and lifecycle

### 8.1 Operations
`put(doc)` (upsert by id), `delete(id)`, `put_batch`, `commit()`/auto-commit policy, `get(id)`.

### 8.2 Flow
1. Op appended to the **WAL** (batched, group commit). Durability level per call: `Buffered | Fsync | Async` (default `Fsync` at commit).
2. Op applied to the **memtable** (visible to searches).
3. When memtable exceeds size/age threshold (default 64 MiB / 10 s idle), it is **flushed** into a new segment (frame/object).
4. A **commit** publishes a new manifest (local: SPEC §4.5; remote: SPEC §10.4).
5. WAL chunks fully covered by committed segments are dropped (`wal_start` advances).

### 8.3 Upserts and deletes
Upsert = tombstone for the previous docid (looked up via `IdMap` across segments newest-first) + insert into memtable.
Delete = tombstone only. Tombstones are the only mutable-looking state; they live in per-segment **tombstone sidecars**
(versioned, referenced by the manifest), never inside the immutable segment bytes.

Tombstone sidecar (proposed; exact byte layout fixed by an ADR in ROADMAP 6.4.2, see SPEC §18.3):
- One sidecar per `(segment_id, version)`; `version` is a `u64` that increases by 1 per rewrite.
- Payload: `segment_id u128`, `version u64`, `doc_count u32` (must equal the segment's), then the **full** set of deleted
  docids as a serialized roaring bitmap (portable format). Full set, not a delta, so one sidecar is enough to read.
- Docids `>= doc_count` are corruption.
- Stored locally as a `TombstoneSidecar` frame (SPEC §4.4), remotely as `tombstones/<collection>/<segment_id>.<ver>.tomb` (SPEC §10.4).
- The manifest maps each live segment to its current sidecar version (absent = no deletions). Superseded sidecars become
  garbage once no snapshot references them (same lifetime rule as segments, SPEC §8.5).

### 8.4 Concurrency
- **Single writer** per database/shard (file lock locally; lease remotely, SPEC §11.3). Opening a second writer fails fast.
- Any number of readers; readers see a consistent manifest snapshot (MVCC by manifest `seq`).

### 8.5 Compaction (merge policy)
Tiered: group segments by size tier (factor 10), merge when ≥ 8 in a tier or when deleted ratio > 20%.
Merges run in background threads, are crash-safe (new segment + new manifest commit), and never block reads or writes.
Obsolete segments are deleted only after no snapshot references them (reference counting / epoch).

### 8.6 Recovery
On open: pick superblock → load manifest → replay WAL frames after `wal_start` into a memtable (validating CRCs,
truncating at the first invalid frame) → ready. Recovery MUST be idempotent.

---

## 9. Blobs

### 9.1 Model
Documents may reference one blob (v1): `blob_ref = { blob_id, size, mime, chunk_size, user_meta? }`.
The index NEVER contains blob bytes — only the reference. `blob_id` is the **SHA-256** of the blob's bytes (32 bytes;
hex-encoded, lowercase, wherever it appears as text) (ADR-0002). Persia always computes it from the bytes it receives;
if a caller supplies an expected id, a mismatch is an error. Chunks must not become addressable under a
`blob_id` before it is verified (upload staging: SPEC §18.8). `BlobRef`/`BlobId` are value types in `persia-format` so
that documents and segments can carry them without depending on `persia-blob`.

### 9.2 Properties
- Content-addressed ⇒ automatic **dedup**, idempotent `put`.
- Chunked (default **4 MiB**), each chunk with CRC32C ⇒ resumable upload, **range reads**, streaming.
- Not compressed (clients' files are already compressed).
- Metadata (mime, filename, tags) lives in the document and is indexed per schema; the engine never inspects blob content.

### 9.3 Local layout (`.persia.blobs`)
Pack file: header + append-only `BlobChunk` frames `{blob_id, chunk_idx, len, crc, bytes}` + periodic `BlobIndex` frames
(`blob_id → [chunk offsets]`) + superblock like SPEC §4.3. GC rewrites live blobs into a new pack (like container compaction).

### 9.4 Remote layout
`blobs/<hex[0..2]>/<hex(blob_id)>/<chunk_idx>` objects (64-character lowercase hex) (or one object per blob below a size threshold, configurable).

### 9.5 Write ordering and GC
1. Write blob chunks (and blob index). Durable.
2. Commit the document referencing it.
A crash between 1 and 2 leaves an **orphan blob** (harmless). GC: mark = blob_ids referenced by live manifests;
sweep = orphans older than a grace period (default 24 h). A referenced blob must never be missing; `verify` checks this.

### 9.6 API
`put_blob(reader) -> BlobRef`, `get_blob(id) -> Stream`, `get_blob_range(id, range)`, `delete` via document delete + GC.
Convenience: `put(doc, blob: Blob::from_path(..))` performs both steps in the right order.

---

## 10. Storage abstraction and cloud backends

### 10.1 Trait (sketch)
```rust
pub trait Storage: Send + Sync {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes>;
    fn get(&self, key: &Key) -> Result<Bytes>;
    fn put(&self, key: &Key, data: Bytes) -> Result<()>;                 // atomic visibility
    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome>; // commit primitive
    fn list(&self, prefix: &Key) -> Result<Vec<ObjectMeta>>;
    fn delete(&self, key: &Key) -> Result<()>;
    fn capabilities(&self) -> Capabilities; // e.g. conditional_put, strong_list, append
}
```
A **container adapter** implements the local single-file layout (SPEC §4) on top of the filesystem and
presents the same logical objects to the engine.

### 10.2 URL schemes
`persia::open("app.persia")`, `file:///…`, `s3://bucket/prefix`, `gs://bucket/prefix`, `az://account/container/prefix`,
`memory://`. Scheme selects the backend. **Zero Persia-specific configuration** is required when the ambient environment
provides credentials.

### 10.3 Credentials
Use each provider's standard chain: env vars, shared config files, IMDS / IRSA / Workload Identity / managed identity.
Explicit options are overrides, not requirements. Endpoint override supported (MinIO, fake-gcs, Azurite).

### 10.4 Remote commit protocol
Remote layout:
```
<prefix>/
  manifests/<seq:020>.mf        immutable, created with put_if_absent
  segments/<collection>/<segment_id>.seg
  tombstones/<collection>/<segment_id>.<ver>.tomb
  wal/<seq:020>.wal
  blobs/...
  LEASE                          writer lease (SPEC §11.3)
```
1. Upload new segments/WAL objects (any order, immutable, content-named).
2. **Commit = `put_if_absent(manifests/<seq+1>.mf)`**. Success ⇒ committed. Conflict ⇒ another writer won ⇒ fence/abort.
3. Readers find the latest manifest by `list` (or a best-effort `CURRENT` hint) and may tail for newer ones.

### 10.5 Local-first replication ("replicated" mode)
`db.replicate("s3://…")`: local file is the primary; a background replicator uploads committed segments
and then manifests in order, tracks the **replicated seq**, and is resumable. Local writes never block on the network.
Remote readers can open the replica read-only (eventually consistent, lag exposed as a metric).

### 10.6 Read path and cache
Remote reads go through a **block cache** (RAM + local NVMe, LRU/ARC, keyed by `(object, block)`), with:
range-request coalescing, readahead for sequential sections, request hedging (optional), and singleflight to
dedup concurrent identical fetches. Segment footers and FSTs are pinned on open.

---

## 11. Distribution

### 11.1 Model
A **cluster** is a set of **shards** — each shard is an independent Database (own manifest chain) under
`<prefix>/shard-<n>/`. Documents route by `hash(id) mod shard_count` (xxh3). `shard_count` is fixed at collection-set
creation in v1; resharding (split) is a post-v1 operation designed around segment immutability.

### 11.2 Roles (all in `persia-server`, selected by flags/env)
- **writer**: owns one shard's lease, accepts writes, runs compaction, uploads segments.
- **reader**: serves queries from committed manifests; stateless; scales via HPA; warms cache from object storage.
- **router/coordinator** (may be co-located): scatter-gather queries across shards; merges top-k and facets.

### 11.3 Writer lease and fencing
Lease object `LEASE` = `{holder_id, fence_token, expires_at}` renewed periodically via conditional update.
Every manifest embeds `fence_token`. A writer whose lease lapsed MUST stop writing; a stale writer's manifests
are rejected by readers (token regression) and cannot win `put_if_absent` once a newer seq exists.

### 11.4 Replica refresh
Readers poll for the next manifest seq (default 1 s, with jitter) and swap snapshots atomically.
Staleness is exposed as `persia_replica_lag_seconds`.

### 11.5 Cross-shard scoring
Default: shard-local statistics (fast, slightly inexact). Option `global_stats: true`: router gathers `(N, df, avglen)`
per query term from shards in a pre-phase (DFS-style) before scoring.

### 11.6 Kubernetes
Shipped artifacts: OCI image (distroless, non-root, read-only rootfs), Helm chart:
`StatefulSet` for writers (one pod per shard, PVC for cache/WAL), `Deployment` + `HPA` (QPS/CPU) for readers,
headless Service for shard discovery, PodDisruptionBudgets, readiness/liveness (`/readyz`, `/livez`),
graceful shutdown (drain, flush, release lease), `ServiceMonitor`.

---

## 12. Server and API

### 12.1 Transports
gRPC (tonic) is primary; HTTP/JSON (axum) mirrors it for tooling. Both generated from/aligned with `persia.proto`.

### 12.2 Services (sketch)
```proto
service Persia {
  rpc CreateCollection(CreateCollectionRequest) returns (Collection);
  rpc Put(stream PutRequest) returns (PutResponse);        // docs, optional blob chunks
  rpc Delete(DeleteRequest) returns (DeleteResponse);
  rpc Get(GetRequest) returns (Document);
  rpc Search(SearchRequest) returns (SearchResponse);     // includes relaxation info
  rpc GetBlob(GetBlobRequest) returns (stream BlobChunk); // supports ranges
  rpc Commit(CommitRequest) returns (CommitResponse);
  rpc Stats(StatsRequest) returns (StatsResponse);
}
```
Errors map to canonical gRPC codes; all responses carry `manifest_seq` for read-your-writes via `min_seq` on requests.

### 12.3 Auth and transport security
TLS (rustls) optional-on by config; API keys / bearer tokens in v1; mTLS supported. No auth in embedded mode.

### 12.4 Observability
`tracing` spans + OpenTelemetry export; Prometheus `/metrics`
(query latency histograms by relaxation level, cache hit ratio, segments/collection, merge backlog, WAL bytes,
replica lag, blob throughput). Structured JSON logs.

---

## 13. SDKs

- **Rust** (`sdk/rust`, crate `persia-client`) and **Go** (`sdk/go`, module `persia-go`).
- Generated stubs from `persia-proto` + thin idiomatic layer: connection pooling, client-side load balancing
  (shard-aware for writes), retries with backoff on idempotent ops, deadlines, batching, blob streaming helpers.
- A **shared conformance suite** (language-neutral scenarios in YAML/JSON) runs against both SDKs and the server in CI.
- Semantic versioning; SDK ↔ server compatibility matrix documented.

---

## 14. Configuration

Embedded: builder options (`OpenOptions`), sensible defaults, no files.
Server: flags → env (`PERSIA_*`) → optional TOML. Defaults MUST be production-safe.
Key knobs: `memtable_bytes`, `commit_interval`, `fsync_policy`, `merge_threads`, `search_threads`,
`cache_ram_bytes`, `cache_disk_path/bytes`, `block_size`, `blob_chunk_size`, `relax.*`.

---

## 15. Versioning and compatibility

- `format_major` bump = breaking; readers reject. `format_minor` bump = additive.
- `incompat_flags` for features old readers cannot handle; `rocompat_flags` for read-only-safe features.
- Each segment records its own format version ⇒ mixed-version databases are valid; compaction upgrades gradually.
- Golden files for every released format version live in `tests/fixtures/` and are opened by CI forever.
- Until v1.0: no compatibility promise, but every change still bumps the version and updates fixtures.

---

## 16. Quality requirements

### 16.1 Testing (normative)
- Unit + property tests for every primitive and round-trip.
- Fuzz targets for: frame reader, superblock, segment footer, each section decoder, query parser, WAL replay.
- **Differential testing**: random operation sequences (put/delete/commit/merge/reopen/crash) executed against the engine and the
  reference model; results (hit sets, scores within epsilon) MUST match.
- **Crash testing**: fault-injecting `Storage` (torn writes, dropped fsync, partial uploads, reordering) and `kill -9` loops.
- Integration against MinIO, fake-gcs-server, Azurite via docker-compose.
- Synthetic load + soak tests, relevance regression suite (fixed corpora + judged queries; nDCG@10 tracked).

### 16.2 Performance (targets, to be validated by benchmarks — not promises)
| Metric | Target |
|---|---|
| Query p50 (warm, 10M short docs, 1 node) | < 10 ms |
| Query p99 (warm) | < 50 ms |
| Cold namespace first query (object storage) | < 500 ms |
| Indexing throughput (1 writer) | ≥ 50k docs/s (short docs) |
| Open time (1 GB db, local) | < 100 ms |
| Recovery after crash | proportional to WAL size, < 5 s for 64 MiB WAL |

### 16.3 Benchmarks
Reproducible harness (`benches/`, dataset download scripts, pinned configs), tracked in CI for regressions (> 5% fails).
Comparison against other engines is reported with methodology, never cherry-picked.

---

## 17. Security considerations

- Treat every byte read from disk/network as hostile (fuzzing; bounds-checked parsing; decompression-bomb limits).
- Limits: max doc size, max field count, max query terms, max fuzzy expansions, max result window.
- Path traversal safe (collection/blob names validated); no shelling out.
- Secrets never logged; redaction in debug output of storage config.
- Supply chain: `cargo deny`, `cargo audit`, pinned toolchain, reproducible container builds, SBOM.

---

## 18. Open questions (track as ADRs)

1. ~~Blob hash~~: resolved by ADR-0002, SHA-256 (§9.1).
2. Single blob per document vs many (`blobs: []`) in v1.
3. Tombstone sidecar format vs copy-on-write segment footers. Current proposal: full-set sidecars (SPEC §8.3).
4. Positions: always-on vs opt-in (current: opt-in).
5. Resharding strategy (split-by-hash-range using immutable segments + filtering merge).
6. Vector search layout (IVF-style clusters in segments) — reserve `incompat_flags` bit.
7. Language analyzers scope for v1 (en, es, pt first).
8. Blob upload staging: the id is known only after hashing the last byte, but §9.3/§9.4 key chunks by `blob_id`.
   Proposal to evaluate: stage chunks under an upload id, then bind them to the verified `blob_id` with the
   `BlobIndex` frame (local) or a per-blob index object written with `put_if_absent` (remote); dedup check after
   hashing; resumable uploads re-hash stored chunks or persist hasher state. Also: how an expected id is passed
   (API/gRPC). Must be decided (ADR) before ROADMAP 9.2.
