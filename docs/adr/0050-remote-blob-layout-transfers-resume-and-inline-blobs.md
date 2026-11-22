# ADR-0050: Remote blob layout: keys, parallel transfers, resume, inline small blobs, object times

- Status: accepted
- Date: 2026-11-18
- Related: SPEC §9.1.1, §9.2, §9.4, §9.5, §10.1, §17; ROADMAP 9.3.1, 9.3.2, 9.3.3, 9.5.2, 9.5.5; ADR-0002,
  ADR-0034, ADR-0035, ADR-0041, ADR-0012

## Context

ADR-0034 fixed the remote layout's keys (`blobs/<xx>/<id>` for blob manifests, `blobs/uploads/<upload_id>/<chunk_idx>`
for staged chunks), binding by `put_if_absent`, and a resume that re-hashes the staged chunks. ADR-0035 reserved the
blob manifest's `flags` for an inline small-blob form (ROADMAP 9.3.3). What was left open: how the store maps
`Storage` answers and errors onto the `BlobStore` contract (`crates/persia-blob/src/store.rs`), how far a transfer
runs in parallel, the resume primitives, the inline layout, and how the GC sweep (ROADMAP 9.5.2) learns an object's age
for its grace period (SPEC §9.5).

## Decision

### 1. Keys and their parsers (ROADMAP 9.3.1)

`RemoteKey` (`crates/persia-blob/src/remote/keys.rs`) is `Manifest(blob_id)` or `Chunk { upload_id, index }`.
`RemoteKey::parse` accepts exactly the text `RemoteKey::key` writes: `blobs/<xx>/<64 lowercase hex>` whose `<xx>` is
the id's first byte, and `blobs/uploads/<32 lowercase hex>/<10 decimal digits ≤ u32::MAX>`; any other key (a shared
bucket's, an uppercase or shorter variant, a deeper path) is `None`. A sweep lists `blobs/<xx>/` per prefix
(`manifest_prefix`) and `blobs/uploads/` (`uploads_prefix`) and parses every key, so a key Persia did not write is
never deleted or taken for a blob. `RemoteBlobStore::staged_chunks(upload_id)` lists one upload's chunks with their
lengths (and `last_modified`, item 6), and also drops parsed keys of another upload, in case a backend's listing
ignores the prefix.

### 2. Mapping the `Storage` answers (ROADMAP 9.3.1)

- `stage_chunk`: `put_if_absent` of the raw bytes. `Created` is `Ok`; `AlreadyExists` (resolved by the backend
  against our own bytes, SPEC §10.1) is `StagedChunkConflict`.
- `bind`: `put_if_absent` of the encoded manifest. `Created` is `Bound`. `AlreadyExists` gets the bound manifest,
  decodes it and checks it names the id (`check_blob_id`), then answers `AlreadyBound`. If that manifest is gone
  before the `get` (a sweep deleted it in between), the outcome is known (not ours), so the `put_if_absent` is tried
  again, at most 3 times in all. Any error of `put_if_absent` is `BindOutcomeUnknown`, except `Unsupported`.
- Without `Capabilities::conditional_put`, staging and binding fail with `Error::ConditionalPutUnsupported` before any
  request; reads need no capability, so a read-only backend still serves blobs.
- `manifest`: `get`; `NotFound` is `None`. `read_chunk`: `get` of the chunk object; `NotFound` is `Corrupt` (a bound
  manifest names it), and an index past the last chunk is `Corrupt`. The callers check the length and the CRC32C
  centrally (`read_chunk_checked`, `read_range`).
- `discard_upload` deletes every chunk `staged_chunks` lists, tries them all, and reports the first error.
- A backend error of kind `Corrupt` (e.g. `ShortRead`) becomes `Error::Corrupt` (encoding `"blob storage object"`),
  so `is_corruption` holds. Every other backend error is `Error::Storage(source)`.

### 3. Parallel transfers (ROADMAP 9.3.2)

