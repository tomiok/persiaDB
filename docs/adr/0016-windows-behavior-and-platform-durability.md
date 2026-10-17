# ADR-0016: Windows behavior and platform durability

- Status: accepted
- Date: 2026-10-17
- Related: SPEC §4.5.1, §4.5.2, §4.6, §4.7, §10.1.1; amends ADR-0010 and ADR-0012; ROADMAP 2.3.9, 2.4.5.3, 2.4.8,
  2.4.16, 2.4.18

## Context

ADR-0010 and ADR-0012 left Windows questions open because no CI job compiled the Windows code: whether an open
mapping blocks the `rename` that compaction does (ADR-0010: "may prevent replacing it by `rename`. 2.4.5.3 verifies
… and makes compaction work around it"), and whether directories can be synced (ADR-0012: "Directories are not
fsynced on Windows yet … an acknowledged write on Windows may be lost in a machine crash"). The container's writer
lock, `LockFileEx` through `File::try_lock`, is mandatory on Windows, so it kept every reader out (SPEC §4.5.1). The
`windows-latest` CI job (ROADMAP 2.3.9) now runs the storage tests, and answers these.

## Decision

1. **Directory sync.** A directory is synced on Windows by opening it with `FILE_FLAG_BACKUP_SEMANTICS` and write
   access and calling `FlushFileBuffers` (no `unsafe`: the flag goes through `OpenOptionsExt::custom_flags`). CI
   verifies it on NTFS. A volume that cannot flush directories (FAT, some redirectors) fails the operation with an
   I/O error rather than acknowledging a write it cannot make durable. This supersedes ADR-0012's consequence that
   Windows directories are not synced. The whole sync table is SPEC §4.5.2.
2. **Mappings do not block compaction's rename.** Tested in CI (2.4.5.3): renaming a file over a mapped one succeeds
   with the mapped file's handle open or closed, and the mapping keeps the old bytes, because the standard library
   falls back to POSIX rename semantics (Windows 10 1607 and later, NTFS). Compaction needs no workaround there; on
   other file systems the rename may fail and compaction fails without harm. This answers ADR-0010's open point.
3. **Writer lock beside the container.** On Windows the writer lock is an exclusive lock on `<name>.lock` next to
   the container, created if missing and never removed (removing it would race with a writer taking it). Readers of
   the container are never blocked. A bare `File` cannot be locked (`ContainerFile::try_lock` is `Unsupported`), so
   `Container::open_on` read-write, `create_on` and `compact_into` on a `File` fail instead of running unlocked; the
   path API (`create`, `open`, `compact`) holds the lock beside the container. A create that finds the lock held
   reports `AlreadyExists`: a container is open, or being created, at that path.
4. **Readers racing a replace.** Opening an object that a concurrent `put` is replacing can briefly fail with
   `ERROR_ACCESS_DENIED` (the replaced file is pending deletion); the local backend retries the open.
5. **No file identity.** The standard library exposes none on Windows, so the checks that a path still names the
   file a handle has open reduce to "the path names something" (ROADMAP 2.4.18 tracks a real check, which needs FFI).

## Consequences

- The local backend's durability guarantee (SPEC §10.1.1) holds on Windows as on Unix.
- A `<name>.lock` file accompanies every container opened for writing on Windows.
- Windows code is compiled, linted and tested on every pull request.

## Alternatives considered

- **Lock a byte range past the data** (mandatory locks only cover their range, so readers would not be blocked):
  needs `LockFileEx` through FFI, i.e. `unsafe` and a new dependency or bindings; the sidecar needs neither.
- **Keep directories unsynced on Windows** (ADR-0012's interim state): NTFS journals metadata lazily, so an
  acknowledged write could be lost in a machine crash.
- **Close mappings or defer compaction until readers drain**: unnecessary, given decision 2.
