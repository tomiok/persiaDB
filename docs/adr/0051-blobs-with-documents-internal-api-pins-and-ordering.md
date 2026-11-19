# ADR-0051: Blobs with documents: the internal API, the pin set, and the order of `put(doc, blob)`

- Status: accepted
- Date: 2026-11-18
- Related: SPEC §7.5.1, §8.2.1, §9.2, §9.5, §9.6, §18.9; ROADMAP 9.4.1–9.4.3, 9.5.1, 9.5.2, 9.5.6; ADR-0034, ADR-0041

## Context

ROADMAP 9.4 joins blobs to documents: `put_blob` and `get_blob_range` over the `BlobStore` contract, `put(doc, blob)`
that stores the blob before it logs the document (SPEC §9.5), and hits that return the document's `blob_ref`. The
public embedded API (`Db`, `Collection`, `Blob::from_path`) is M10's, and the engine must stay independent of
`persia-blob` (it never looks a `BlobRef` up, SPEC §9.5). GC (ROADMAP 9.5) needs a pin set that `put_blob` fills
and the sweep checks under one lock. SPEC §18.9 was decided for ROADMAP 9.2.2 as "sync per blob": `bind` answers
`Bound` only once the blob is durable. Nothing in the engine said when a logged record becomes durable.

## Decision

1. **An internal layer in `persia`.** `persia::internal` (`#[doc(hidden)]`, not a stable API) holds:
   - `blob::Blobs<S: BlobStore + ?Sized>`: the store (`Arc<S>`), the pin set and the upload-id source, with
     `put_blob(reader, &PutBlobOptions) -> Result<(BlobRef, Pins)>`, `get_blob(&BlobRef) -> BlobReader` (an
     `impl Read`), `get_blob_range(&BlobRef, Range<u64>)` and `pin_contained(&BlobRef) -> Option<Pins>`;
   - `pins::{PinSet, Pins, Pinned}`;
   - `documents::BlobDatabase<S, L: DocumentLog = Database>`: `put(collection, &doc, Blob, Durability) -> BlobRef`,
     `commit`, `release_durable`; `Blob` is `Reader`, `Path` (each with `PutBlobOptions`) or `Stored(BlobRef)`.
   M10 wraps `BlobDatabase` in `Db`/`Collection` and maps `Blob::from_path`/`from_reader` onto `Blob`; nothing here
   exposes `set_blob_ref` (SPEC §9.6). The layer is generic over the store, so the pack file (ROADMAP 9.2) and the
   remote layout (9.3) plug in unchanged; `DocumentLog` is the engine `Database` in production, a wrapper in tests.
2. **`put_blob`.** Options are checked before any byte is read: a chunk size below 64 KiB however it was built
   (`ChunkSizeBelowMin`, ADR-0041), an invalid `mime`/`user_meta` (`InvalidBlobRef`). It draws a 128-bit upload id
   from the engine's `SegmentIds` (SPEC §8.7.1's keyed PRF) and pins it first. It reads up to 64 KiB + 1 byte: a
   blob that ends within 64 KiB is hashed whole and goes to `bind_small`; a larger one is chunked by `Chunker` and
   staged with `stage_chunks` a window at a time (32 MiB of chunks, 1..=64 chunks), which bounds memory. Then
   `expected_id` is checked (mismatch: `BlobIdMismatch`, upload discarded best-effort), the blob id is pinned, and
   the manifest bound. Default `mime` is `application/octet-stream`, default chunk size 4 MiB.
3. **Binding outcomes.** An unknown outcome is retried with the same manifest up to `bind_attempts` times
   (default 16), then `put_blob` fails with that error; an upload whose outcome was once unknown is never
   discarded. `AlreadyBound` returns a `BlobRef` with the bound manifest's chunk size, after checking it names the
   id and the size; its chunks are discarded only when the bound manifest names another upload and no outcome was
   unknown (a retry may find its own earlier write). A staging or reader error discards nothing: a resume
   (ROADMAP 9.3.2) or GC takes the chunks. Discard failures are logged (`tracing::warn!`) and ignored (GC is the
   backstop).
