# ADR-0034: Blob upload staging, binding, and the expected-id API

- Status: accepted
- Date: 2026-11-07
- Related: SPEC §9.1–§9.6, §10.1, §10.4, §18.8; ROADMAP 9.1.0–9.1.3, 9.2, 9.3, 9.5; ADR-0002, ADR-0011, ADR-0024

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
2. **Stage under an upload id, never under a `blob_id`.** Each upload draws a random 128-bit `upload_id`. Chunks are
   written as `(upload_id, chunk_idx)`: locally as `BlobChunk {upload_id, chunk_idx, len, crc, bytes}` frames in the
   pack (§9.3), remotely as `blobs/uploads/<upload_id:032x>/<chunk_idx:010>` objects holding the raw chunk bytes
   (§9.4). Chunks stay there for life: there is no rename or copy step.
3. **Bind with a blob manifest, written only after the id is verified.** The *blob manifest* is the blob's chunk
   list: `blob_id`, `size`, `chunk_size`, `upload_id`, one CRC32C per chunk, and a CRC32C trailer of its own (a remote
   one is not inside a frame, like the manifest and tombstone sidecars, ADR-0024/ADR-0030). Byte layout: ROADMAP
   9.1.3 (SPEC §9.1.1). A blob exists, and is addressable by its id, only once its blob manifest is durable:
   - locally inside a `BlobIndex` frame of the pack, published by the pack's superblock commit (§9.3, ROADMAP 9.2);
   - remotely as `blobs/<hex[0..2]>/<hex(blob_id)>`, written with `put_if_absent`. `Created` binds it. `AlreadyExists`
     (already resolved against our own bytes, §10.1) means another upload bound the id first: the blob is
     deduplicated, our staged chunks are orphans. An error leaves the outcome unknown: keep the chunks and read the
     key to resolve it, as the commit protocol does (§10.4). A backend without `conditional_put` cannot store
     blobs (`Unsupported`), the same requirement as commits.
4. **Dedup after hashing.** When the id is known: if it is already bound, the upload's staged chunks are deleted
   best-effort (the GC sweep is the backstop) and `put_blob` returns a `BlobRef` with the **bound** blob's
   `chunk_size` (two uploads of the same bytes with different chunk sizes are one blob). Dedup never skips reading
   the bytes, expected id or not: a caller proves it has a blob by sending it.
5. **Expected id.** `put_blob(reader, &PutBlobOptions { mime, user_meta, expected_id })`. A supplied
   `expected_id` is checked against the computed id when the reader ends; a mismatch is
   `BlobIdMismatch { expected, actual }` (a caller error, not corruption) and nothing is bound; staged chunks are
   deleted best-effort. gRPC: an optional `expected_blob_id` (32 bytes) in the first message of the `PutBlob` stream;
   HTTP: the standard `Repr-Digest: sha-256=:<base64>:` header (RFC 9530). Both arrive in ROADMAP 11.
6. **Resume (remote, ROADMAP 9.3.2) re-hashes.** A resumed upload names its `upload_id`; the writer lists the staged
   chunks, takes the contiguous run from chunk 0 (a full-length object per index; anything after a gap is
   rewritten), feeds their bytes to a fresh hasher, and continues from the next offset. The hasher's internal state
   is never persisted.
7. **GC (§9.5).** Mark: the `blob_id`s referenced by live manifests and snapshots, then their blob manifests' upload
   ids. Sweep: blob manifests not marked, and staged chunks whose upload id no marked manifest names, once older than
   the grace period (from the newest chunk's `last_modified`, ROADMAP 9.5.5), so an idle upload stays resumable for
   that long. Locally the sweep is the pack rewrite.
8. **Small blobs (ROADMAP 9.3.3).** A blob that fits under the threshold is buffered and hashed before any write, so
   it needs no staging: it can be written straight under its verified id. To leave room for that, the blob manifest
   has a `flags` byte, all bits reserved and zero in v1; 9.3.3 assigns one with a SPEC update.

## Consequences

- No byte is ever reachable under a `blob_id` that Persia did not compute, so dedup is safe with untrusted clients.
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
