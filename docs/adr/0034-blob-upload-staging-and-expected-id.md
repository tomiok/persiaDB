# ADR-0034: Blob upload staging, binding, and the expected-id API

- Status: accepted; amended 2026-11-07 after review (items 2–4, 6, 7: chunks never overwritten, unknown binding
  outcomes, read checks, GC pins)
- Date: 2026-11-07
- Related: SPEC §9.1–§9.6, §10.1, §10.4, §18.8; ROADMAP 9.1.0–9.1.3, 9.2, 9.3, 9.5; ADR-0002, ADR-0011, ADR-0024, ADR-0035

## Context

A `blob_id` is the SHA-256 of the blob's bytes, computed by Persia while it reads them (ADR-0002), so it is known
only after the last byte. SPEC §9.3 and §9.4 key chunks by `blob_id` (`BlobChunk {blob_id, …}` frames,
`blobs/<xx>/<id>/<chunk_idx>` objects), which a streaming writer cannot do without either buffering the whole blob
or making chunks addressable under an id nobody has verified yet. The latter lets a client that claims an id it
does not send (or sends wrong bytes for) plant chunks that a reader of the real blob would serve, which is the
aliasing attack ADR-0002 exists to prevent. SPEC §18.8 asks for a staging scheme, a dedup point, a resume story
and the shape of the "expected id" parameter before ROADMAP 9.2.

Constraints: object stores have no rename, and the `Storage` trait has no copy (§10.1); `put_if_absent` is the
only atomic primitive; a blob can be larger than memory; uploads from servers can be cut and resumed (ROADMAP
9.3.2); GC must never delete a referenced blob (§9.5).

## Decision

1. **Fixed-size chunking.** Chunk `i` is bytes `[i·chunk_size, min((i+1)·chunk_size, size))`; every chunk but the
   last is full; an empty blob has no chunks. Boundaries depend only on `size` and `chunk_size`, never on how the
   reader delivers bytes. The store's `chunk_size` defaults to 4 MiB (SPEC §9.2, range `1..=64 MiB`, §9.1).
2. **Stage under an upload id, never under a `blob_id`.** Each upload draws a random 128-bit `upload_id` (like a
   `segment_id`: unique, not secret; integrity never depends on it, item 6). Chunks are written as
   `(upload_id, chunk_idx)`: locally as `BlobChunk {upload_id, chunk_idx, len, crc, bytes}` frames in the pack (§9.3),
   remotely as `blobs/uploads/<upload_id:032x>/<chunk_idx:010>` objects holding the raw chunk bytes, created with
   `put_if_absent` (§9.4). A staged chunk is never overwritten, and stays there for life: there is no rename or copy
   step.
3. **Bind with a blob manifest, written only after the id is verified** and every chunk write is acknowledged. The
   *blob manifest* is the blob's chunk list: `blob_id`, `size`, `chunk_size`, `upload_id`, one CRC32C per chunk, and a
   CRC32C trailer of its own (a remote one is not inside a frame, like the manifest and tombstone sidecars,
   ADR-0024/ADR-0030). Byte layout: ADR-0035 (SPEC §9.1.1). A blob exists, and is addressable by its id, only once
   its blob manifest is durable:
   - locally inside a `BlobIndex` frame of the pack, published by the pack's superblock commit (§9.3, ROADMAP 9.2);
   - remotely as `blobs/<hex[0..2]>/<hex(blob_id)>`, written with `put_if_absent`. `Created` binds it. `AlreadyExists`
     (already resolved against our own bytes, §10.1) means another upload bound the id first: the blob is
     deduplicated, our staged chunks are orphans. An error leaves the outcome unknown: retry the same `put_if_absent`
     until it answers, and never delete the chunks of an upload whose outcome was once unknown (a conditional write
     still in flight may land later); GC reclaims them. A backend without `conditional_put` cannot store blobs
     (`Unsupported`), the same requirement as commits.
   A reader checks the manifest against what it looked it up by (`check_ref`: id, size, chunk size) before serving
   chunks: the manifest's CRCs cover only its own bytes, so a manifest under the wrong key would otherwise serve
   another blob's bytes.
4. **Dedup after hashing.** When the id is known: if it is already bound, `put_blob` decodes the bound manifest and
   checks it names the id (one that does not is `Corrupt`, reported by `verify`, never reused), then returns a
   `BlobRef` with the **bound** blob's `chunk_size` (two uploads of the same bytes with different chunk sizes are one
   blob). The upload's staged chunks are deleted best-effort remotely (the GC sweep is the backstop) and left to the
   pack rewrite locally. Dedup never skips reading the bytes, expected id or not: a caller proves it has a blob by
   sending it.
5. **Expected id.** `put_blob(reader, &PutBlobOptions { mime, user_meta, expected_id })`. A supplied
   `expected_id` is checked against the computed id when the reader ends; a mismatch is
   `BlobIdMismatch { expected, actual }` (a caller error, not corruption) and nothing is bound; staged chunks are
   deleted best-effort. gRPC: an optional `expected_blob_id` (32 bytes) in the first message of the `PutBlob` stream;
   HTTP: the standard `Repr-Digest: sha-256=:<base64>:` header (RFC 9530). Both arrive in ROADMAP 11.