4. **`get_blob`** looks the manifest up and checks it against the `BlobRef` once (`BlobNotFound` or `Corrupt` at
   open), then reads one window at a time through `read_range`, so every chunk's length and CRC32C is checked and at
   most one window is in memory. Each window looks the manifest up again (one small read per 32 MiB).
5. **The pin set** maps each pinned `Pinned::{Upload, Blob}` to a count of live guards, behind one mutex. A `Pins`
   guard releases its ids on drop. GC uses `pinned()` for its mark and `unless_pinned(id, delete)` for each sweep
   delete: the check and the delete run under the lock, so a pin taken after the mark waits and still protects its
   object (SPEC §9.5). `delete` must not take pins. A `Blob` pin is taken before its manifest is read, and the upload
   that manifest names is pinned right after (a duplicate pins the bound blob's upload; `pin_contained` pins the
   upload of the manifest it found), so a referenced blob's chunks are pinned beside its manifest. Between the two
   pins only the blob pin protects them: the sweep (ROADMAP 9.5.2), before deleting an upload's chunks under the
   lock, must also find no pinned blob whose manifest names that upload.
6. **Order of `put(doc, blob)`.** The blob is stored (`put_blob` returns only after `bind` answered, so it is
   durable) or, for `Blob::Stored`, pinned and then checked with `contains` (`BlobNotStored` otherwise), before the
   document is logged with the reference. This holds for every WAL durability, since no WAL byte names the blob
   before it is durable. A crash before the log leaves an orphan blob; never a reference without its blob.
7. **The durability hook.** `WalWriter::durable_mark()` (records numbered up to it are synced: advanced by every
   successful `sync`, an `Fsync` append and `commit_point`) and `Database::{wal_mark, durable_mark}`. `put` keeps
   the pins with the `wal_mark` taken after logging, and releases them once `durable_mark` reaches it: at once at
   `Fsync`, at the next commit or `Fsync` operation otherwise. A put that appended its record and then failed (a
   failed sync, which poisons the database) keeps its pins while the `BlobDatabase` lives, since the record may be
   on disk; a put rejected before its record was appended releases them. No format change.
8. **Hits** already carry the `BlobRef` (SPEC §7.5.1): the DocStore row holds it, so memtable, segments, merges and
   replay return it; the `BlobRefs` section duplicates it per docid for GC's mark and `verify` (ROADMAP 9.5).
   ROADMAP 9.4.3 adds the tests that pin this down.

## Consequences

- `persia` cannot use `SimFile` (its allowed dependencies are `persia-engine` and `persia-blob`), so the crash test of
  `put(doc, blob)` uses real files: a store wrapper fails every call from the n-th on (a crash), the database is
  dropped (buffered records are lost, written ones kept: at least what a machine crash keeps) and reopened, and every
  document found must have its blob bound. A `DocumentLog` wrapper checks the blob is bound and pinned at the moment
  each document is logged.
- Pins of documents logged `Buffered`/`Async` live until the next commit or `Fsync` operation; a writer that never
  commits holds them. GC therefore never sweeps a blob such a document references.
- `bind_attempts` bounds the retry SPEC §9.4 states as "until it answers": a caller sees an error instead of a hang;
  the upload stays for GC, and nothing references the blob. Its pins are dropped then, so only the grace period (24 h) keeps
  the upload's chunks from a sweep while a late conditional write may still bind them: the grace period must stay far
  above any such delay.
- `BlobDatabase::log_mut` exposes the engine's `Database::put`, which takes any `set_blob_ref`; M10's `Db` must not
  re-expose it (SPEC §9.6).
- Pins of a poisoned database's appended records stay while the `BlobDatabase` lives; M10 drops it on poison.

## Alternatives considered

- **`put_blob` in `persia-blob`.** It needs only the store, but the pin set's other user, GC's mark, needs the
  engine; keeping both in `persia` keeps the pin set beside its users. The sweep can stay in `persia-blob` behind a
  closure if wave 2 prefers.
- **Release pins when `put` returns.** Wrong for `Buffered`/`Async`: the record is not durable, and a GC between
  would see neither the WAL record (not on disk) nor a pin.
- **Infinite bind retry.** Literal SPEC §9.4, but a remote store that keeps failing would hang the writer.