`stage_chunks` and `read_chunks` run on `std::thread::scope`. They use at most `parallelism` threads, each with one
request in flight (`RemoteBlobStore::with_parallelism`, default `DEFAULT_PARALLELISM` = 8). With 1 they run in the
caller's thread. Workers take the next index from a shared counter. After a failure no new index is handed out, so a
failed transfer stops early. Results go back in index order, and the error reported is that of the lowest failing
index. Every index below the first failure was handed out before it, so that error does not depend on scheduling. No
dependency is added (`rayon` would also serve, but a pool shared with the engine's merges would mix I/O waits into
CPU work). Memory stays the caller's: `stage_chunks` takes the slices the caller passes (it bounds how many), and
`read_chunks` returns the chunks `read_range` asked for.

### 4. Resume (ROADMAP 9.3.2)

- `ResumeToken { upload_id, chunk_size }` (SPEC §9.4). `RemoteBlobStore::resume(token)` lists the upload, keeps the
  run of chunks `0, 1, …` whose listed length is `chunk_size`, and reads them in windows of `parallelism`. A chunk
  read back at another length, or missing, ends the run there. The bytes go to a `ContentHasher` in order, and each
  chunk's CRC32C is recorded. The result, `ResumedUpload`, has `next_index`, `offset()` (`next_index · chunk_size`,
  where the caller's reader must start), the CRCs and the hasher. The hasher's state is never persisted (ADR-0034
  choice C).
- `Chunker::resume(reader, chunk_size, hasher, next_index)` (`chunker.rs`) continues the cut at chunk `next_index`, at
  offset `next_index · chunk_size`. It requires `hasher.len()` to equal that offset (every earlier chunk is full),
  else it fails with `Error::InvalidResume`. `ResumedUpload::into_chunker(reader)` builds one and hands back the
  stored CRCs, which the caller extends with the new chunks' CRCs for the manifest. The id is the SHA-256 of the
  stored bytes followed by the reader's, so a resuming caller passes `expected_id` (SPEC §9.6).
- If a chunk write answers `StagedChunkConflict` (e.g. the short last chunk of an upload that was already bound), the
  caller restarts under a new upload id: `RemoteBlobStore::restage(token, chunk_crcs, new_upload_id)` copies chunks
  `0..chunk_crcs.len()` (the run kept at resume plus the chunks staged since, all full) under the new id, re-reading
  each one and checking its length and its CRC against `chunk_crcs` (one that changed or vanished is `Corrupt`).
  The caller then stages the conflicting chunk and the rest under the new id, so the client never needs to rewind.
  The new upload id is pinned before `restage` (SPEC §9.5), and the old one stays pinned until the put ends.
- Who drives it: `put_blob` (ROADMAP 9.4.2) does not take a token yet. A later leaf adds
  `PutBlobOptions::resume: Option<ResumeToken>` and the loop that `crates/persia-blob/tests/remote_resume.rs`
  `put_resumable` implements against these primitives.

### 5. Inline small blobs (ROADMAP 9.3.3): manifest flag bit 0

Flag bit 0 (`INLINE = 0x1`) means that the blob's bytes are inside its blob manifest and nothing was staged.

```
0   4    magic       "PBLM"
4   4    version     u32 = 1
8   4    flags       u32 = 0x1 (INLINE)
12  32   blob_id
44  8    size        u64, 1..=chunk_size: exactly one chunk
52  4    chunk_size  u32, 1..=64 MiB
56  16   upload_id   u128 = 0: no upload
72  4    chunk_crc   u32, the CRC32C of the bytes (n = 1)
76  size bytes       the blob's bytes
..  4    crc32c      u32 over every byte before it
```

- The length is `80 + size`. The CRC list keeps its one entry, so `chunk_count()` is 1, `chunk_crcs()` holds the
  bytes' CRC, and `read_range`'s length and CRC checks hold unchanged. `read_chunk(m, 0)` returns the inline bytes
  without a request.
- The encoding's rule is "exactly one chunk" (`1 ≤ size ≤ chunk_size`). The 64 KiB threshold (`SMALL_BLOB_MAX`, the
  chunk-size floor) is the writer's rule, as the floor itself is (ADR-0041). So a reader accepts inline blobs of up to
  64 MiB, and the threshold can move without a format change. The empty blob is never inline: its plain manifest
  (no chunks) is already one object with nothing staged, so the value has one encoding.
