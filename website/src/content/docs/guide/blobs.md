---
title: "Blob storage"
description: "Store files with documents in PersiaDB, an embedded Rust search database: put_blob, the local pack file, remote object keys, and garbage collection."
head:
  - tag: title
    content: "Store files with documents in an embedded Rust database | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Store files with documents in an embedded Rust database | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Store files with documents in an embedded Rust database | PersiaDB"
---

Persia DB can store a file next to the document that describes it: a PDF, an image, or any other bytes. The search index keeps a reference. The bytes live in a content-addressed blob store ([SPEC §9](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#9-blobs)).

That store is library code on `main`. It is not `persia::Db`. The public facade is milestone M10 and is not built. %%BLOB_NOW%% Live counts are [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md). The crates are unpublished (`publish = false`). There is no `cargo add persia`.

The calls below are `persia::internal` (`#[doc(hidden)]` in [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs)). [ADR-0051](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0051-blobs-with-documents-internal-api-pins-and-ordering.md) says that module may change until `Db` and `Collection` wrap it. [Getting started](/guide/getting-started/) separates that library code from the planned `Db::open`. [Embedded API](/guide/embedded-api/) is the planned surface.

## What a blob is

A blob id is the SHA-256 of the bytes ([`BlobId`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/blob.rs), [ADR-0002](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0002-blob-hash-sha256.md)). Persia hashes what it reads. `PutBlobOptions.expected_id` that does not match is `Error::BlobIdMismatch`, and nothing is bound.

A [`BlobRef`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/blob.rs) is `blob_id`, `size`, `mime`, `chunk_size`, and optional `user_meta` (SPEC §9.1, [ADR-0011](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0011-blob-ref-encoding.md)). `mime` defaults to `application/octet-stream` (`DEFAULT_MIME`). `user_meta` is at most 32 entries. Searchable metadata belongs in document fields.

The same bytes are one blob. A second store of those bytes binds the id that is already there (`BindOutcome::AlreadyBound`) and discards the new upload's chunks best-effort.

Writers cut the bytes into fixed-size chunks. The default chunk size is 4 MiB (`ChunkSize::DEFAULT`). A writer uses 64 KiB through 64 MiB (`ChunkSize::MIN`..=`ChunkSize::MAX`). The on-disk encoding still accepts a chunk size from 1 byte, so an old manifest stays readable ([ADR-0041](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0041-blob-chunk-size-floor.md)). Each chunk carries a CRC32C.

A blob of at most 64 KiB (`SMALL_BLOB_MAX`) is hashed whole and bound in one step (`BlobStore::bind_small`). On the remote layout that is one inline manifest object. A larger blob is staged a window at a time (`WINDOW_BYTES` is 32 MiB, and at most `WINDOW_MAX_CHUNKS`, 64).

## The calls that exist

These signatures are in the tree today.

| Call | Where | What it does |
|---|---|---|
| `Blobs::put_blob(&self, reader: impl Read, options: &PutBlobOptions) -> Result<(BlobRef, Pins)>` | [`persia::internal::blob`](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/internal/blob.rs) | Reads `reader`, stages chunks, binds the id. |
| `Blobs::get_blob(&self, blob_ref: &BlobRef) -> Result<BlobReader>` | same | A `std::io::Read` of the whole blob, one window of chunks at a time. |
| `Blobs::get_blob_range(&self, blob_ref: &BlobRef, range: Range<u64>) -> Result<Vec<u8>>` | same | One byte range, every chunk checked. |
| `BlobDatabase::put(&mut self, collection: &str, doc: &Document, blob: Blob, durability: Durability) -> Result<BlobRef>` | [`persia::internal::documents`](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/internal/documents.rs) | Stores the blob, then logs the document with that `BlobRef`. |
| `BlobDatabase::put_blob`, `get_blob`, `get_blob_range` | same | The `Blobs` calls, on the database handle. |
| `BlobDatabase::gc(&self, options: &GcOptions) -> Result<GcReport>` | [`persia::internal::gc`](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/internal/gc.rs) | Mark, then sweep. Nothing calls this in the background. |

`Blob` is `Reader(reader, options)`, `Path(path, options)`, or `Stored(blob_ref)`. `Blob::from_reader` and `Blob::from_path` use `PutBlobOptions::default()`.

`PutBlobOptions` fields: `mime`, `user_meta`, `expected_id`, `chunk_size` (default 4 MiB), `bind_budget` (default 60 seconds, `DEFAULT_BIND_BUDGET`, at most one hour, `MAX_BIND_BUDGET`). There is no resume field. `RemoteBlobStore::resume` exists; `put_blob` does not take its token yet (ROADMAP 9.3.4).

A search hit's stored document carries the reference: `SearchHit.document.blob_ref()` ([`SearchHit`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db/search.rs)). The hit does not carry the file bytes. Each segment that has a blob stores a `BlobRefs` section (SPEC §5.3.6).

The two stores behind `Blobs` are [`PackStore`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-blob/src/pack/store.rs) (a local file) and [`RemoteBlobStore`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-blob/src/remote.rs) (any `Storage` with `conditional_put`). `MemoryBlobStore` is compiled only for tests and the `testing` feature.

## Example

This is library code, the same shape as the doctest on `BlobDatabase::gc`. `Database::open` does not open the pack (ROADMAP 9.4.5), so the example opens `<name>.persia.blobs` itself. `[0; 16]` is the id written into the pack header. Pairing that id with the container's database uuid is the same unfinished leaf. `Database::create` picks its own uuid and does not return it.

```rust
use std::sync::Arc;

use persia::internal::blob::Blobs;
use persia::internal::documents::{Blob, BlobDatabase};
use persia_blob::{PackMode, PackStore, pack_path};
use persia_engine::db::{Database, DatabaseOptions};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;

let path = std::path::Path::new("app.persia");
let mut log = Database::create(path, DatabaseOptions::default())?;
log.create_collection(SchemaBuilder::new("files").build()?)?;
let pack = PackStore::open(pack_path(path), PackMode::ReadWrite, [0; 16])?;
let mut db = BlobDatabase::new(log, Blobs::new(Arc::new(pack)));

let blob_ref = db.put(
    "files",
    &Document::new("invoice-1"),
    Blob::from_reader(&b"%PDF-1.7 example"[..]),
    Durability::Fsync,
)?;
assert_eq!(db.get_blob_range(&blob_ref, 0..8)?, b"%PDF-1.7");
```

A git dependency, not a crates.io release:

```toml
[dependencies]
persia = { git = "https://github.com/tomiok/persiaDB" }
persia-blob = { git = "https://github.com/tomiok/persiaDB" }
persia-engine = { git = "https://github.com/tomiok/persiaDB" }
```

`persia::internal` is hidden from rustdoc. The paths in the example are still public.

## Write order and durability

`BlobDatabase::put` stores the blob first and logs the document second (SPEC §9.5). The blob is durable when `BlobStore::bind` returns `BindOutcome::Bound`:

- In the pack, the `BlobIndex` frame is synced, then the superblock slot that covers it (SPEC §9.3, §18.9, [ADR-0049](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0049-blob-pack-layout-and-durability.md)).
- On the remote layout, `put_if_absent` of the manifest returned `Created` (SPEC §9.4, [ADR-0050](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0050-remote-blob-layout-transfers-resume-and-inline-blobs.md)).

`put_blob` pins the upload id before it reads a byte, then pins the blob id before it binds. The `Pins` it returns keep garbage collection off that upload and that id until you drop them. `BlobDatabase::put` holds those pins until the WAL record that names the blob is durable: immediately for `Durability::Fsync`, and at the next commit or `Fsync` operation otherwise. `commit` releases whatever that commit made durable, including when the commit itself fails after a sync.

A crash after the bind and before the document is logged leaves an orphan. No document points at it. Garbage collection may delete it. A `Blob::Stored` the store has not bound is `Error::BlobNotStored`, and nothing is logged.

A bind whose outcome is unknown is retried with the same manifest, with backoff, until `bind_budget`. The upload is not discarded. If the store is poisoned (`Error::is_store_poisoned`, the pack after a failed write or sync), the retries stop at once (ROADMAP 9.4.7). Reopen the pack to learn whether the bind landed.

## Local pack file

`pack_path` appends `.blobs`. The container `app.persia` keeps its blobs in `app.persia.blobs`. The file is created on the first `stage_chunk` or `bind`, not at `open`. A database that never stored a blob has no pack file.

The layout matches the container prefix ([file format](/guide/file-format/)): a 4 KiB header, two 4 KiB superblock slots, then append-only frames from byte 12288. The header magic is `PERSIAB\x01` (`BLOB_PACK_MAGIC`), so a pack does not open as a container and a container does not open as a pack. Frames are `BlobChunk` and `BlobIndex`. They are not compressed. The superblock carries a pack generation.

`PackStore::open(path, mode, db_uuid)` recovers the file (SPEC §9.3):

- It checks the header, picks the active slot, and rebuilds the index by scanning committed frames.
- A read-write open truncates a torn tail back to the committed `log_end` and syncs that length. Bytes past `log_end` were never acknowledged.
- An index frame past `log_end` whose chunks are all present is rolled forward (`PackRecovery::rolled_forward`).
- A read-write open also deletes stale temp files `<name>.persia.blobs.tmp.*` left by a crashed create or rewrite (`PackRecovery::swept`). A removal that fails is a warning, and the open continues. `persia container sweep` does not yet list those names (ROADMAP 9.2.10).

`PackMode::ReadWrite` is the writer's mode. `PackMode::ReadOnly` never creates, truncates, or writes. Two read-write handles on one file are not rejected in code yet (ROADMAP 9.2.7). The comment on `PackMode` says only the process that holds the container's writer lock should open read-write.

A failed write or sync poisons the handle (`Error::PackPoisoned`). Later writes fail until you reopen, and recovery decides what survived.

A chunk read is one read and one CRC32C check (ROADMAP 9.2.9). The read buffer is still zero-filled at most once per `get_blob` stream. Removing that fill is ROADMAP 9.2.11.

## Remote layout

`RemoteBlobStore::new(storage)` stores manifests and chunks as objects ([keys.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-blob/src/remote/keys.rs)):

```text
blobs/<hex[0..2]>/<hex(blob_id)>                 blob manifest
blobs/uploads/<upload_id:032x>/<chunk_idx:010>   staged chunk
```

Both are `put_if_absent` and are never overwritten. Staging the same chunk index with different bytes is `Error::StagedChunkConflict`. `uploads` is not two hex digits, so a manifest key and a chunk key never share a prefix. `RemoteKey::parse` accepts only keys this layout writes. A listing that returns a foreign object skips it.

Reads load the manifest, check that it names the id, and `BlobManifest::check_ref` checks id, size, and chunk size against the `BlobRef`. Every chunk's length and CRC32C are checked against the manifest. A remote object is treated as hostile input (SPEC §17).

Transfers run up to `DEFAULT_PARALLELISM` (8) requests at a time. `with_parallelism` changes that. `1` runs them on the caller thread.

`resume` lists the upload, hashes the leading run of full chunks, and returns a `ResumedUpload`. `put_blob` does not call it yet (ROADMAP 9.3.4).

The store needs `Capabilities::conditional_put`. Without it, staging and binding return `Error::ConditionalPutUnsupported`. Memory and the local-file `Storage` have that capability. S3, GCS, and Azure Blob backends are milestone M12 and are not written. The key layout itself does not wait on those backends.

`ObjectMeta.last_modified` is when the current bytes were written (SPEC §10.1, [meta.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-storage/src/meta.rs)). Memory uses its clock. A local file uses the file modification time. An object store would use `Last-Modified`. `None` means the backend cannot tell.

## Corruption

Damaged bytes are `Err`, not a panic.

| What is wrong | Error |
|---|---|
| A pack header, slot, or frame fails its check | `Error::CorruptAt` with `encoding`, `what`, and the byte `offset` of that slot or frame (0 for the header, 4096 or 8192 for a slot, 12288 onward for a frame) |
| A manifest names another id, or its size or chunk size disagrees with the `BlobRef` | `Error::Corrupt` from `check_blob_id` or `check_ref` |
| A chunk is missing, the wrong length, or the wrong CRC32C | `Error::Corrupt`, or `CorruptAt` when the pack can name the frame |
| The bytes do not hash to `expected_id` | `Error::BlobIdMismatch` (a caller error; nothing is bound) |
| The id is not bound | `Error::BlobNotFound` from `get_blob` |

`Error::is_corruption` is true for `Corrupt` and `CorruptAt`. `corrupt_offset` reads the offset when there is one.

## Garbage collection

GC runs only when the writer calls `BlobDatabase::gc` ([ADR-0052](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0052-blob-garbage-collection-mark-sweep-and-pack-rewrite.md)). There is no background thread.

`mark` takes three sets, while the database is borrowed so a document cannot be logged, deleted, or committed during the mark:

1. Blob ids and upload ids in the writer's pin set.
2. Blob ids `Database::referenced_blobs` still sees: live snapshots, snapshots a reader still holds, and WAL records from `wal_start` on.
3. The upload id named by each marked manifest. An inline manifest names none.

A referenced id with no manifest is listed on `GcReport.missing`. `persia verify` does not yet fail on that (ROADMAP 9.5.3).

`sweep` then deletes what the mark does not keep. Each pin check and delete holds the pin set's lock, so a `put_blob` or a read that starts during the sweep keeps what it pins.

Remote objects follow `GcOptions::grace` (default `DEFAULT_GRACE`, 24 hours). The floor is two hours, twice `MAX_BIND_BUDGET`, because a `put_if_absent` started inside the bind budget can land after the budget ends. An unmarked manifest older than the grace period is deleted, then the staged chunks of every upload that no kept manifest names. An upload's age is its newest chunk. An object whose `last_modified` is `None` is kept and counted in `SweepReport.unknown_age`. A manifest that cannot be read stops chunk deletion for that pass (`uploads_skipped`): GC deletes less, never more.

The pack has no grace period. It is rewritten when the frames nothing keeps are at least `rewrite_ratio` of the frame bytes (default `DEFAULT_REWRITE_RATIO`, 0.5). `0` forces a rewrite. Above `1` it never runs. The new file replaces the old one and the generation increases by one (`Rewrite::Done`). If a pin taken after the plan names something the rewrite would drop, the new file is discarded (`Rewrite::Deferred`). `Rewrite::BelowRatio` leaves the file alone. `Rewrite::NoPack` means there is no file.

A read-only pack handle does not reopen when `refresh` returns `Error::PackReplaced` (ROADMAP 9.2.13). It keeps the file it opened until that error. A reader in another process is not part of this process's mark. On the remote layout, a reader that lags the writer can find a deleted document's blob gone after the grace period (ROADMAP 13.2.5). That lease is not built.

## Still planned

- `persia::Db` and `Collection`, including blob methods on that facade (M10, ROADMAP 10.1). The names in [Embedded API](/guide/embedded-api/) are the sketch.
- Opening the pack inside `Database::open`, at `<name>.persia.blobs`, created on the first `put_blob`, with the container's `db_uuid` (ROADMAP 9.4.5).
- A `put_blob` resume token. `RemoteBlobStore::resume` and `Chunker::resume` exist; `PutBlobOptions` has no `resume` field (ROADMAP 9.3.4).
- `persia verify` reporting a referenced blob that is missing (ROADMAP 9.5.3). The public `put` rejecting a `BlobRef` this database never bound, and `verify` reporting one that was set under the public API, is ROADMAP 9.5.6. `BlobDatabase::put` already returns `BlobNotStored` for a `Blob::Stored` the store lacks. `Document::set_blob_ref` can still attach a reference with no bytes behind it.
- Enforcing a single read-write `PackStore` with the container's writer lock (ROADMAP 9.2.7).
- A read-only local pack that reopens after `Error::PackReplaced` (ROADMAP 9.2.13).
- S3, GCS, and Azure backends (M12). The remote key layout runs on `Storage` today (memory and local file).

## FAQ

### Can I call `Db::put_blob`?

No. There is no `persia::Db`. The working calls are `persia::internal::blob::Blobs::put_blob` and `persia::internal::documents::BlobDatabase::put`. M10 is the facade that is supposed to wrap them.

### Where does an embedded Rust database store the file?

Next to the container, in `<name>.persia.blobs`, or as objects under `blobs/` and `blobs/uploads/` when the store is a `RemoteBlobStore`. Segment bytes do not contain the file. They contain the `BlobRef`.

### What if the process dies after the blob is durable and before the document is logged?

The blob is an orphan. No committed or WAL-logged document names it, unless the log call had already appended a record. Garbage collection may delete an orphan. It does not delete a blob a live snapshot, a held snapshot, the WAL from `wal_start`, or a pin still names.

### Does storing the same file twice keep two copies?

No. The id is the SHA-256 of the bytes. The second bind finds the id already bound.

### Does garbage collection run on its own?

No. The writer calls `BlobDatabase::gc`. The default remote grace period is 24 hours. The local pack is rewritten, with no grace period, when at least half of its frame bytes are unreferenced.

### Can I store blobs in S3 today?

The key layout and the sweep's use of `ObjectMeta.last_modified` are written. The S3, GCS, and Azure backends are not (M12). `RemoteBlobStore` works over the memory backend and the local-file backend, which implement `Storage`.
