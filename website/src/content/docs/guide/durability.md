---
title: "Durability and crash recovery"
description: "How PersiaDB, an embedded Rust search database, keeps writes crash-safe: the WAL, commit, recovery on reopen, the writer lock, checksums, and persia verify."
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

Persia DB is an embedded search database in Rust. A local database is one container file, `<name>.persia`. Writes go to a write-ahead log first. A commit publishes a manifest. Opening the file again recovers the last commit and replays the log ([SPEC §4.5](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#45-commit-protocol-local), [SPEC §8](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#8-write-path)).

That path is library code in `persia-engine` and `persia-storage`. It is not `persia::Db`. The public facade is milestone M10 and is not built. %%WRITE_PATH_NOW%% Live counts are [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md). The crates are unpublished (`publish = false`). There is no `cargo add persia`.

The names below are the ones in the tree: `Database`, `Durability`, `ReadOnlyDatabase`, and the `persia` command. [Key concepts](/guide/concepts/) defines WAL, manifest, and commit. [Crate architecture](/guide/architecture/) is where the engine sits. [File format and storage](/guide/file-format/) is the container layout. Client files such as a PDF live in a separate pack; [blob storage](/guide/blobs/) is that file and its own crash rules. Short answers are on the [FAQ](/guide/faq/).

## What is durable today

The local container commit is implemented and crash-tested (M2). The document write path on top of it is in progress (M6): the memtable, the WAL, flush and commit, deletes and upserts, segment merging, and open/recovery, including the single-writer lock, `persia verify`, and a crash-loop harness.

A crash during a commit leaves the previous commit or the new one. Recovery does not mix the two. Tests inject faults at the protocol steps and kill the process (`FaultyStorage`, and a kill -9 loop). This page describes that local file. A commit to object storage, and a lease that fences writers across machines, are milestones M12 and M13 and are not written.

## The write-ahead log

Every acknowledged `put` and `delete` is a WAL record before it is visible: operation (`put` or `delete`), collection name, payload, and a CRC32C ([`WalRecord`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/wal/chunk.rs), SPEC §8.2.1). A put's payload is the document encoding. A delete's payload is the document id. Records are grouped into chunks. A chunk starts with the magic `PWAL`. Locally, one chunk is one `WalChunk` frame in the container, uncompressed.

[`Durability`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/wal/writer.rs) is how durable the call is when it returns:

| Level | When the call returns | What a crash keeps |
|---|---|---|
| `Buffered` | The record is in the writer's memory, until the next write-out. | Nothing of that record. |
| `Async` | The record is written to the file and not synced. | The record if the process dies. A power loss can still drop it. |
| `Fsync` | The record is written and synced, together with everything written before it (group commit). The default. | The record. |

`Fsync` is one sync for the group, not one sync per record. A later `Fsync` call, and every commit, syncs the WAL first, so a `Buffered` or `Async` record from earlier becomes durable then. [`Database::durable_mark`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db.rs) is the last operation known durable. It never moves backward.

The memtable checks the operation before the record is acknowledged. A put the memtable rejects is not logged (SPEC §8.2.1). A failed WAL write poisons the database: later writes fail until you drop it and open again, and the reopen is what says which records reached the file.

Chunks rotate. The default target is 1 MiB (`DEFAULT_CHUNK_TARGET`). A commit advances `wal_start` to the end of the log once every flushed record is in a segment, so the next open does not replay records that are already in segments. One exception: a flush during replay keeps `wal_start` at the chunk being replayed, because later records in that chunk are not in a memtable yet. Replaying that chunk again is harmless. Replay is idempotent.

## Put, delete, and commit

These methods are on [`persia_engine::db::Database`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db.rs).

| Call | What it does |
|---|---|
| `Database::create` / `Database::open` | Creates a new container, or opens one read-write and recovers it. |
| `create_collection` | Commits the collection at once. A record is logged only after its collection's create is committed. |
| `put` / `put_batch` | Upsert by document id, at a `Durability`. |
| `delete` | Hides that id. Deleting an id the collection does not have is not an error. |
| `commit` | Flushes every memtable that holds operations and publishes a manifest. Returns the committed `seq`, unchanged when there was nothing to flush. |
| `tick` | Commits when the [`CommitPolicy`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db/policy.rs) says a memtable is due. |

`put` logs the record, then applies it to the memtable. The new version hides older ones at once. The next commit writes tombstones for the committed copies (SPEC §8.3). Tombstones live in a sidecar. The segment file is not rewritten.

`put_batch` reaches `durability` once for the whole batch. The documents before the last are logged `Buffered`. A batch of two or more stays hidden from readers until that level is reached. A batch can commit part of itself when a memtable crosses its size threshold; that commit syncs the WAL first, so it publishes only durable documents. A failed sync poisons the database with the batch still unseen.

`commit` plans the flush before it writes. A failure while planning (a segment that cannot be built, a sidecar that cannot follow the current one) leaves the database unchanged and usable. Any later error poisons it: the commit may or may not have happened, and a reopen tells.

The local protocol is the container's (SPEC §4.5). The engine appends segment frames and syncs them before it appends the manifest, so a crash can tear at most that manifest frame. [`Container::commit`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-storage/src/container.rs) then syncs the frames, manifest included, writes the inactive superblock slot (`seq + 1`, `log_end`, `wal_start`), and syncs again. The two slots sit on separate 4 KiB pages. Open keeps the slot with the higher `seq` whose CRC passes. A crash before the new slot is durable leaves the previous commit. Synced frames past `log_end` are still on disk. Recovery finds them.

Nothing calls `tick` or `commit` on a background thread. Merge workers only merge. The defaults, if you call `tick`, are a 64 MiB memtable, a 10 second idle flush, and a 60 second commit interval (`FlushPolicy`, `CommitPolicy::DEFAULT_INTERVAL`). A memtable that is already at the size threshold is flushed inside `put`, before the new document.

## What is visible after a write

A `put` or `delete` at any durability is visible on the writer's published snapshot as soon as the memtable accepts it. [`Database::snapshot`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db.rs) and [`Database::reader`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db.rs) load that snapshot. [`CollectionSnapshot::get`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db/snapshot.rs) sees the memtable copy (read-your-writes, SPEC §8.2–§8.4). `reader` is an `ArcSwap` handle: it does not block the writer, and it does not wait for a commit.

`commit` publishes a new snapshot. The flushed documents are in a new immutable segment, and that collection's memtable is empty. A reader that already loaded a snapshot keeps it. The next `snapshot()` call sees the new `seq`.

Another process does not see the writer's memory. [`ReadOnlyDatabase::open`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db/read_only.rs) reads the committed segments and replays WAL frames from `wal_start` on, including frames written after `log_end`. An `Async` record that reached the file and was not synced can be in that replay, and a power loss can still drop it. A `Buffered` record that was never written out is absent. The read-only open does not follow later commits. Open it again for those. Refreshing a `ReadOnlyDatabase` in place, from the next manifest and the new WAL frames, is planned (ROADMAP 6.6.6).

Ranked search is `CollectionViews::search` on a collection snapshot. `Database` has no `search` method. `persia::Db` is not in the crate. [Getting started](/guide/getting-started/) separates that library code from the illustrative `Db::open`.

## Crash, reopen, and WAL replay

[`Database::open`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db.rs) runs the same sequence as a first open (SPEC §8.6, [`recovery`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db/recovery.rs)):

1. The container recovers: header, the active superblock slot, the frames from `wal_start` to `log_end`, then a scan of the tail. `Database::open` uses `Repair::Safe` (ADR-0014). Zeros and stale frames in that tail are truncated. Other bytes are copied to a quarantine file beside the container and then truncated. A frame whose `frame_seq` could still be an acknowledged write is not discarded: the open fails with `Error::CorruptTail`. `Repair::Quarantine`, on `Database::open_container` after you open the container yourself, copies that frame aside and opens what precedes it.
2. The committed manifest is decoded and checked against the slot: `seq`, `fence_token`, and `wal_start` match, and every location it holds is a frame of the right kind below `log_end`.
3. Each collection's segments and tombstone sidecars are opened. Each collection gets an empty memtable.
4. WAL chunks from `wal_start` on are replayed in log order. A record whose CRC fails, a record the memtable rejects, or a record for an unknown collection fails the open. Records before the bad one stay applied only inside that failed attempt. The database is not returned half-replayed.
5. The first snapshot is the committed segments plus the replayed memtables.

A manifest frame left after `log_end` is a commit that was cut before its superblock slot. Both the writer and a read-only open replay the WAL after it. The writer then commits once, flushing nothing, so the next records it logs are not hidden behind that frame (ROADMAP 6.6.9).

If one collection's unflushed records hold `wal_start` back, replay can fill another collection's memtable. The writer flushes during replay when a memtable hits its size threshold or runs out of document ids (about 2^31). A read-only open cannot flush. Its memory grows with the unflushed backlog until that docid limit fails the open.

A schema change and a collection drop flush every memtable first, so `wal_start` reaches the end of the log and no record of the old schema is replayed (ROADMAP 6.3.10).

## One writer and many readers

One read-write `Database` per container (SPEC §8.4, §4.5.1). The lock is taken without waiting. A second `Database::open` fails at once with the container's `Locked` error. On Unix the lock is on the container file. On Windows it is `<name>.lock` beside the file. A read-only open takes no lock and never writes, so it runs beside the writer.

Dropping the writer joins its merge workers first. They do not hold the file. The lock is released before the descriptor is closed, so a duplicated or inherited descriptor does not keep the next open out (ROADMAP 6.6.11).

Where the lock belongs to the process rather than the handle (Linux NFS, and some FUSE and CIFS mounts), a second read-write open of the same file in that process is not rejected. The code documents that limit (SPEC §4.5.1).

## Segment merges and file compaction

Segments are immutable. A merge reads N segments, drops tombstoned documents, remaps document ids, and writes one new segment (SPEC §8.5). Term dictionaries, postings, columns, stored documents, the id map, and blob references are copied. Statistics are recomputed. Nothing is analyzed again.

[`Database::merge_step`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db/merging.rs) commits merges that have finished, plans more with the tiered policy, and starts as many as there are threads. The default is one worker thread. `0` runs one merge on the caller's thread. Call `merge_step` periodically. The threads do not wake on their own, and a merge never blocks a put or a read.

The writer commits the result through the same commit protocol. `Segment::verify` runs on the merged segment before that commit. Deletes committed to the inputs while the merge ran are applied to the result, so a deleted document does not come back. A failed merge is reported. Its inputs are not merged again until the database is reopened. Obsolete segment frames stay until every snapshot that can still read them is dropped.

The container file grows until compaction. [`Database::compact`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db/compaction.rs) copies the live frames into a new file and replaces the old one (SPEC §4.6). With the default [`CompactionOptions`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db/compaction.rs), `merge_step` does that when dead bytes are at least 40% of the file and at least 1 MiB. [`Database::space_usage`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/db.rs) reports the dead bytes. A failed sync of the new file leaves the database on that file and poisoned. Open segments keep the bytes they already hold, including a mapping of the old file, which stays valid.

A streaming merge that encodes while it reads, instead of holding the merged segment in a builder, is planned (ROADMAP 6.5.6). Merges that exist today still publish through commit.

## Checksums and corruption

Damaged bytes are `Err`, not a panic.

| What is wrong | What you get |
|---|---|
| The container header, a superblock slot, or a frame fails its CRC32C | The container error. A torn tail of zeros or stale frames is truncated on a read-write open. A tail that may hold an acknowledged write fails `Database::open` with `CorruptTail`. A bad CRC inside a committed frame fails the open. |
| The manifest disagrees with the active slot, or names a frame of the wrong kind | `DatabaseError`, and `is_corruption` is true. |
| A WAL record's CRC, length, or operation is wrong | `ReplayError`. The open fails. |
| A segment block or footer fails `Segment::verify` | `ReadError`. `persia verify` runs that check. |

Integers on disk are little-endian. Frame and block lengths are checked before a reader allocates past its bomb limit. Fuzz targets cover the WAL replay, the container parsers, and the segment open. [File format and storage](/guide/file-format/) is the checksum layout.

## `persia verify` and `persia inspect`

The `persia` binary checks a database without taking the writer lock and without writing, so a live writer may have the file open.

`persia verify <container>` opens the database read-only and runs the checks an open runs, then every segment's bytes, one live copy per document id, and the bytes after the last commit. It then opens the blob pack. Every blob a live document or a WAL record references must be bound in `<name>.persia.blobs`, and the pack's frame checksums and chunk CRC32Cs are checked. `--quick` skips that checksum scan and only checks that referenced blobs are bound. `--deep` also reads each referenced blob and checks its SHA-256 against its id. The document checks run before the pack is opened. A reader lease is taken first, so a writer's garbage collection does not delete a blob those documents show while the command runs. If the committed `seq` moves during the run, the command fails with "changed during verify, rerun" (exit status 1). [Blob storage](/guide/blobs/) is the pack scan.

`persia inspect container <container>` prints the active superblock (`seq`, `wal_start`, `log_end`) and every frame from `wal_start`, including frames after `log_end`. `--id` lists the WAL records that put or delete that document id. `persia inspect segment` prints one segment file. `--verify` checks the whole segment and fails if it is corrupt.

Exit status: 0 success, 1 another failure, 2 usage error, 3 corrupt Persia data or a missing referenced blob, 4 the file is not a Persia container. A header that has the magic and then fails its CRC is damage (status 3). A wrong magic, or a file shorter than the magic, is status 4.

`persia verify` analyzes replayed text with the built-in analyzers only. A schema may name an analyzer the application registered (ROADMAP 6.6.8): `verify`, `persia stats` and `persia dump documents` skip the analysis of that field's replayed text and run every other check (`verify` names the field on its `analysis` row). `stats` and `dump documents` read a database the same way as `verify`: no lock, nothing written. `persia compact` opens it read-write, so it fails (status 1) while another writer holds the lock, and on a schema that names a registered analyzer, which its read-write open needs (ROADMAP 10.4.4). Import and search are not in the binary (ROADMAP 10.4.2).

## Example

This is `persia_engine::db::Database`. `persia::Db` is not exported. `Database::create` fails if the file already exists. `create_collection` commits. The `put` is visible on the snapshot before `commit`. After `commit`, a new process `open`s the same file and reads the document back from the recovered snapshot.

```rust
use persia_engine::db::{Database, DatabaseOptions};
use persia_engine::document::{Document, Value};
use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::Durability;

let path = std::path::Path::new("notes.persia");
let schema = SchemaBuilder::new("notes")
    .field("body", FieldKind::Text(TextOptions::default()))
    .build()?;
let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
let mut db = Database::create(path, DatabaseOptions::default())?;
db.create_collection(schema)?;
db.put(
    "notes",
    &Document::new("n1").with(body, Value::String("hello".into())),
    Durability::Fsync,
)?;
let snap = db.snapshot();
let notes = snap.collection("notes").ok_or("no collection")?;
assert!(notes.get("n1")?.is_some());
db.commit()?;
drop(db);

let db = Database::open(path, DatabaseOptions::default())?;
let snap = db.snapshot();
let notes = snap.collection("notes").ok_or("no collection")?;
assert!(notes.get("n1")?.is_some());
```

A git dependency, not a crates.io release:

```toml
[dependencies]
persia-engine = { git = "https://github.com/tomiok/persiaDB" }
```

An `Fsync` put is on disk when `put` returns, even if the process dies before `commit`. The next `open` replays it into the memtable. A `Buffered` put that has not been written out is gone. A second `Database::open` of the same path, while this writer is still alive, returns an error.

## Still planned

- `persia::Db` and `Collection` (M10). The names in [Embedded API](/guide/embedded-api/) are the sketch.
- Refreshing a `ReadOnlyDatabase` in place, instead of opening the file again (ROADMAP 6.6.6).
- `persia compact` on a schema that names an analyzer registered past the built-ins (ROADMAP 10.4.4).
- A commit to S3, GCS, or Azure, and a writer lease across machines (M12, M13). The local file lock is what exists.
- Encryption at rest (M20). It has not been started.
- A background thread that calls `tick` or `merge_step`. The caller invokes both.

## FAQ

### Is Persia DB a crash-safe embedded database in Rust?

The local container is. `Database::open` recovers the last committed manifest and replays the WAL. An `Fsync` write is synced before the call returns. A crash before the new superblock slot is durable leaves the previous commit. `persia::Db` is not in the crate, and the crates are unpublished, so this is library code on `main`, not a release you can `cargo add`.

### Does a put survive a crash before commit?

An `Fsync` put does. It is a synced WAL record, and the next open replays it. An `Async` put survives the process dying and can be lost if the machine loses power. A `Buffered` put that was never written out does not survive. `commit` is what moves those documents into a segment and lets the log drop them.

### Can two processes write one file?

One read-write handle. The second `Database::open` fails immediately. Any number of `ReadOnlyDatabase::open` handles can run beside the writer. They see the committed segments and the WAL frames already written. They do not see a later commit until they open again.

### What does persia verify do after a crash?

It opens the container read-only, checks the manifest, the segments, the WAL, and the tail, then checks that every referenced blob is in the pack. It does not truncate, quarantine, or repair. A read-write `Database::open` is what applies `Repair::Safe` to the tail. Exit status 3 means corruption or a missing referenced blob.

### Where do checksums stop a bad file?

A CRC32C covers the container header, each superblock slot, each frame, each WAL record, and segment blocks. Zeros and stale frames past the commit are dropped on a read-write open. A tail that may still hold an acknowledged write fails that open. A bad checksum inside a committed frame fails the open. The database is not returned with that frame applied.