- `upload_id` is 0, so two writers of the same small blob in the same chunk size write equal bytes. The second
  `put_if_absent` then resolves to `Created` (SPEC §10.1), as idempotent as a retry. The GC mark (ROADMAP 9.5.1)
  takes no upload id from an inline manifest.
- Validation order after `flags`: `chunk_size` in range, `n` fits a `u32`, then for an inline manifest: `n` is 1
  (else `Corrupt`), `upload_id` is 0, exactly `4 + size` bytes after the header before the trailer (checked before
  allocating), and the chunk CRC equals the CRC32C of the bytes. A bit other than bit 0 is still `UnsupportedFlags`,
  so a v1 reader without this decision, which knew no bit, reports an inline manifest as unsupported, not corrupt.
- Decoding does not hash the bytes against `blob_id`, as for chunked blobs: `check_blob_id` and `check_ref` catch a
  manifest under the wrong key, the trailer and the CRC catch damage, and `verify` (ROADMAP 9.5.3) re-hashes.
- `RemoteBlobStore::bind_small` writes the inline manifest with one `put_if_absent` (one round trip). It writes the
  plain manifest for the empty blob, and answers like `bind`. It checks, as the default does, that the bytes are the
  caller's one-chunk manifest's and at most `SMALL_BLOB_MAX`.
- Golden files: `tests/fixtures/blob-manifest/v1/inline-one.blm` (1 byte) and `inline-max.blm` (64 KiB in 64 KiB
  chunks, the largest a writer produces).

### 6. `ObjectMeta::last_modified` (ROADMAP 9.5.5)

`ObjectMeta` gains `last_modified: Option<SystemTime>`: when the object's current bytes were written, by the backend's
clock. A sweep compares it with its own clock, and the 24 h grace period dwarfs any skew.

- Memory: the time of the `put` or `put_if_absent` that created the bytes, read from the store's `Clock`
  (`MemoryStorage::with_clock`; `Clock::now` is a provided method, `SystemTime::now()` by default; `ManualClock`
  makes tests deterministic). A `put_if_absent` that resolves to `Created` on equal bytes keeps the old time, since
  nothing was written.
- Local: the file's modification time (`EntryKind::File::modified`, from the same `stat` that gives the length), or
  `None` where the platform has none. A copy that does not preserve times makes objects look newer, which only delays
  a sweep.
- `SimFs` (crash tests) and the container adapter (`persia-engine`'s `ContainerObjects`, whose objects are frames
  of one file) report `None`. Cloud backends (not written yet) report the provider's `Last-Modified`.
- `None` means unknown. A sweep must never take an unknown age for an expired one: it keeps such objects, or ages
  them from when it first saw them. That choice is 9.5.2's.

## Consequences

- A blob of at most 64 KiB costs one object and one request to write and to read (the manifest `get` is the read).
  Larger blobs cost `1 + n` objects, written up to 8 at a time.
- A resume re-reads what was staged once (ADR-0034 Consequences). A restart re-writes it under the new id without the
  client rewinding.
- Adding a field to `ObjectMeta` breaks code that builds it with a struct literal: the container adapter in
  `persia-engine` is updated in the same change.
- The fuzz target `blob_manifest` reaches the inline branch through its resealed mode, unchanged.

## Alternatives considered

- **Inline with the caller's upload id**: traceable, but two writers of the same small blob conflict
  (`AlreadyExists`) where equal bytes could simply both succeed. And the mark would take an upload id from a
  manifest that has none.
- **Inline without the CRC entry** (`n` = 0): saves 4 bytes, but `chunk_count()` would be 0 for a non-empty blob,
  and every caller of `chunk_crcs` would need a special case.
- **The 64 KiB threshold as a format rule**: a reader would reject larger inline blobs, and moving the threshold
  would be a format change. Rejected as for the chunk-size floor (ADR-0041).
- **`rayon` for transfers**: a dependency that is already allowed, but a global pool sized for CPU work. Scoped
  threads bound in-flight requests per store, with no shared state.
- **`last_modified` as `u64` milliseconds**: no less precise for a 24 h grace period, but `SystemTime` is what
  `std::fs::Metadata` and the object stores' SDKs give, and it carries its epoch.
