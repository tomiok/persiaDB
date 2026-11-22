# ADR-0054: Reader leases on the local blob pack

- Status: proposed
- Date: 2026-11-21
- Related: SPEC §4.6, §4.8, §8.4, §8.6, §9.3, §9.5, §9.6, §18.10; ROADMAP 9.2.13, 9.2.15, 9.2.16, 13.2.4,
  13.2.5; ADR-0016, ADR-0019, ADR-0021, ADR-0049, ADR-0052, ADR-0053

## Context

GC's mark (SPEC §9.5, ADR-0052 item 2) sees only the readers of the writer's own process. A read-only handle
(`ReadOnlyBlobDatabase`) in another process, or a second read-only handle in the writer's process, holds documents of
an older commit than the writer's. When the writer deletes a document, commits, and GC rewrites the pack without its
blob, that reader still shows the document, and once its `refresh` reopens the rewritten pack
(`PackReplaced`, `PackStore::reopen`, ROADMAP 9.2.13) `get_blob` fails with `BlobNotFound` (SPEC §9.3, §9.5). The
writer must keep every blob a live reader may still reference.

The user decided (2026-11-21): one reader lease file per read-only handle, beside the pack, held under an OS advisory
lock for the handle's life and holding the manifest seq of the documents it holds; a reader that dies loses its lock,
and the writer removes a reader lease it can lock; no wall clock, no heartbeat. GC skips its sweep (and so the pack
rewrite) while a live reader lease is behind the mark, with no per-blob bookkeeping. This ADR fixes the file, the
protocol, and the edge cases. It is an on-disk format (a new file kind), so it follows the format discipline: SPEC
text, its own magic and version, this ADR, and golden files (item 17).

## Decision

### Where and what

1. **Location.** `<pack>.leases/`, derived from the pack's path: `<name>.persia.blobs.leases/` for the pack
   `<name>.persia.blobs` that `open` and `with_pack_beside` find from the container's canonical path (SPEC §9.3), and
   `<pack>.leases` for the pack `with_pack_in` is given, so the writer and every reader of one pack agree, through a
   symbolic link too. A reader creates the directory if it is missing (`AlreadyExists` is success); nothing removes
   it. It is not a temporary: a sweep of side files (`Container::side_files`, `persia container sweep`) leaves it and
   everything in it alone. A copy of the database (SPEC §4.8) need not take it: a copied reader lease is unlocked, so
   the copy's writer removes it as stale (item 8).
2. **Names.** One file per read-only handle: `<lease_id>.lease`, `lease_id` a random `u128` (drawn as a `pack_id` is)
   written as 32 lowercase hex digits. On Windows its lock lives beside it, `<lease_id>.lease.lock` (item 5). Any
   other name in the directory, and any entry of those names that is not a regular file (a directory, a symbolic
   link, a FIFO, which an open would block on), was not written by Persia: never opened, locked or removed, as
   foreign remote keys are not (SPEC §9.4).
3. **Record.** A reader lease file holds exactly one 64-byte record, little-endian (SPEC §4.1 rules), with its own
   magic and version:
   ```
   0   4   magic         bytes "PRLS" (0x50 0x52 0x4C 0x53)
   4   4   version       u32 = 1
   8   4   flags         u32 = 0 (no bit assigned)
   12  4   pid           u32: the holder's process id, for operators only (never trusted)
   16  16  db_uuid       the container's db_uuid; all zero while the handle opens its documents
   32  16  lease_id      u128, the id in the file's name
   48  8   manifest_seq  u64: the committed seq of the documents the handle holds; 0 while it opens them
   56  4   reserved      u32 = 0
   60  4   crc32c        u32 over bytes 0..60
   ```
   **Opening** is `db_uuid` all zero (and then `manifest_seq` must be 0): the handle has not opened its documents yet.
   **Open** is any other `db_uuid`, with the seq of its documents, which may be 0 (a database that never committed).
   So each state has one encoding; a zero `db_uuid` with a non-zero `manifest_seq` is corruption.
   Decoding checks, in this order: at least 8 bytes, the magic, `version` (0 is corruption, above 1 is unsupported:
   a later version may be longer), then exactly 64 bytes, the CRC32C, `flags` (any bit is unsupported), `reserved`
   zero, the opening rule above; then, against the context, `lease_id` against the name and an open `db_uuid`
   against the pack's. Anything else that does not hold is corruption (`Error::Corrupt`, `Error::UnsupportedVersion`,
   `Error::UnsupportedFlags`). The writer reads at most 4096 bytes of a reader lease, so a hostile file costs one
   bounded read. Codec and protocol live in `persia-blob` (`pack/lease.rs`, beside the pack layout); the GC rule
   (item 9) in `persia::internal::gc`.

