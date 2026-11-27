---
title: "Durability and crash recovery"
description: "How PersiaDB, an embedded Rust search database, keeps writes crash-safe: durability levels, commit, recovery on reopen, the writer lock, checksums, and persia verify."
head:
  - tag: title
    content: "Crash-safe embedded database in Rust: WAL and recovery | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Crash-safe embedded database in Rust: WAL and recovery | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Crash-safe embedded database in Rust: WAL and recovery | PersiaDB"
---

Persia DB is an embedded search database in Rust. A local database is one container file, `<name>.persia`. Writes go to a write-ahead log first. A commit publishes a manifest. Opening the file again recovers the last commit and replays the log ([SPEC §4.5](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#45-commit-protocol-local), [SPEC §8](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#8-write-path-and-lifecycle)).

The calls below are the embedded API, `persia::Db` and `persia::Collection` ([Embedded API](/guide/embedded-api/)), and the `persia` command. %%WRITE_PATH_NOW%% Live counts are [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md). The crates are unpublished (`publish = false`). There is no `cargo add persia`.

[Key concepts](/guide/concepts/) defines WAL, manifest, and commit. [Crate architecture](/guide/architecture/) is where the engine sits. [File format and storage](/guide/file-format/) is the container layout. Client files such as a PDF live in a separate pack; [blob storage](/guide/blobs/) is that file and its own crash rules. Short answers are on the [FAQ](/guide/faq/).

## What is durable today

The local container commit is implemented and crash-tested (M2), and so is the document write path on top of it (M6): the memtable, the WAL, flush and commit, deletes and upserts, segment merging, and open and recovery, including the single-writer lock, `persia verify`, and a crash-loop harness.

A crash during a commit leaves the previous commit or the new one. Recovery does not mix the two. Tests inject faults at the protocol steps and kill the process (a fault-injecting storage, and a kill -9 loop through `Db`). This page describes that local file. A commit to object storage, and a lease that fences writers across machines, are milestones M12 and M13 and are not written.

## The write-ahead log

Every `put` and `delete` that returns is a WAL record before it is visible: the operation, the collection, the document (or the deleted id), and a CRC32C (SPEC §8.2.1). Records are grouped into chunks, one frame of the container each.

`persia::Durability` is how durable a write is when its call returns. `OpenOptions::durability` sets it for a database, `Collection::with_durability` for one handle:

| Level | When the call returns | What a crash keeps |
|---|---|---|
| `Buffered` | The record is in the writer's memory, until the next write-out. | Nothing of that record. |
| `Async` | The record is written to the file and not synced. | The record if the process dies. A power loss can still drop it. |
| `Fsync` | The record is written and synced, together with everything written before it (group commit). The default. | The record. |

`Fsync` is one sync for the group, not one sync per record. A later `Fsync` write, and every commit, syncs the WAL first, so a `Buffered` or `Async` record from earlier becomes durable then.

A document the schema refuses is not logged: the call fails with `ErrorCode::InvalidArgument` (or `Limit`), and nothing changes. A failed WAL write poisons the database: later writes fail with `ErrorCode::Poisoned` until you close it and open it again, and the reopen is what says which records reached the file.

A commit drops the log that its segments cover, so the next open does not replay records that are already in segments. Replay is idempotent.

## Put, delete, and commit

These are methods of `persia::Collection` and `persia::Db`.

| Call | What it does |
|---|---|
| `Db::open` / `OpenOptions::open` | Creates a new database, or opens one read-write and recovers it. |
| `Db::create_collection`, `create_collection_json` | Commits the collection before it returns. |
| `Collection::put`, `put_batch`, `put_json`, `put_struct` | Upsert by document id, at the handle's `Durability`. |
| `Collection::delete` | Hides that id. Deleting an id the collection does not have is not an error. |
| `Collection::commit`, `Db::commit` | Flushes every collection's unflushed writes into segments and publishes a manifest. |
| `Db::close` | The final commit, then the maintenance thread is joined and the writer lock released. |

`put` logs the record, then applies it to the memtable. The new version hides older ones at once. The next commit writes tombstones for the committed copies (SPEC §8.3). Tombstones live in a sidecar. The segment file is not rewritten.

`put_batch` reaches the durability once for the whole batch (one sync for `Fsync`). A reader sees none of the batch until all of it is durable, and all of it once the call returns `Ok`. A rejected document ends the batch: the documents before it are put, it and the ones after are not.

A commit builds every segment before it writes anything, so a failure there changes nothing. A failed write, sync, or commit after that poisons the database (`ErrorCode::Poisoned`): the commit may or may not have happened, and a reopen tells.

You do not have to call `commit`. A read-write `Db` commits on its own maintenance thread (SPEC §8.2, ADR-0064): when a collection's writes have paused for `OpenOptions::idle_flush` (10 seconds by default), when its first unflushed write is `commit_interval` old (60 seconds) while writes never pause, and when a memtable reaches `memtable_bytes` (64 MiB). `Db::close` commits what is left. A commit only bounds what a later open replays: a write is as durable as its `Durability`, committed or not.

## What is visible after a write

A `put` or `delete` at any durability is visible to every read on that database once it returns: `Collection::get` and `Collection::search` load the latest published state, which includes the memtable (read-your-writes, SPEC §8.4). Reads take no lock, do not block the writer, and do not wait for a commit.

A commit moves the flushed documents into a new immutable segment. A read that already started keeps the state it loaded. The next read sees the new one.

Another process does not see the writer's memory. A read-only `Db` (`OpenOptions::new().read_only(true)`) reads the committed segments and replays the WAL records already in the file, including those written after the last commit. An `Async` record that reached the file and was not synced can be in that replay, and a power loss can still drop it. A `Buffered` record that was never written out is absent. A read-only `Db` shows that state until `Db::refresh`, which brings it up to what the writer has made durable since (SPEC §8.4, §8.6).

## Crash, reopen, and WAL replay

A read-write `Db::open` of an existing file recovers it before it returns (SPEC §8.6):

1. The container recovers its last commit. Zeros and stale frames after it are truncated. Other bytes there are copied to a quarantine file beside the container, then truncated (ADR-0014). A tail that could still hold an acknowledged write is not discarded: the open fails with `ErrorCode::Corrupt`.
2. The committed manifest is checked against the container, and each collection's segments and tombstone sidecars are opened.
3. The WAL records written after that commit are replayed in log order. A record whose checksum fails, or for a collection the commit does not hold, fails the open with `ErrorCode::Corrupt`; a record the collection refuses fails it too. The database is not returned half-replayed.
4. Reads then see the committed segments plus the replayed writes.

A read-only open runs the same checks, takes no lock, and never truncates, quarantines, or writes.

<!-- engine-internals -->
These are engine internals, not API to call: how the engine lays out a commit and what its recovery reads. They explain the guarantees above.

The engine appends the segment frames and syncs them before it appends the manifest frame, so a crash can tear at most that manifest frame. The container then syncs the frames, writes the inactive superblock slot (`seq + 1`, `log_end`, `wal_start`), and syncs again. The two slots sit on separate 4 KiB pages. Open keeps the slot with the higher `seq` whose CRC passes. A crash before the new slot is durable leaves the previous commit. Synced WAL frames past `log_end` are still on disk, and recovery finds them.

Recovery reads the header, the active slot, and the frames from `wal_start` to `log_end`, then scans the tail. The manifest's `seq`, `fence_token`, and `wal_start` must match the slot, and every location it holds must be a frame of the right kind below `log_end`. WAL chunks (magic `PWAL`, uncompressed) are replayed from `wal_start` on. A manifest frame left after `log_end` is a commit cut before its slot: the writer replays the WAL after it, then commits once, flushing nothing, so the records it logs next are not hidden behind that frame (ROADMAP 6.6.9).

If one collection's unflushed records hold `wal_start` back, replay can fill another collection's memtable. The writer flushes during replay when a memtable reaches its size threshold or runs out of document ids (about 2^31). A read-only open cannot flush: its memory grows with the unflushed backlog until that limit fails the open. A schema change and a collection drop flush every memtable first, so no record of the old schema is replayed (ROADMAP 6.3.10).
<!-- /engine-internals -->

## One writer and many readers

One read-write `Db` per file (SPEC §8.4, §4.5.1). The lock is taken without waiting: a second read-write open, in this process or another, fails at once with `ErrorCode::Conflict`. On Unix the lock is on the container file. On Windows it is `<name>.lock` beside the file. A read-only open takes no lock and never writes, so it runs beside the writer.

`Db::close` joins the maintenance thread and its merges, then releases the lock, so the next open succeeds at once. Dropping a `Db` without `close` waits at most 5 seconds for that thread. Past that bound the thread finishes on its own and keeps the lock until it ends, so an open right after such a drop can fail with `Conflict`.

Where the lock belongs to the process rather than the handle (Linux NFS, and some FUSE and CIFS mounts), a second read-write open of the same file in that process is not rejected. The code documents that limit (SPEC §4.5.1).

## Segment merges and file compaction

Segments are immutable. A merge reads several segments, drops deleted documents, and writes one new segment (SPEC §8.5). Nothing is analyzed again. A read-write `Db` merges in the background, on `OpenOptions::merge_threads` threads (1 by default; 0 runs each merge on the maintenance thread). A merge never blocks a put or a read, and its result is published through the same commit protocol, with deletes committed while it ran applied to it, so a deleted document does not come back.

The container file grows until compaction. The maintenance thread compacts it, copying the live frames into a new file that replaces the old one (SPEC §4.6), when dead bytes are at least 40% of the file and at least 1 MiB. `persia compact <file>` does the same from the command line, read-write, so it fails while another writer has the database. `persia stats` reports the live and dead bytes.

A streaming merge that encodes while it reads, instead of holding the merged segment in memory, is planned (ROADMAP 6.5.6).

## Checksums and corruption

Damaged bytes are an error, not a panic: `ErrorCode::Corrupt`.

| What is wrong | What you get |
|---|---|
| The container header, a superblock slot, or a frame fails its CRC32C | A torn tail of zeros or stale frames is truncated on a read-write open. A tail that may hold an acknowledged write, or a bad CRC inside a committed frame, fails `Db::open` with `Corrupt`. |
| The manifest disagrees with the container, or names a frame of the wrong kind | `Db::open` fails with `Corrupt`. |
| A WAL record's CRC, length, or operation is wrong | `Db::open` fails with `Corrupt`. |
| A segment block fails its checksum | The read that reaches it (`get`, `search`) fails with `Corrupt`. `persia verify` checks every block. |

Integers on disk are little-endian. Frame and block lengths are checked before a reader allocates past its bomb limit. Fuzz targets cover the WAL replay, the container parsers, and the segment open. [File format and storage](/guide/file-format/) is the checksum layout.

## `persia verify` and `persia inspect`

The `persia` binary checks a database without taking the writer lock and without writing, so a live writer may have the file open.

`persia verify <container>` opens the database read-only and runs the checks an open runs, then every segment's bytes, one live copy per document id, and the bytes after the last commit. It then opens the blob pack. Every blob a live document or a WAL record references must be bound in `<name>.persia.blobs`, and the pack's frame checksums and chunk CRC32Cs are checked. `--quick` skips that checksum scan and only checks that referenced blobs are bound. `--deep` also reads each referenced blob and checks its SHA-256 against its id. A reader lease is taken first, so a writer's garbage collection does not delete a blob those documents show while the command runs. If the writer commits during the run, the command fails with "changed during verify, rerun" (exit status 1). [Blob storage](/guide/blobs/) is the pack scan.

`persia inspect container <container>` prints the active superblock (`seq`, `wal_start`, `log_end`) and every frame from `wal_start`, including frames after `log_end`. `--id` lists the WAL records that put or delete that document id. `persia inspect segment` prints one segment file. `--verify` checks the whole segment and fails if it is corrupt.

Exit status: 0 success, 1 another failure, 2 usage error, 3 corrupt Persia data or a missing referenced blob, 4 the file is not a Persia container. A header that has the magic and then fails its CRC is damage (status 3). A wrong magic, or a file shorter than the magic, is status 4.

`persia verify` analyzes replayed text with the built-in analyzers only. A schema may name an analyzer the application registered (ROADMAP 6.6.8): `verify`, `persia stats` and `persia dump documents` skip the analysis of that field's replayed text and run every other check (`verify` names the field on its `analysis` row). `persia compact` and `persia gc` open the database read-write: they fail (status 1) while another writer holds the lock, and on a schema that names a registered analyzer they fail, writing nothing, while the WAL still holds a write of such a collection (open the database with its application and close it, then retry; ROADMAP 10.4.4, 10.4.8).

## Example

The sample opens a database in a temporary directory (the `tempfile` crate); any path works. A write is visible before any commit. A `Fsync` write is synced when its call returns, so it survives a crash before the commit; this sample closes the database, which commits, opens the same file again, and reads the documents back.

```rust
use persia::{Db, Durability, ErrorCode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("notes.persia");

    let db = Db::open(&path)?;
    let notes = db.create_collection_json(
        r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#,
    )?;
    assert_eq!(notes.durability(), Durability::Fsync); // the default
    notes.put_json(r#"{"id": "n1", "body": "hello"}"#)?;
    assert!(notes.get("n1")?.is_some()); // visible before any commit

    // A handle of its own durability: in the file when the call returns, not synced.
    let fast = db.collection("notes")?.with_durability(Durability::Async);
    fast.put_json(r#"{"id": "n2", "body": "hello again"}"#)?;

    // One writer: a second read-write open of the same file fails at once.
    let second = Db::open(&path).err().ok_or("a second writer opened")?;
    assert_eq!(second.code(), ErrorCode::Conflict);
    db.close()?; // commits, joins the maintenance thread, releases the lock

    let db = Db::open(&path)?;
    let notes = db.collection("notes")?;
    assert!(notes.get("n1")?.is_some());
    assert!(notes.get("n2")?.is_some());
    db.close()?;
    Ok(())
}
```

A git dependency, not a crates.io release:

```toml
[dependencies]
persia = { git = "https://github.com/tomiok/persiaDB" }
```

An `Fsync` put is on disk when `put` returns, even if the process dies before a commit. The next open replays it. A `Buffered` put that has not been written out is gone.

## Still planned

- A commit to S3, GCS, or Azure, and a writer lease across machines (M12, M13). The local file lock is what exists.
- A streaming merge writer (ROADMAP 6.5.6).
- Encryption at rest (M20). It has not been started.

## FAQ

### Is Persia DB a crash-safe embedded database in Rust?

The local container is. `Db::open` recovers the last committed manifest and replays the WAL. An `Fsync` write is synced before the call returns. A crash before a commit's superblock slot is durable leaves the previous commit. The crates are unpublished, so this is code on `main`, not a release you can `cargo add`.

### Does a put survive a crash before commit?

An `Fsync` put does. It is a synced WAL record, and the next open replays it. An `Async` put survives the process dying and can be lost if the machine loses power. A `Buffered` put that was never written out does not survive. A commit is what moves those documents into a segment and lets the log drop them; the database commits in the background and at `Db::close`.

### Can two processes write one file?

One read-write handle. The second read-write `Db::open` fails immediately with `ErrorCode::Conflict`. Any number of read-only handles (`OpenOptions::read_only`) can run beside the writer. They see the committed segments and the WAL records already in the file, and later writes after `Db::refresh`.

### What does persia verify do after a crash?

It opens the database read-only, checks the manifest, the segments, the WAL, and the tail, then checks that every referenced blob is in the pack. It does not truncate, quarantine, or repair. A read-write `Db::open` is what truncates or quarantines a torn tail. Exit status 3 means corruption or a missing referenced blob.

### Where do checksums stop a bad file?

A CRC32C covers the container header, each superblock slot, each frame, each WAL record, and segment blocks. Zeros and stale frames past the commit are dropped on a read-write open. A tail that may still hold an acknowledged write fails that open. A bad checksum inside a committed frame fails the open. The database is not returned with that frame applied.
