# ADR-0054: Reader leases on the local blob pack

- Status: proposed
- Date: 2026-11-21
- Related: SPEC §4.5.1, §4.8, §8.4, §9.3, §9.5, §18.10; ROADMAP 9.2.13, 9.2.15, 9.2.16, 13.2.4, 13.2.5; ADR-0016,
  ADR-0019, ADR-0021, ADR-0049, ADR-0052, ADR-0053

## Context

GC's mark (SPEC §9.5, ADR-0052 item 2) sees only the readers of the writer's own process. A read-only handle
(`ReadOnlyBlobDatabase`) in another process, or a second read-only handle in the writer's process, holds documents of
an older commit than the writer's. When the writer deletes a document, commits, and GC rewrites the pack without its
blob, that reader still shows the document, and once its `refresh` reopens the rewritten pack
(`PackReplaced`, `PackStore::reopen`, ROADMAP 9.2.13) `get_blob` fails with `BlobNotFound` (SPEC §9.3, §9.5). The
writer must keep every blob a live reader may still reference.

The user decided (2026-11-21): one lease file per read-only handle, beside the pack, held under an OS advisory lock
for the handle's life and holding the `seq` of the documents it holds; a reader that dies loses its lock, and the
writer removes a lease it can lock; no wall clock, no heartbeat. GC skips its sweep (and so the pack rewrite) while a
live lease is older than the mark, with no per-blob bookkeeping. This ADR fixes the file, the protocol, and the edge
cases. It is an on-disk format (a new file kind), so it follows the format discipline: SPEC text, its own magic and
version, this ADR, and golden files.

## Decision

### Where and what

1. **Location.** `<name>.persia.blobs.leases/`, a directory beside the pack (`<name>.persia.blobs`), found from the
   container's canonical path as the pack is (SPEC §9.3), so a database opened through a symbolic link uses the
   leases beside the real file, as its writer does. A reader creates the directory if it is missing (`AlreadyExists`
   is success); nothing removes it. It is not a temporary: a sweep of side files (`Container::side_files`,
   `persia container sweep`) leaves it and everything in it alone. A copy of the database (SPEC §4.8) need not take
   it: a copied lease is unlocked, so the copy's writer removes it as stale (item 6).
2. **Names.** One file per read-only handle: `<lease_id>.lease`, `lease_id` a random `u128` (as a `pack_id` is drawn)
   written as 32 lowercase hex digits. On Windows the lock lives beside it, `<lease_id>.lease.lock` (item 5). Any
   other name in the directory was not written by Persia: never read, locked or removed, as foreign keys are not
   (SPEC §9.4).
3. **Record.** A lease file holds exactly one 64-byte record, little-endian (SPEC §4.1 rules), with its own magic and
   version:
   ```
   0   4   magic      bytes "PRLS" (0x50 0x52 0x4C 0x53)
   4   4   version    u32 = 1
   8   4   flags      u32 = 0 (no bit assigned)
   12  4   pid        u32: the holder's process id, for operators only (never trusted)
   16  16  db_uuid    the container's db_uuid
   32  16  lease_id   u128, the id in the file's name
   48  8   seq        u64: the committed seq (the manifest's) of the documents the handle holds; 0 while it opens them
   56  4   reserved   u32 = 0
   60  4   crc32c     u32 over bytes 0..60
   ```
   Decoding checks, in this order: exactly 64 bytes, the magic, the CRC32C, `version` (0 is corruption, above 1 is
   unsupported), `flags` (any bit is unsupported), `reserved` zero, then `lease_id` against the name and `db_uuid`
   against the pack's. Anything else that does not hold is corruption (`Error::Corrupt`, `Error::UnsupportedVersion`,
   `Error::UnsupportedFlags`). Each value has one encoding. The writer reads at most 4096 bytes of a lease, so a
   hostile file costs one bounded read.

### The reader's side