### The reader's side

4. **Life cycle.** A `ReadOnlyBlobDatabase` holds one reader lease from before it opens its documents until it is
   dropped:
   - *Take* (`ReaderLease::take`): create the directory if missing; `create_new` `<id>.lease`; write the opening
     record; take the lock (item 5); then check that the path still names this file (`Vfs::same_file`). Then open the
     container read-only at its path and read its committed seq from the superblock (as `persia verify` does: a
     compaction renames a new file over the path, SPEC §4.6), and keep it as the lease's **taken seq**. If the lock is
     not granted, its sidecar cannot be opened (Windows: a stale removal's delete pending, item 12), or the name is
     gone (the writer judged the new file stale before the lock was taken and removed it), the handle drops that file
     and starts again with a new id, at most 3 times; then the take fails with `Error::LeaseContended`. The lock is
     taken after the record is written, so a reader lease a writer finds locked always holds a whole record.
   - *Open*: `ReadOnlyBlobDatabase::open(path)` canonicalizes `path`, takes the reader lease beside its pack, opens
     the documents, checks them (below), then writes the open record (`db_uuid`, `manifest_seq`).
   - *Given documents*: `with_lease(lease, db, path)` (and `with_pack_beside`, `with_pack_in`, which take a lease
     first and then call it) checks, before anything else:
     - `db.seq() >= taken seq`, else `Error::OpenedBeforeLease { opened, taken }`: a GC may have swept a blob those
       documents show between their open and the lease. **Equal is safe** whenever the documents were opened: every
       GC that ran before the lease existed had a mark seq at most the taken seq, which is the documents' seq, and a
       GC whose mark is not newer than a reader's documents sweeps no blob they show (item 9, "why one seq is
       enough");
     - the lease's directory is `<pack>.leases` of the pack these documents use (the one derived from
       `db.container()`'s canonical path, or the pack `with_pack_in` is given), else `Error::LeaseElsewhere`. The
       same check runs after `open`'s document open: a symbolic link retargeted between the canonicalization and
       the open would otherwise lease one pack and read another.
     `persia verify` takes the lease first and uses `with_lease`, so it has no window.
   - *Refresh*: `refresh(reopened)` writes `reopened`'s seq into the record only after it succeeded and the previous
     documents are dropped, never before: until then the handle still holds the older documents, and the older seq is
     what protects them. A refresh that fails keeps the record. The reader lease is the handle's, not the pack
     handle's: a `PackReplaced` reopen (`PackStore::reopen`) keeps it.
     **Not protected: a snapshot kept across a refresh** (decided 2026-11-21). A `Snapshot` taken from `database()`
     is an `Arc` that can outlive the `ReadOnlyDatabase` a refresh replaced, and the record then names the newer
     documents only. A GC may then sweep a blob only such a snapshot still shows: reading it fails with
     `BlobNotFound`; it never returns other bytes. The handle does not track those snapshots (no weak references, as
     the writer keeps, ADR-0052 item 2); a follow-up leaf may.
   - *Drop*: the reader lease is declared after the documents and the pack, so it drops last. It removes its
     name(s), then releases the lock explicitly (`ContainerFile::unlock`, not only `close`: a forked child's copy of
     the descriptor would keep a `flock`, SPEC §4.5.1). A failed removal is logged; the unlocked file left is stale
     and the writer removes it.
5. **The lock** is the container's primitive. On Unix the reader lease file itself is locked
   (`ContainerFile::try_lock`, an advisory `flock` on the open file description). On Windows `LockFileEx` is
   mandatory (the standard library locks the whole range), so a lock on the file would keep the writer from reading
   the record: the lock is `Vfs::lock_beside(<id>.lease)`, which creates and locks `<id>.lease.lock` (ADR-0016 item
   3), and the record stays unlocked. The file is opened, written and read by the same handle for its whole life; the
   record is rewritten **in place** (one positioned write of 64 bytes at 0): a rename would replace the inode, and on
   Unix the lock would stay on the old one.
6. **Updates and torn reads.** A rewrite is not atomic against a concurrent read: the writer may read a record whose
   CRC does not match. That reader lease counts as live and unreadable (item 8), so GC holds back one round. A rewrite
   that fails (an I/O error) is logged and the handle goes on: the record left is the older seq or a torn one, both of
   which make GC keep more, never less; the next refresh writes it again.
7. **Durability.** Nothing about a reader lease is synced: not the record, not its name, not the directory. It only
   matters while its process lives, and a machine crash ends every process: every reader lease a crash leaves or
   loses is stale. This is not an `fsync later`: no acknowledged state depends on a reader lease.

### The writer's side

8. **The scan.** GC on the pack (`BlobDatabase::gc`, ADR-0052), after the mark and before the sweep, holding the
   pack's writer lock (ROADMAP 9.2.15), lists the directory (missing: no reader leases).
   - **First, whether locks can be judged at all**, from the pack file the writer holds open
     (`ContainerFile::locks_per_handle`), before any reader lease is opened. Where locks belong to the process rather
     than the handle (ADR-0019), a lock the writer is granted proves nothing, and opening then closing a reader
     lease file in the writer's process would drop the lock of a read-only handle in that process (any close drops a
     process's POSIX locks). There every listed `<id>.lease` counts as live and unreadable, without being opened, and
     nothing is removed.
   - Otherwise each entry that is a regular file named `<id>.lease` (`DirEntry` kind `File`; item 2) is judged by its
     lock, never by its record:
     - **Gone**: `NotFound` on open (removed since the listing, by its holder's drop or by an earlier step): skipped.
     - **Not openable**: `PermissionDenied` on open (another user's file the writer may not read) or any other open
       error: live and unreadable. It holds GC back; it is never a GC failure.
     - **Stale**: the writer is granted the lock (Unix: `try_lock` on the file opened read-only; Windows:
       `lock_beside`). The holder is gone, whatever the record says or its version: the writer removes `<id>.lease`
       (and on Windows `<id>.lease.lock`) while it holds the lock, then unlocks explicitly (`ContainerFile::unlock`,
       also when a removal failed, as `persia-storage`'s side-file sweep does). A failed removal is logged and retried
       by the next GC; a stale reader lease never holds GC back. A `<id>.lease.lock` with no `<id>.lease` is removed
       the same way.
     - **Live**: the lock is held elsewhere (`WouldBlock`). The writer reads the record and decodes it.
     - **Lock error**: any other `try_lock` error (`EBADF`, `ENOLCK`, an emulation's failure): live and unreadable,
       never a GC failure.
     A live record that does not decode (torn, damaged, another version, another `db_uuid` or `lease_id`) is live and
     unreadable, and is logged (`tracing::warn`).
9. **The rule.** The mark's seq is the writer's committed seq, read while the log is frozen (`gc` borrows the
   database shared, so nothing is committed while it runs, ADR-0052 item 1). A live reader lease **lets the sweep run
   only if it is open and its `manifest_seq` equals the mark's seq**. It holds GC back when it is opening (whatever
   the mark's seq, 0 included: compared by state, not by seq), unreadable, behind (`manifest_seq` below the mark's),
   or ahead (above it: not this writer's history, a restored or copied database, so not trusted; logged with the
   lease's path, `tracing::warn`). When any live reader lease holds GC back, the sweep does not run: no pack
   rewrite, nothing dropped. The report says so: `GcReport` (made `#[non_exhaustive]`) gains the reader lease
   counts (live, opening, behind, ahead, unreadable, stale removed, the oldest `manifest_seq`), and the rewrite is
   reported as held back by readers. The next `gc` decides again.
   **Why one seq is enough**: a reader whose documents are at the writer's committed seq shows only documents the
   mark keeps (the segments and tombstones of that seq, and WAL records from its `wal_start`, which the mark keeps
   whole, SPEC §9.5); a document committed later can only reference a blob bound or pinned after that GC.
   **Why the lease must exist before the documents are opened**: if the scan does not list a reader lease, it was
   created after the listing, so its documents are opened after it, while the log is frozen, at the mark's seq.
   **GC does not sweep while any reader lease is behind the committed seq** (decided 2026-11-21). Every commit puts
   every read-only handle in another process behind until it refreshes, and so does a container compaction, which
   commits a new seq without dropping a document (SPEC §4.6 step 3; also at merge steps when automatic). A writer that
   runs delete, commit, `gc` therefore does not sweep while such a reader lives and has not refreshed; `GcReport`
   says the sweep was held back by reader leases. A sweep over two GC rounds (remembering what the first round would
   drop) is a follow-up leaf.
10. **The pin set and the mark are unchanged.** A reader lease is not a pin: it holds back the whole sweep, not one
    blob. In-process readers stay in the mark (ADR-0052 item 2); a read-only handle in the writer's process takes a
    reader lease like any other (on a local disk `flock` excludes another open file description of the same process).

### Edge cases

11. **A directory the reader cannot write.** Only when creating the directory (`create_dir`) or the reader lease
    file (`create_new`) fails with permission denied (`EACCES`, `EPERM`) or a read-only file system (`EROFS`), the
    handle opens **without a reader lease**: it logs a warning and reports it (`ReadOnlyBlobDatabase::lease()` is
    `None`). Such a handle has today's risk (SPEC §9.5): a document it shows whose blob a writer elsewhere has
    collected reads as `BlobNotFound`, never as other bytes. Any other error taking the reader lease, including a
    `PermissionDenied` from the lock's sidecar (item 12), is not this case: it retries or fails the open.
    Decided 2026-11-21: the handle opens without a reader lease and logs a `tracing` warning that names the path and
    says that GC may sweep blobs this reader still references. The `persia` facade does not exist yet (M10; the
    crate exposes only its hidden `internal` module), so `lease()` reaches no public API today; M10 must expose it.
12. **Windows.** As item 5: the record is never locked, the lock is the sidecar; a process that exits loses its
    locks. On the pinned toolchain (1.99.0) `std::fs::remove_file` calls `DeleteFileW`, and only when that fails with
    `ERROR_ACCESS_DENIED` retries with POSIX delete semantics on a handle opened for `DELETE`. The standard library
    opens files with `FILE_SHARE_DELETE`, so deleting an open file succeeds, but whether its name disappears at once
    or stays **delete pending** until the last handle closes depends on the Windows version and file system; opening
    a delete-pending name fails with access denied. So while the writer removes a stale `<id>.lease.lock` it still
    holds, the owner's `lock_beside` of that name may get `PermissionDenied`: that is "lock not granted, start again
    under a new id" (item 4), never "the directory refuses writes" (item 11). Names are unique per handle, so removing
    a stale reader lease's lock file does not race with a later taker, unlike the container's `<name>.lock` (ADR-0016
    item 3), which is never removed.
13. **File systems where locks cannot be judged.** `ContainerFile::locks_per_handle` is true only on a local disk
    file system the build knows (ADR-0021; on Linux ext2/3/4, XFS, Btrfs, F2FS, ZFS, bcachefs, JFS, ReiserFS, NILFS,
    tmpfs, ramfs). Everything else counts as unknown, including overlayfs (a Docker container's own file system),
    vfat and exFAT, 9p (WSL2's `/mnt/c`), virtiofs, NFS, CIFS and FUSE. There (item 8) every reader lease counts as
    live: **one reader that crashed holds GC back for good**, until an operator removes its file. A directory listing
    cached by another client may also miss a reader lease just created. The operator path is a CLI command (a
    ROADMAP leaf to add): it runs in a process of its own that holds no reader lease, so a lock it is granted does
    show that no other process holds it, and it can remove stale reader leases there; it can also remove one by id.
14. **Remote stores** are out of scope: there is no pack, readers of the remote layout are covered by the grace
    period, and their leases are ROADMAP 13.2.4 and 13.2.5.
15. **Forward compatibility.** The lock is the liveness signal for every version: a future version may change the
    record (it is then unreadable here, so a live one holds GC back) but not the locking; a different protocol needs
    a different directory name.
16. **What this does not protect against.** A writer binary from before this ADR ignores reader leases and sweeps as
    it did (the container's format does not change, so nothing stops an older writer). A process that forks a child
    which does not `exec` shares the open file description, so the child keeps the reader lease live until it exits
    or closes the descriptor: GC is held back meanwhile (it keeps more). An `exec` closes it (files are opened
    close-on-exec).

### Format discipline and tests (phase 2)

17. **Format discipline.**
    - SPEC: §9.3 (the location, names, record, decode order, take and drop, the Windows sidecar, the read-only
      directory case, file systems where locks cannot be judged); §9.5 (the scan's outcomes and the rule; the
      read-only handle sentence); §9.6 (the reader lease of `ReadOnlyBlobDatabase`, `with_lease`, and the errors
      `OpenedBeforeLease`, `LeaseElsewhere` and `LeaseContended`, the last when the take's retries run out); §8.6 (a
      read-only open of the engine still writes nothing, but a `ReadOnlyBlobDatabase` and `persia verify` create the
      directory and a reader lease file beside the pack); §4.8 (a copy need not take `<name>.persia.blobs.leases/`);
      §18.10 (c) (readers of another process hold reader leases).
    - Its own magic `"PRLS"` and version 1 (item 3).
    - This ADR.
    - Golden files: `tests/fixtures/blob-lease/v1/` holds an opening record and an open one with a `pid` and a
      `manifest_seq`, checked byte for byte by `golden_fixtures_are_byte_for_byte_stable` in
      `crates/persia-blob/tests/lease_format.rs`, built there as `pack_format.rs` builds the pack's (a missing one is
      written with `PERSIA_WRITE_BLOB_LEASE_FIXTURES=1`, never regenerated once released, SPEC §15).
18. **Tests.**
    - Codec: each check in its order; the opening record round trips; a zero `db_uuid` with a non-zero
      `manifest_seq` is corruption; an open record with `manifest_seq` 0 round trips; a round-trip proptest; hostile
      input (every byte flip and truncation rejected, never a panic); the fuzz target
      `fuzz/fuzz_targets/reader_lease.rs`
      (`fuzz/` reaches `persia-blob`, not `persia`).
    - Reader: the take retries when the writer removed the new file, and fails with `LeaseContended` after 3;
      `with_lease` refuses documents older than the taken seq and a lease of another pack's directory, and takes equal
      and newer ones; a retargeted symbolic link between canonicalization and open is `LeaseElsewhere`; a refresh
      advances the record only after it succeeds; a read-only directory opens without a lease.
    - Writer: GC holds back while a live reader lease is behind, ahead, opening, or unreadable (torn, foreign), and
      sweeps once it is refreshed or dropped; delete, commit, `gc` with a reader that refreshes: held back, then sweeps
      on the next `gc`; a stale reader lease (a reader killed while holding it, a child process `kill -9`) is removed
      and does not hold GC back; FIFO, symbolic link and directory entries named `<id>.lease` are ignored, without a
      hang; a `try_lock` error and an entry gone mid-scan (a simulated `Vfs`) are live-unreadable and skipped; with
      `locks_per_handle` false and a read-only handle in the writer's process, nothing is opened or removed and GC
      holds back; on Windows, a take racing a stale removal starts again.

## Consequences

- Every `ReadOnlyBlobDatabase` creates `<name>.persia.blobs.leases/` beside the pack the first time, even for a
  database that never stores a blob (a reader lease must predate the documents, and the pack may be created later),
  and a 64-byte file while it lives. An engine read-only open still writes nothing.
- A reader that never refreshes holds GC back for as long as it lives, once the writer has committed past it, and so
  does every reader right after a commit or a compaction until it refreshes (item 9). That is the decided trade-off:
  no per-blob delete-seq bookkeeping.
- A GC that is held back deletes nothing locally that round; the pack keeps growing until readers catch up.
- On file systems where locks cannot be judged (item 13: Docker overlayfs, WSL2 `/mnt/c`, network and FUSE file
  systems) a dead reader's lease holds GC back until an operator removes it.
- A refresh costs one 64-byte positioned write; a reader lease take costs a container open to read its seq; a GC
  costs one directory listing and one open, lock and read per reader lease.
- `with_pack_beside` and `with_pack_in` may now fail with `OpenedBeforeLease` when a writer committed between the
  documents' open and the lease; `with_lease` has no such window.

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
  (decided); the rule here can later be refined that way without changing the reader lease file.
- **Rewrite by write-to-temporary and rename**: atomic for readers of the record, but on Unix the lock would stay on
  the old inode and the new file would look stale.
- **Two record slots** (as the superblock): a torn update would fall back to the older slot instead of holding GC
  back one round. Not worth the complexity for a rare, harmless window.
- **One reader lease per process** shared by its handles: fewer files, but needs process-global state and a minimum
  over handles; one per handle is local and simpler.
- **A flags bit for the opening state** instead of the zero `db_uuid`: equivalent; the zero `db_uuid` keeps every
  flag unassigned and makes a record that claims a database also name it.
- **An error when the reader lease cannot be created** (item 11): stricter, but read-only media and
  read-only mounts could no longer be read, for a risk that only shows as `BlobNotFound`.
- **Byte-range locks past the record on Windows** (mandatory locks cover only their range): needs `LockFileEx`
  through FFI (ADR-0016); the sidecar needs neither.
