# ADR-0018: Stale temporary files are swept under their lock, on request only

- Status: proposed
- Date: 2026-10-17
- Related: SPEC §4.5.1, §10.1.1; amends ADR-0012 (consequence "a crash can leave temporary files behind");
  ADR-0014, ADR-0016; ROADMAP 2.3.8, 2.4.15, 2.4.19

## Context

A crashed `put` of the local store leaves an `@tmp.<nonce>.<n>` file next to the object it was writing (ADR-0012),
and a crashed create or compaction of a container leaves a `<name>.tmp.*` or `<name>.compact.*` file next to it.
None of them holds acknowledged data, and readers skip them, but they take space for ever. ADR-0012 left open which
process may delete them and when. The container's sweep (ROADMAP 2.4.15) already answered this for its own files:
the writer that holds the container's lock removes the temporary files whose own lock is free.

The local store has no store-wide writer lock: any number of processes may `put` concurrently. Deleting another
process's temporary file never loses data (that `put` fails at its rename or link, and was never acknowledged), but
it does make a live write fail. Process ids cannot tell live writers from dead ones (ADR-0012: pid 1 in every
container, separate pid namespaces on one volume), and file ages depend on clocks and on how long a large write
takes.

## Decision

1. **The lock tells.** A `put` takes an exclusive lock on its temporary file (`flock` on Unix) right after creating
   it, and holds it until the temporary name is gone (renamed, or removed after `put_if_absent`'s link). A sweep
   opens each temporary file, takes its lock without waiting, and removes the file **while holding the lock**; a
   file whose lock is held is in use and kept. The lock dies with its process, so a crashed writer's file is
   immediately sweepable.
2. **The handshake closes the race.** Between creating its file and locking it, a `put` can lose it to a sweep. After
   taking the lock it checks that the name still names its file (`Vfs::same_file`); if the lock was held by a sweep,
   or the name is gone, it takes another name. Every `put` that reaches its rename therefore holds the lock on the
   file the name designates, and no sweep can remove it. A listing (`temporaries`, `Container::side_files`) also
   takes each lock for an instant; a `put` that meets a held lock removes its own just-created file before taking
   another name, so neither a listing nor a sweep leaves a file behind or makes a `put` fail.
3. **On request only.** Sweeping walks the whole store, so `LocalStorage::open` never does it.
   `LocalStorage::sweep_temporaries` and `temporaries` (list only) are called by an operator or a maintenance job,
   e.g. `persia store sweep` (ADR-0019). The container's `sweep_stale_files` stays as it is (2.4.15), and
   `Container::side_files` lists a container's temporary and quarantine files without opening it.
4. **Windows.** File locks there are mandatory (`LockFileEx`), so neither `put` nor a sweep takes them (ADR-0016):
   whether a temporary file is in use is unknown. A local store's sweep then removes nothing unless forced, which an
   operator does only while no other process writes to the store. The container's sweep needs no check on Windows:
   it holds the container's writer lock, which excludes the creates and compactions that make those files.
5. **Quarantine copies are never swept as stale.** They may hold acknowledged writes (ADR-0014). They are listed with
   the temporary files and removed only by `Container::remove_quarantine_files`, through a read-write handle, at an
   operator's explicit request, and only the copies named: `persia container sweep --quarantine` names those listed
   before its own open, never one that open's recovery just made.

## Consequences

- A crash's leftovers are bounded by how often an operator sweeps; nothing grows without limit once sweeping is
  scheduled, and nothing is removed behind anyone's back.
- Each `put` makes two more system calls (lock and an identity check by path); the lock is released when its handle
  is closed.
- A sweep that takes a file in the instant before its writer locks it costs that writer one more temporary name,
  never an error.
- On Windows a local store's temporary files accumulate unless an operator forces a sweep while the store is idle.
- **The handshake assumes `flock` locks per open file description**, as on local Linux and macOS file systems. Where
  `flock` is emulated with per-process POSIX locks (Linux NFS, some FUSE and CIFS mounts), a sweep in the same
  process as a `put` can take that `put`'s file and the `put` fails at its rename. No data is lost (it was never
  acknowledged), but such stores should be swept only while idle, until 2.3.10's file-system detection can tell.

## Alternatives considered

- **Age threshold** (remove `@tmp` files older than a grace period): portable, but it needs file times in the VFS and
  trusts clocks; a slow write of a large object can outlive any fixed threshold and fail.
- **Process ids in the name, checked for liveness**: wrong across pid namespaces and after pid reuse (ADR-0012).
- **Sweep at open**: frees space with no operator action, but every open, in every process, walks the whole tree.
- **A store-wide writer lock**: would make sweeping trivial, but the local store is shared by design (SPEC §10.1).