4. **Life cycle.** A `ReadOnlyBlobDatabase` holds one lease from before it opens its documents until it is dropped:
   - *Take*: create the directory if missing; `create_new` `<id>.lease`; write the record with `seq` 0; take the lock
     (item 5); then check that the path still names this file (`Vfs::same_file`). If the lock is not granted, or the
     name is gone (the writer judged the new file stale before the lock was taken and removed it), the handle drops
     that file and starts again with a new id, at most 3 times; then the open fails. The lock is taken after the
     record is written, so a lease a writer finds locked always holds a whole record.
   - *Open*: `ReadOnlyBlobDatabase::open` takes the lease, then opens the documents, then writes their `seq` into
     the record. While `seq` is 0 the lease is older than every mark, so no sweep runs between the open and the
     record's update.
   - *Given documents*: `with_pack_beside` and `with_pack_in` receive documents opened before the lease existed.
     They take the lease, then read the container's committed seq again (open the container read-only at its path
     and read its superblock, as `persia verify` does, because a compaction renames a new file over the path). If the
     documents' seq is lower, a sweep may already have taken a blob they show: the call fails with a new error
     (`Error::OpenedBeforeLease { opened, current }`), and the caller opens the documents again. A new pair,
     `ReaderLease::take_beside(path)` and `ReadOnlyBlobDatabase::with_lease(lease, db, path)`, lets a caller take the
     lease first and leave no window; `persia verify` uses it.
   - *Refresh*: `refresh(reopened)` writes `reopened`'s seq into the record only after it succeeded and the previous
     documents are dropped, never before: until then the handle still holds the older documents, and the older seq
     is what protects them. A refresh that fails keeps the record. The lease is the handle's, not the pack handle's:
     a `PackReplaced` reopen (`PackStore::reopen`) keeps it.
   - *Drop*: the lease is declared after the documents and the pack, so it drops last. It removes its name(s), then
     releases the lock (`ContainerFile::unlock`, not only `close`: a forked child's copy of the descriptor would keep
     a `flock`, SPEC §4.5.1). A failed removal is logged; the unlocked file left is stale and the writer removes it.
5. **The lock** is the container's primitive. On Unix the lease file itself is locked (`ContainerFile::try_lock`, an
   advisory `flock` on the open file description). On Windows `LockFileEx` is mandatory, so a lock on the lease file
   would keep the writer from reading the record: the lock is `Vfs::lock_beside(<id>.lease)`, which creates and locks
   `<id>.lease.lock` (ADR-0016 item 3), and the record stays unlocked. The lease file is opened, written and read by
   the same handle for its whole life; the record is rewritten **in place** (one positioned write of 64 bytes at 0):
   a rename would replace the inode, and on Unix the lock would stay on the old one.
6. **Updates and torn reads.** A rewrite is not atomic against a concurrent read: the writer may read a record whose
   CRC does not match. That lease counts as live with an unknown `seq` (item 8), so that GC skips one round. A
   rewrite that fails (an I/O error) is logged and the handle goes on: the record left is the older seq or a torn
   one, both of which make GC keep more, never less; the next refresh writes it again.
7. **Durability.** Nothing about a lease is synced: not the record, not its name, not the directory. A lease only
   matters while its process lives, and a machine crash ends every process: every lease a crash leaves or loses is
   stale. This is not an `fsync later`: no acknowledged state depends on a lease.

### The writer's side

8. **The scan.** GC on the pack (`BlobDatabase::gc`, ADR-0052), after the mark and before the sweep, holding the
   pack's writer lock (`<name>.persia.blobs.lock` or the pack file, ROADMAP 9.2.15), lists the directory (none: no
   leases) and judges each `<id>.lease` by its lock, never by its record:
   - **Stale**: the writer gets the lock (Unix: `try_lock` on the file opened read-only; Windows: `lock_beside`). The
     holder is gone, whatever the record says or whatever its version: the writer removes `<id>.lease` (and on Windows
     `<id>.lease.lock`) while it holds the lock, then releases it. A failed removal is logged and retried by the next
     GC; a stale lease never holds GC back. A `<id>.lease.lock` with no `<id>.lease` is removed the same way.
   - **Live**: another handle holds the lock. The writer reads the record. Its `seq` counts; a record that does not
     decode (torn, damaged, another version, another `db_uuid` or `lease_id`) counts as live with an unknown `seq`,
     older than every mark, and is logged (`tracing::warn`).
   - **Unknown**: where locks belong to the process rather than the handle (a network or FUSE file system,
     `ContainerFile::locks_per_handle` is false, ADR-0019), a granted lock proves nothing: the lease counts as live
     with an unknown `seq`, and is not removed. There a lease left by a dead reader holds GC back until an operator
     removes it, and a directory listing cached by another client may miss a lease just created; SPEC §9.3 says
     both. Leases, like the pack, are meant for a local disk.
9. **The rule.** The mark's seq is the writer's committed seq, read while the log is frozen (`gc` borrows the
   database shared, so nothing is committed while it runs, ADR-0052 item 1). If a live lease's `seq` is below it, or
   unknown, the sweep does not run: no pack rewrite, nothing dropped. The report says so (`GcReport` gains the lease
   counts: live, behind, unreadable, stale removed, and the oldest `seq`; the rewrite is reported as held back by
   readers). Otherwise the sweep runs as before. The next `gc` decides again.
   Why one seq is enough: a reader whose documents are at the writer's committed seq shows only documents the mark
   keeps (the segments and tombstones of that seq, and WAL records from its `wal_start`, which the mark keeps whole,
   SPEC §9.5); a document committed later can only reference a blob bound or pinned after that GC. Why the lease
   must exist before the documents are opened: if the scan does not see a lease, that lease was created after the
   listing, so its documents are opened after it, while the log is frozen, at the mark's seq.