6. **Resume (remote, ROADMAP 9.3.2) re-hashes.** The resume token is the `upload_id` and the upload's `chunk_size`
   (chunk objects do not record it). The writer lists the staged chunks, takes the contiguous run of full-length
   chunks from chunk 0, feeds their bytes to a fresh hasher, and continues from the next offset. Chunk writes are
   `put_if_absent`, so a chunk already there with other bytes (e.g. the short last chunk of an upload already bound)
   is never replaced: the resume restarts under a new `upload_id`. The hasher's internal state is never persisted.
   The id is computed from the stored bytes, so whoever knows an upload id can change what a resume produces, never
   what an id names; a resuming client should pass `expected_id`.
7. **GC (§9.5) runs in the writer, beside `put_blob`, with a pin set.** `put_blob` pins its upload id before its first
   chunk write (or listing, on resume) and the `blob_id` it returns before binding or looking it up, until the
   document referencing it is durable (WAL or commit) or the upload is dropped. Mark: the `blob_id`s referenced by
   live manifests and snapshots, by WAL records from `wal_start`, and pinned; then the upload ids their blob manifests
   name, and pinned upload ids. Sweep: unmarked blob manifests, then staged chunks whose upload id no marked manifest
   names, once older than the grace period (from the newest chunk's `last_modified`, ROADMAP 9.5.5), each pin check
   and delete one step under the pin set's lock. Manifests go before their chunks, so a crash mid-sweep never leaves
   a manifest without chunks. The grace period is for a crashed or replaced writer's uploads; the pins are what
   protects a blob a not-yet-durable document references (without them, deduplicating onto an orphan older than the
   grace period would hand out a `BlobRef` GC is about to delete). Locally the sweep is the pack rewrite.
8. **Small blobs (ROADMAP 9.3.3).** A blob that fits under the threshold is buffered and hashed before any write, so
   it needs no staging: it can be written straight under its verified id. To leave room for that, the blob manifest
   has a `flags` field (`u32`), all bits reserved and zero in v1; 9.3.3 assigns one with a SPEC update.

## Consequences

- No byte is ever reachable under a `blob_id` that Persia did not compute (chunks are never overwritten, and readers
  check the manifest against the id), so dedup is safe with untrusted clients.
- GC must run in the writer process: a separate GC process would not see `put_blob`'s pins.
- A blob costs one more small object remotely (the manifest), and a read costs one more `get` before the chunks; the
  manifest is immutable and cacheable, so the blob reader keeps it with the open blob.
- Every upload writes its bytes, even a duplicate; dedup saves storage, not ingress. A future "upload only if
  missing" API would need a possession proof and its own ADR.
- Resume re-reads what was already uploaded (network egress, once per resume); acceptable for resumes, which are rare.
- Range reads fetch whole chunks to check their CRC (≤ `chunk_size` extra per end of the range); remote chunk objects
  are raw bytes, so a sub-range maps to one `get_range` per chunk.
- SPEC §9.3, §9.4, §9.5, §9.6 change; §18.8 is closed.

## Alternatives considered

- **Spool, then write under the id.** Buffer the whole blob to local disk, hash, then upload under `blob_id`. Simple
  keys, but writers need scratch disk the size of the largest blob and every byte is written twice.
- **Content-address each chunk** (`blobs/chunks/<sha256(chunk)>`). Chunk-level dedup and natural resumability, but a
  second SHA-256 pass over every byte (half the ingest throughput, ADR-0002 Consequences), and fixed-size chunks
  rarely dedup across different blobs anyway.
- **Copy staged chunks to `blobs/<id>/…` after hashing.** Keeps the SPEC's keys but doubles writes, needs a copy
  operation the `Storage` trait does not have, and leaves a window where a half-copied blob is visible.
- **Persist the SHA-256 state for resume.** Saves the re-read, but depends on the hasher's serialization staying
  stable across `sha2` versions; can be added later behind the same resume API.
- **Trust the expected id for dedup** (skip the upload when it is already bound). Saves ingress, but lets anyone who
  learns an id reference a blob they never had (the proof-of-possession problem of client-side dedup).

## Choices made (2026-11-07)

| # | Choice | Options | Decision |
|---|---|---|---|
| A | Staging scheme | upload-id + bind (2–3) · local spool · per-chunk hash · copy | **upload-id + bind** |
| B | Expected id short-circuits dedup | no · yes | **no** (item 4; a possession proof would need its own ADR) |
| C | Resume | re-hash stored chunks · persist hasher state | **re-hash** |
| D | Remote chunk objects | raw bytes · framed (`PFR1`) with CRC | **raw** (CRC lives in the blob manifest; sub-ranges map directly) |
| E | Blob manifest integrity | own CRC32C trailer · rely on a frame | **own trailer** (remote objects are not frames) |
| F | Remote manifest key | `blobs/<xx>/<id>` · `blobs/<xx>/<id>/manifest` · `.bm` suffix | **`blobs/<xx>/<id>`** (one key per blob, also for 9.3.3's inline form) |
| G | HTTP expected-id header | `Repr-Digest` (RFC 9530, base64) · `x-persia-blob-id` (hex) | **`Repr-Digest`** |
| H | Backends without `conditional_put` | `Unsupported` · last-writer-wins `put` | **`Unsupported`** |
