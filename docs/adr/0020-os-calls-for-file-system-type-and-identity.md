# ADR-0020: `libc` and `windows-sys` for the file system type and the Windows file identity

- Status: proposed
- Date: 2026-10-18
- Related: SPEC §4.5.1, §4.7; amends ADR-0016 (decision 5) and ADR-0017 (decision 3); ADR-0001, ADR-0010;
  ROADMAP 2.3.10, 2.4.18

## Context

Two questions about an open file need the operating system, and the standard library answers neither:

- **Is it on a local disk?** Mapped reads (ADR-0017) are safe to turn on where a page fault reads a local device; on
  a network or FUSE file system a server or a user-space process that fails turns into `SIGBUS` (Unix) or
  `EXCEPTION_IN_PAGE_ERROR` (Windows), which ends the process, where `pread` would return an error. Answering needs
  `fstatfs` (`f_type` on Linux, `MNT_LOCAL` on macOS) or `GetDriveTypeW` on Windows (ROADMAP 2.3.10).
- **Does a path still name the file a handle has open?** Compaction checks it before renaming over the container
  (SPEC §4.5.1, §4.6), and the local store checks it after locking a temporary file (ADR-0018). Unix has
  `(st_dev, st_ino)` in std; on Windows the volume serial number and file id are behind the unstable
  `windows_by_handle` feature (still unstable in Rust 1.99), so the check was reduced to "the path names something"
  (ADR-0016 decision 5, ROADMAP 2.4.18).

Both need `unsafe` foreign calls. Declaring them by hand means getting struct layouts right per OS and architecture
(`struct statfs` differs on every Unix); `libc` and `windows-sys` are the maintained bindings, and both are already
in the build graph (`libc` through `memmap2`, `windows-sys` through `clap`).

## Decision

1. **Dependencies.** `persia-storage` depends on `libc` (`cfg(unix)`) and `windows-sys` (`cfg(windows)`, features
   `Win32_Foundation`, `Win32_Storage_FileSystem`, `Win32_System_WindowsProgramming`), declared in the workspace
   allowed list, and `cargo xtask check-deps` rejects them in any other crate. They are raw bindings only: no behavior
   comes with them.
2. **One `unsafe` module.** Every new foreign call is in `persia_storage::sys` (`crates/persia-storage/src/sys.rs`),
   listed in `UNSAFE_ALLOWED`; each `unsafe` block states the invariant it relies on (valid buffers of the right
   size, the handle or descriptor open for the call, the result initialized only on success).
3. **Local disk.** `sys::is_local_disk(&File)`:
   - Linux and Android: `fstatfs`, then an allow-list of `f_type` magic numbers: ext2-4, XFS, Btrfs, F2FS, ZFS,
     bcachefs, JFS, ReiserFS, NILFS, overlayfs, tmpfs, ramfs. FAT and exFAT are left out (mostly removable media,
     which Linux cannot tell from `fstatfs`), and every network and FUSE type is left out.
   - macOS: `fstatfs`, local if `MNT_LOCAL` is set and the type is not macFUSE/osxfuse, nor FAT or exFAT (mostly
     removable media, which `MNT_LOCAL` does not exclude).
   - Windows: the handle's final path (`GetFinalPathNameByHandleW`), its volume (`GetVolumePathNameW`), and
     `GetDriveTypeW`: local for `DRIVE_FIXED` and `DRIVE_RAMDISK` only (so not removable, remote or optical drives).
   - Any other OS: never local.

   An unknown answer is "not local": the cost of a wrong "no" is `pread`, of a wrong "yes" a crash. A probe that
   fails (a Windows volume mounted without a drive letter, where `GetFinalPathNameByHandleW` has no DOS path) is an
   error from `is_on_local_disk`, and "not local" for the automatic choice.
4. **The automatic choice** (amends ADR-0017 decision 3). `Container::enable_mapped_reads_if_local` maps when the
   file is on a local disk and returns whether it did; `Container::is_on_local_disk` exposes the answer. `open` does
   not map by itself: on Windows a live mapping keeps another handle's read-write open from truncating a torn tail
   (ADR-0017), so the caller decides which handles map (read-only ones suit best). The embedded API (M10) calls it.
5. **Windows file identity** (amends ADR-0016 decision 5). `sys::file_id` returns the volume serial number and the
   128-bit file id (`GetFileInformationByHandleEx(FileIdInfo)`, unique on NTFS and ReFS), falling back to the 64-bit
   file index of `GetFileInformationByHandle` only where the 128-bit form is unsupported (FAT, exFAT; any other
   failure is an error, so the two forms are never compared). An all-zero identity, which some SMB servers report,
   is an error rather than a match. `RealFs::same_file` opens the path for attribute access only (which no sharing
   mode refuses) and compares the two identities, as Unix compares `(st_dev, st_ino)`.

## Consequences

- Mapped reads can be on by default where they are safe, without the embedder knowing the mount table.
- Compaction on Windows now refuses a path that something else renamed over, as on Unix.
- A second module with `unsafe` code in `persia-storage`, small and reviewed with this ADR. The build graph does not
  grow; two direct dependencies are added.
- Detection reads the file system once per call; it is not cached, so a remount between calls is seen.
- File systems missing from the Linux allow-list (a new local one) read with `pread` until they are added.
- Known approximations: overlayfs counts as local, true of container images (overlay2) but not of a layer on NFS;
  a fixed disk or an ext4/XFS file system may sit on a network block device (iSCSI, NBD, a VHD on a share), where a
  failed page read ends the process as on a network file system.
- Nothing maps by default yet: the embedded API wires the automatic choice up for its read handles (ROADMAP
  10.2.4).

## Alternatives considered

- **Own `extern` declarations**: no new direct dependency, but `struct statfs` has a different layout on each OS and
  architecture, and a wrong one is silent memory corruption.
- **Parsing `/proc/self/mountinfo`**: no `unsafe`, but Linux only, racy against remounts, and the device of a path
  still has to be matched to a mount.
- **`rustix`**: safe wrappers for `fstatfs`, but not in the graph, and no help on Windows.
- **Waiting for std's `windows_by_handle`**: unstable since 2019; 2.4.18 would stay open indefinitely.
- **Deferring both leaves**: closes M2 sooner, but leaves the Windows identity check reduced to a path existence test.