10. **The pin set and the mark are unchanged.** A lease is not a pin: it holds back the whole sweep, not one blob.
    In-process readers stay in the mark (ADR-0052 item 2); a read-only handle in the writer's process takes a lease
    like any other (on a local disk `flock` excludes another open file description of the same process).

### Edge cases

11. **A directory the reader cannot write.** When creating the directory or the lease fails because the directory
    refuses writes (permission denied, a read-only file system), the handle opens **without a lease**: it logs a
    warning and reports it (`ReadOnlyBlobDatabase::lease()` is `None`). Such a handle has today's risk (SPEC §9.5):
    a document it shows whose blob a writer elsewhere has collected reads as `BlobNotFound`, never as other bytes.
    Read-only media, a read-only mount of a snapshot and `persia verify` on one keep working. Any other error taking
    the lease fails the open.
12. **Windows.** As item 5: the record is never locked, the lock is the sidecar; a process that exits loses its
    locks. Names are unique per handle, so removing a stale lease's lock file does not race with a later taker, unlike
    the container's `<name>.lock` (ADR-0016 item 3), which is never removed.
13. **Remote stores** are out of scope: there is no pack, readers of the remote layout are covered by the grace
    period, and their leases are ROADMAP 13.2.4 and 13.2.5.
14. **Forward compatibility.** The lock is the liveness signal for every version: a future version may change the
    record (it is then unreadable here, so a live one holds GC back) but not the locking; a different protocol needs
    a different directory name.

### Tests (phase 2)

15. Unit tests of the codec (each check in order), a round-trip proptest and hostile input (every byte flip and
    truncation rejected, never a panic), a fuzz target `fuzz/fuzz_targets/reader_lease.rs`, golden files under
    `tests/fixtures/blob-lease/v1/` (a lease while opening, with `seq` 0, and one with a `seq` and `pid`). Behaviour:
    GC skips while a live older lease exists and sweeps once it is refreshed or dropped; a refresh advances the record
    only after it succeeds; a stale lease (a reader killed while holding it, a child process `kill -9`) is removed
    and does not hold GC back; a torn or foreign live record holds GC back; the take retries when the writer removed
    the new file; `with_pack_beside` refuses documents older than the container; a read-only directory opens without
    a lease.

## Consequences

- Every `ReadOnlyBlobDatabase` creates `<name>.persia.blobs.leases/` beside the pack the first time, even for a
  database that never stores a blob (a lease must predate the documents, and the pack may be created later), and a
  64-byte file while it lives.
- A reader that never refreshes holds GC back for as long as it lives, once the writer has committed past it. That is
  the decided trade-off: no per-blob delete-seq bookkeeping. A long-lived reader should refresh.
- A GC that is held back deletes nothing locally that round; the pack keeps growing until readers catch up.
- On a network or FUSE file system a dead reader's lease holds GC back until an operator removes it (item 8).
- A refresh costs one 64-byte positioned write; a GC costs one directory listing and one open, lock and read per
  lease.
- `with_pack_beside` and `with_pack_in` may now fail with `OpenedBeforeLease` when a writer committed between the
  documents' open and the lease.

## Alternatives considered

- **Heartbeat and TTL** (a reader rewrites a timestamp; GC ignores old ones): needs agreeing clocks and a period; a
  paused but live reader (a stopped process, a suspended VM) loses its protection, and a dead one holds GC back for a
  whole TTL. The lock is exact and free.
- **One shared lock on a single readers file** (readers take a shared lock, the writer tries an exclusive one): tells
  only that some reader lives, not what it holds, so GC would wait while any reader exists, even one that is up to
  date. Mandatory locks on Windows would also block reading such a file.
- **A reader-seq record in the container**: read-only handles would write the container, which breaks read-only
  opens of read-only media and adds write contention and a container format change for a cache-like fact.
- **Per-blob delete-seq bookkeeping** (GC drops a blob once every reader is past the commit that dropped its last
  reference): finer, GC is never held back entirely, but it needs persistent delete seqs per blob. Rejected for now
  (decided); the rule here can later be refined that way without changing the lease file.
- **Rewrite by write-to-temporary and rename**: atomic for readers of the record, but on Unix the lock would stay on
  the old inode and the new file would look stale.
- **Two record slots** (as the superblock): a torn update would fall back to the older slot instead of holding GC
  back one round. Not worth the complexity for a rare, harmless window.
- **One lease per process** shared by its handles: fewer files, but needs process-global state and a minimum over
  handles; one per handle is local and simpler.
- **An error when the lease cannot be created** (item 11): stricter, but read-only media and read-only mounts could
  no longer be read, for a risk that only shows as `BlobNotFound`.
- **Byte-range locks past the record on Windows** (mandatory locks cover only their range): needs `LockFileEx`
  through FFI (ADR-0016); the sidecar needs neither.
