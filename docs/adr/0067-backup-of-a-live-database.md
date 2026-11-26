# ADR-0067: Backing up a live database: committed images under the writer mutex, copied without it, linked pack first

- Status: proposed
- Date: 2026-11-25
- Related: SPEC §4.5.2, §4.6, §4.7, §4.8, §9.3, §9.5; ROADMAP 10.1.8; ADR-0016, ADR-0049, ADR-0052, ADR-0064

## Context

A database is two files (SPEC §4.8): the container and, once a blob was stored, the blob pack. SPEC §4.8 described
three safe manual copies; the third, with the writer open, needs the container copied before the pack and no
compaction or GC rewrite until both copies end, and even then a byte copy of a file being appended to may end in a
torn tail and holds the WAL frames written after the last commit. ROADMAP 10.1.8 asks for `Db::backup(dest)`: both
files, consistent, at one commit, with the writer open and not blocked for the copy, durable, and never a half
backup under `dest`.

## Decision

1. **Committed images, not byte copies of the files.** A `CommittedImage` (`persia-storage`) is the 12288-byte
   prefix (the header page and **both** slots holding the commit, as a compaction writes them, SPEC §4.6)
   and a second handle on the file the commit was made in (`ContainerFile::try_clone_handle`, no lock), from which
   `12288..log_end` is copied in 64 KiB chunks. Below a committed `log_end` nothing is written again (SPEC §4.7) and
   the in-place slots are never read from the file, so the copy is the commit whatever the writer does meanwhile,
   with nothing after `log_end`: no WAL frame of a later write, no chunk of an unbound upload, no torn tail.
   `Container::committed_image` and `PackStore::committed_image` (the latter under the pack's writer mutex) take
   them; the engine offers the container's on `Database` and `ReadOnlyDatabase`.
2. **Taken under the writer mutex, copied without it.** A read-write `Db::backup` takes the writer mutex for a
   commit's time: a commit (so every write that returned is in the copy), a `CompactionHold` (engine: automatic
   compaction skips while one lives) and a `GcHold` (`BlobDatabase::gc` marks and sweeps nothing while one lives),
   the container's image, then the pack's (the order SPEC §4.8 requires: a blob is bound before any record that
   references it). The copy then runs with the mutex released. The holds are dropped when the copy ends. The second
   handles alone would keep the copy right through a rename over the path (on Unix, and on Windows where the
   standard library opens files with `FILE_SHARE_DELETE` and renames with POSIX semantics), but the holds keep what
   SPEC §4.8 promised, and spare FAT volumes, where `MoveFileExW` may refuse to replace an open file (SPEC §4.7).
3. **Read-only databases back up too**, the state they show: the last commit of the container their state was read
   from and their pack handle's committed state, under their own state lock (a refresh waits). They hold nothing in
   another process: its compaction or GC renames a new file over the path, which their handles do not follow. What
   their open replayed after that commit is not copied.
4. **Written new, linked in order, durable.** `write_backup_in` (`persia-storage`, generic over the `Vfs`, so
   crash-tested on `SimFs`) refuses a destination that exists before writing anything; writes each image to a
   temporary name `<name>.backup.<nonce>.<n>` beside its destination and syncs it; hard-links each to its name (which
   fails if the name appeared) and syncs the directory before the next link, **the pack first**; then removes the
   temporary names and syncs the directory. A crash leaves a prefix of the files linked, each complete: `dest` never
   exists without its pack. A failure removes what it linked. Hard links are required, as for `Container::create`.
5. **`dest` is never replaced** (SPEC did not say): `AlreadyExists` if anything is at `dest` or `<dest>.blobs`. Its
   directory must exist; it is not created. `dest` is a path or a `file://` URL, read as `Db::open` reads one.
6. **Amended in review (2026-11-25).**
   - The header page is copied as the file holds it, read when the image is taken (no writer of this version rewrites
     it in place), not encoded again: a read-only handle of a file with `rocompat_flags` this build does not know
     backs it up unchanged. Only the slots are written from memory.
   - **Locks that belong to the process** (`locks_per_handle` false, NFS-style emulated `flock`, ADR-0019): closing
     any descriptor of the container drops its writer lock, so the container's second handle is opened once, kept in
     the container, and closed only when the container drops, after its lock is released. The pack's second handle
     closes no lock: its writer lock is the file beside it.
   - **Undo in the reverse order of the links**: the container's link removed first, the directory synced, then the
     pack's; the first removal or sync that fails stops the undo and leaves the earlier files, complete. So the names
     left are, at every instant and across a crash, a prefix of the link order: never `dest` without its pack.
   - **After both links**, a temporary name that cannot be removed is logged and left (it is not part of the backup)
     and the backup succeeds; the last directory sync failing is an `Io` error whose message says the backup was
     written (both files complete, maybe not durable). An error therefore does not mean `dest` is free.
   - The destination check does not follow a symbolic link (a dangling link is a name that exists). A poisoned
     writer cannot commit, so its backup fails with `Poisoned`. Checksums are not checked on the way: `verify` the
     copy to check it.
   - A file system without hard links (FAT32, exFAT) refuses the backup with `Io`: ROADMAP 10.1.28.

## Consequences

- A backup costs a commit and two in-memory prefixes under the writer mutex; the copy is read through the page cache
  beside the writer, which only waits for that commit.
- While a backup copies, a due compaction waits for a later merge step and a GC reports `held_by_copy` without
  sweeping; neither fails.
- A crash between the two links leaves `<dest>.blobs` without `dest`; the next backup to that name refuses it until
  it is removed. Stale temporary files are left like a crashed compaction's.
- A backup of a database without a pack is one file, and its copy creates its pack lazily like any database.
- The copy has the source's `db_uuid`; opening both side by side is opening a copy (SPEC §9.3 on refresh).

## Alternatives considered

- **Copy the files byte for byte while holding the writer mutex.** Consistent and simple, but blocks every write for
  the length of the copy.
- **Byte copies with the mutex released** (SPEC §4.8's third manual way). Copies the WAL after the last commit and a
  possibly torn tail, and reads the slots while a commit rewrites them: the copy is a crash state, not one commit.
- **Rename the temporary files over `dest`.** Replaces a file that appeared at `dest` meanwhile; a link fails instead.
- **The container first.** A crash between the links would leave a container whose blobs are missing.
