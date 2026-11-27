---
title: "Blob storage"
description: "Store files with documents in PersiaDB, an embedded Rust search database: Db::put_blob, Collection::put_with_blob, the local pack file, and persia gc."
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

Persia DB can store a file next to the document that describes it: a PDF, an image, or any other bytes. The search index keeps a reference. The bytes live in a content-addressed blob store beside the database file ([SPEC §9](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#9-blobs)).

The calls are on `persia::Db` and `persia::Collection` ([Embedded API](/guide/embedded-api/)). %%BLOB_NOW%% Live counts are [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md). The crates are unpublished (`publish = false`). There is no `cargo add persia`.

## What a blob is

A blob id (`persia::BlobId`) is the SHA-256 of the bytes ([ADR-0002](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0002-blob-hash-sha256.md)). Persia hashes what it reads. A `PutBlobOptions::expected_id` that does not match fails the call with `ErrorCode::InvalidArgument`, and nothing is bound. `BlobId` prints as 64 hex digits and parses back from them (`FromStr`).

A `persia::BlobRef` is a `blob_id`, a `size`, a `mime`, a `chunk_size`, and optional `user_meta` (SPEC §9.1, [ADR-0011](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0011-blob-ref-encoding.md)), each read by the getter of that name. `mime` defaults to `application/octet-stream`. `user_meta` is at most 32 entries (`BlobRef::MAX_META_ENTRIES`). Searchable metadata belongs in document fields. A reference a program kept outside the database is built again with `persia::blob_ref`, which checks those value rules (`InvalidArgument` otherwise).

The same bytes are one blob. A second store of those bytes binds the id that is already there and discards the new upload's chunks.

The bytes are cut into fixed-size chunks, each with a CRC32C. The default chunk size is 4 MiB. `PutBlobOptions::chunk_size` takes 64 KiB through 64 MiB. A blob already stored keeps its own chunk size. The on-disk encoding still accepts a chunk size from 1 byte, so an old blob stays readable ([ADR-0041](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0041-blob-chunk-size-floor.md)).

## The calls

| Call | What it does |
|---|---|
| `Db::put_blob(reader, &options) -> Result<BlobRef>` | Reads `reader`, stores the chunks, binds the id. Returns once the blob is durable. |
| `Db::get_blob(&blob_ref) -> Result<BlobReader>` | A `std::io::Read` of the whole blob, one window of chunks at a time, each checked. The blob is pinned while the reader lives. |
| `Db::get_blob_range(&blob_ref, range) -> Result<Vec<u8>>` | One byte range, every chunk it touches checked. |
| `Collection::put_with_blob(&doc, blob) -> Result<BlobRef>` | Stores the blob, then logs the document with that `BlobRef`. |
| `persia gc <file>` | Garbage collection, from the command line: deletes the blobs no document references. |

With the cargo feature `async`, `AsyncDb::put_blob`, `AsyncDb::get_blob_range`, and `AsyncCollection::put_with_blob` are the same calls as futures (an upload takes an owned reader). The async API has no `get_blob`, whose reader blocks: read a blob by ranges there.

`persia::Blob` is what `put_with_blob` stores: `Blob::from_reader(reader)`, `Blob::from_path(path)`, or `Blob::Stored(blob_ref)` for a reference `put_blob` returned. `Blob::with_options` gives the first two their `PutBlobOptions`.

`PutBlobOptions::new()` (also its `Default`) is a builder: `mime`, `user_meta(key, value)`, `expected_id`, `expected_size`, `chunk_size` (4 MiB by default), `bind_budget` (60 seconds by default, at most one hour), and `resume`. A store whose outcome stays unknown is retried, with backoff, until the bind budget, then the call fails with `ErrorCode::Transient`: storing again is safe, since blobs are content-addressed. A reader longer than `expected_size` fails at the first byte past it.

`resume` takes the `ResumeToken` of an interrupted upload (`Error::resume_token`: its upload id and chunk size). The local pack cannot resume: a token there is `ErrorCode::Unsupported` before any byte is read. A remote store resumes (SPEC §9.4); remote stores are milestone M12 and are not open through `Db` yet.

A document carries its reference: `Document::blob_ref`, also on a search hit (`hit.document.blob_ref()`). The hit does not carry the file bytes.

## Example

The sample opens a database in a temporary directory (the `tempfile` crate); any path works.

```rust
use std::collections::BTreeMap;
use std::io::Read;

use persia::{Blob, BlobId, Db, Document, ErrorCode, PutBlobOptions, ResumeToken, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("app.persia"))?;
    let files = db.create_collection_json(
        r#"{"name": "files", "fields": {"title": {"type": "text"}}}"#,
    )?;
    let title = files.schema()?.field("title").map(|f| f.id()).ok_or("no title field")?;

    // The blob first, then the document that references it, in one call.
    let invoice = Document::new("invoice-1").with(title, persia::Value::String("march invoice".into()));
    let options = PutBlobOptions::new().mime("application/pdf");
    let stored = files.put_with_blob(&invoice, Blob::from_reader(&b"%PDF-1.7 example"[..]).with_options(options))?;
    assert_eq!(stored.size(), 16);
    assert_eq!(stored.mime(), "application/pdf");

    // Reads by reference: a byte range, or the whole blob as a `Read`.
    assert_eq!(db.get_blob_range(&stored, 0..8)?, b"%PDF-1.7");
    let mut text = String::new();
    db.get_blob(&stored)?.read_to_string(&mut text)?;
    assert_eq!(text, "%PDF-1.7 example");

    // A search hit's document carries the reference, not the bytes.
    let page = files.search(&SearchRequest::new("invoice"))?;
    assert_eq!(page.hits[0].document.blob_ref(), Some(&stored));

    // Store bytes alone, then put a document with the reference: the same id for the same bytes.
    let again = db.put_blob(&b"%PDF-1.7 example"[..], &PutBlobOptions::new())?;
    assert_eq!(again.blob_id(), stored.blob_id());
    files.put_with_blob(&Document::new("invoice-copy"), Blob::Stored(again))?;

    // A reference this database does not hold is refused, and nothing is logged.
    let missing = persia::blob_ref(BlobId::from_bytes([7; 32]), 3, "text/plain", 1 << 20, BTreeMap::new())?;
    let refused = files.put_with_blob(&Document::new("ghost"), Blob::Stored(missing)).err().ok_or("stored")?;
    assert_eq!(refused.code(), ErrorCode::NotFound);
    assert!(files.get("ghost")?.is_none());

    // Bytes that do not hash to `expected_id` bind nothing; the local pack cannot resume an upload.
    let wrong = PutBlobOptions::new().expected_id(BlobId::from_bytes([0; 32]));
    assert_eq!(db.put_blob(&b"abc"[..], &wrong).err().ok_or("stored")?.code(), ErrorCode::InvalidArgument);
    let resume = PutBlobOptions::new().resume(ResumeToken::new(7, 64 << 10)).expected_size(3);
    assert_eq!(db.put_blob(&b"abc"[..], &resume).err().ok_or("stored")?.code(), ErrorCode::Unsupported);

    db.close()?;
    Ok(())
}
```

A git dependency, not a crates.io release:

```toml
[dependencies]
persia = { git = "https://github.com/tomiok/persiaDB" }
```

## Write order and durability

Document durability, the WAL, and what an open replays are [Durability and crash recovery](/guide/durability/). `Collection::put_with_blob` stores the blob first and logs the document second (SPEC §9.5). The blob is durable when the store has bound it: in the local pack, its index frame and then the superblock slot that covers it are synced (SPEC §9.3, [ADR-0049](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0049-blob-pack-layout-and-durability.md)).

The upload and the blob are pinned while the call runs, so garbage collection cannot take them, and the pins are held until the document that names the blob is durable: at once for `Durability::Fsync`, and at the next commit or `Fsync` write otherwise. The bytes are read before the writer is taken, so a large blob does not hold up other writes. A reader passed to `put_blob` or `put_with_blob` must not call back into the same `Db`.

A crash after the bind and before the document is logged leaves an orphan. No document points at it, and `persia gc` deletes it. A `Blob::Stored` reference the store does not hold is `ErrorCode::NotFound`. One that names a stored blob with another `size` or `chunk_size` is `ErrorCode::InvalidArgument` (SPEC §9.6): the reference is the caller's input. Either way nothing is logged. A document that carries a reference set by hand (`Document::set_blob_ref`) is put as `Blob::Stored`, with the same checks (ADR-0063 item 5).

A failed write or sync of the pack poisons it: later blob writes fail with `ErrorCode::Poisoned` until the database is closed and opened again, and recovery decides what survived.

## Local pack file

The container `app.persia` keeps its blobs in `app.persia.blobs`. The file is created with the first blob, not at `Db::open`: a database that never stored a blob has no pack file. `Db::backup` copies the pack with the container (`<dest>.blobs`).

One read-write handle holds the pack, through the writer lock `<name>.persia.blobs.lock`, which is taken at the open even before the pack file exists. A read-only `Db` reads the pack beside the writer. `persia container files` lists the pack's lock, its temporary files, and its reader-lease directory. `persia container sweep` removes the pack's stale temporary files only while it holds that lock. The lock file and the lease directory are never removed.

<!-- engine-internals -->
These are engine internals, not API to call: the pack's layout and what its read-write open recovers (SPEC §9.3).

The layout matches the container prefix ([file format](/guide/file-format/)): a 4 KiB header, two 4 KiB superblock slots, then append-only frames from byte 12288. The header magic is `PERSIAB\x01`, so a pack does not open as a container and a container does not open as a pack. Frames are chunk frames and index frames, not compressed. The superblock carries a pack generation, and the header the container's `db_uuid`.

A read-write open checks the header, picks the active slot, and rebuilds the index by scanning committed frames. It truncates a torn tail back to the committed `log_end` and syncs that length; bytes past `log_end` were never acknowledged. An index frame past `log_end` whose chunks are all present is rolled forward. Stale temporary files `<name>.persia.blobs.tmp.*` left by a crashed create or rewrite are deleted. Before a frame is read into memory, its length is bounded by its kind (ROADMAP 9.2.14): a chunk frame by a 64 MiB chunk, an index frame by the chunk frames scanned so far. An oversize frame in the tail is truncated; a committed frame past the bound is corruption.

A blob of at most 64 KiB is hashed whole and bound in one step. A larger blob is staged a window at a time (32 MiB, at most 64 chunks). A chunk read is one read and one CRC32C check. A garbage-collection rewrite copies the kept blobs into a new file and renames it over the pack; a read-only handle of the old file reopens the new one at its next refresh.
<!-- /engine-internals -->

## Remote layout

<!-- engine-internals -->
These are engine internals, not API to call yet: `Db` opens local files only, and the S3, GCS, and Azure backends are milestone M12. The blob store's remote layout is written and tested over the memory and local-file storage backends (SPEC §9.4, [ADR-0050](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0050-remote-blob-layout-transfers-resume-and-inline-blobs.md)).

```text
blobs/<hex[0..2]>/<hex(blob_id)>                 blob manifest
blobs/uploads/<upload_id:032x>/<chunk_idx:010>   staged chunk
```

Both are `put_if_absent` and are never overwritten. `uploads` is not two hex digits, so a manifest key and a chunk key never share a prefix. A listing that returns a foreign object skips it. A read checks that the blob manifest names the id, and its size and chunk size against the reference, then every chunk's length and CRC32C: a remote object is hostile input (SPEC §17). Transfers run up to 8 requests at a time. A resume pins the token's upload, checks the reader's bytes against the staged chunks' CRC32Cs, and stages only the rest; a resume that would write no new chunk restages the chunks under a fresh upload id, so a sweep cannot treat them as past the grace period (ROADMAP 9.3.6). The store needs a backend with conditional puts.
<!-- /engine-internals -->

## Corruption

Damaged bytes are an error, not a panic.

| What is wrong | `ErrorCode` |
|---|---|
| A pack header, slot, or frame fails its check | `Corrupt` |
| The blob stored under the id names another blob, or is damaged | `Corrupt` |
| A chunk is missing, the wrong length, or the wrong CRC32C | `Corrupt` |
| The bytes do not hash to `expected_id`, or run past `expected_size` | `InvalidArgument` (a caller error; nothing is bound) |
| A reference whose `size` or `chunk_size` is not the stored blob's | `InvalidArgument` (a caller error) |
| The id is not bound in this database | `NotFound` |

A `BlobReader` reports a damaged chunk as an `std::io::Error` whose inner error is the `persia::Error`.

## Garbage collection

Garbage collection runs only when you run `persia gc <file>` ([ADR-0052](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0052-blob-garbage-collection-mark-sweep-and-pack-rewrite.md)). There is no background collection, and the embedded API has no GC call. `persia gc` opens the database read-write, so it fails (status 1) while an application holds the writer lock: close the `Db` first.

It marks every blob a live document or a WAL record references, then deletes the others with their staged chunks. A deleted document's blob is collected once neither a live document nor a WAL record names it. The local pack is rewritten, with no grace period, when the frames nothing keeps are at least half of its frame bytes; the new file replaces the old one. A referenced blob the pack lacks is reported as missing: `persia verify` fails on that dangling reference, naming the collection, the document id, and the `blob_id` (exit status 3). `--json` reports what was deleted and kept and why, and the pack's size before and after (the keys are in the [README](https://github.com/tomiok/persiaDB#cli)).

A read-only `Db` in another process holds a reader lease beside the pack (`<name>.persia.blobs.leases/`, ADR-0054). `persia gc` does not sweep while a live lease is behind the last commit: nothing is deleted, and the report says so. `Db::refresh` brings that reader up to date. `persia verify` takes the same lease first. A directory that refuses the lease opens the read-only handle without one (a warning is logged); a collection in another process may then delete a blob that handle still shows, which reads as `ErrorCode::NotFound`.

`--grace` (default `24h`, at least `2h`) is how long an unmarked object of a remote store is kept after it was written. The local pack keeps no ages and drops what is unmarked at once.

<!-- engine-internals -->
These are engine internals, not API to call: how the collection decides what to keep.

The mark takes the writer's pinned blob and upload ids, then the blob ids the engine still references (live snapshots, snapshots a reader still holds, the WAL from `wal_start` on), then the upload id each marked blob manifest names. Every pin check and delete of the sweep holds the pin set's lock, so an upload or a read that starts during the sweep keeps what it pins. On a remote store, an unmarked blob manifest older than the grace period is deleted, then the staged chunks of every upload no kept manifest names; an object whose age is unknown is kept. Under the pin set's lock, before those chunks are deleted, the sweep lists the upload again and deletes only when that listing equals the first one (ROADMAP 9.5.10). A rewrite of the pack that a newer pin would make drop a kept blob is discarded instead.
<!-- /engine-internals -->

## Still planned

- Garbage collection from the embedded API, so a long-running application need not close its `Db` to run `persia gc`.
- S3, GCS, and Azure backends (M12). The remote key layout runs on the memory and local-file storage backends today.
- A reader lease for the remote layout, so a lagging reader keeps a blob the writer has stopped referencing (ROADMAP 13.2.5).
- A blob only a state kept from before a read-only handle's last refresh shows is not protected by its lease (ROADMAP 13.7.4): after a collection it reads as `NotFound`, never other bytes.
- A remote sweep whose two listings both miss a chunk just written (ROADMAP 13.7.3).
- `persia dump documents` with the blobs (ROADMAP 10.4.6): a dump carries references, not bytes, so it imports with its blobs only into the database it came from.

## FAQ

### Can I call `Db::put_blob`?

Yes. `Db::put_blob` stores bytes and returns their `BlobRef`; `Collection::put_with_blob` stores a blob and the document that references it in one call, the blob first. `Db::get_blob` and `Db::get_blob_range` read it back.

### Where does an embedded Rust database store the file?

Next to the container, in `<name>.persia.blobs`. Segment bytes do not contain the file. They contain the `BlobRef`.

### What if the process dies after the blob is durable and before the document is logged?

The blob is an orphan. No committed or logged document names it. `persia gc` deletes an orphan. It does not delete a blob a live document or a WAL record still names.

### Does storing the same file twice keep two copies?

No. The id is the SHA-256 of the bytes. The second store finds the id already bound.

### Does garbage collection run on its own?

No. Run `persia gc` with the database closed. The local pack is rewritten, with no grace period, when at least half of its frame bytes are unreferenced. A read-only reader in another process that has not refreshed since the last commit holds the sweep back.

### Can `put_blob` resume an upload?

Not on the local pack: a `PutBlobOptions::resume` token there is `ErrorCode::Unsupported`. A remote store resumes, with the `ResumeToken` from `Error::resume_token`, a reader of the whole blob from the start, and `expected_id` or `expected_size`; remote stores open through `Db` with milestone M12.

### Can I store blobs in S3 today?

No. The remote key layout and its garbage collection are written; the S3, GCS, and Azure backends are not (M12).
