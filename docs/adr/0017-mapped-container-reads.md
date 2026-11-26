# ADR-0017: Mapped container reads: an opt-in window over the committed frames

- Status: proposed
- Date: 2026-10-17
- Related: SPEC §4.4, §4.6, §4.7; amends ADR-0010 (decisions 2 and 6); ADR-0016; ROADMAP 2.3.6, 2.3.10, 0.6.7, 0.6.8; amended 2026-11-23 for ROADMAP 10.2.4 (ADR-0021, ADR-0060)

## Context

ADR-0010 allowed one module outside `persia-format` to call `MmapFile::map`: "the local backend's mapped-read
module", for ranges SPEC §4.7 makes immutable. It left three questions to ROADMAP 2.3.6: where exactly the call
site lives, how much to map at once (one mapping per frame, or one window over `12288..log_end`), and when to map
(opt-in, or automatically on local disk file systems).

## Decision

1. **Call site.** `persia_storage::container::mapped` is the only library module outside `persia-format` that maps
   (`persia_storage::sys` holds the OS calls of ADR-0021); the tests and the bench that enable mapped reads hold
   `unsafe` blocks for the `unsafe fn` (decision 3: `container_mapped.rs`, `container_process.rs`,
   `benches/container_read.rs`). The crate root is `#![deny(unsafe_code)]` instead of `forbid` so that module can
   `allow` it; a repository check
   (`xtask/src/repo_checks.rs`, `UNSAFE_ALLOWED`) fails if any other file relaxes the lint. The mapping function is
   private to the container and only ever gets the container's own file and a superblock the container read or
   committed on it, so the API offers no way to map an arbitrary range. That does not make the file exclusively the
   container's: as for any mmap-based database, the embedder must not write or truncate it through another handle
   (including a `try_clone` of a `File` passed to `open_on`) while a mapping lives. That is the "other programs"
   exclusion of ADR-0010 decision 3, extended to other handles in the same process.
2. **One window over `12288..log_end`.** A container with mapped reads holds a single read-only mapping of its
   committed frames. A frame read takes the frame's bytes from the window if the window covers them and reads them
   with `pread` otherwise (frames after `log_end`, which recovery may still truncate). After a commit the window is
   mapped again up to the new `log_end` (mappings never grow in place, ADR-0010); after a compaction
   (`Container::compact`, which writes a new file and renames it over the container, SPEC §4.6) it is mapped on the
   new file. A failed remap after a commit keeps the old window, which stays valid for the frames it
   covers; after a compaction the old window is dropped, since its offsets belong to the old file.
3. **Opt-in only, for now.** `Container::<File>::enable_mapped_reads` turns it on; the default stays `pread`.
   It is an `unsafe fn` (ROADMAP 0.6.8): soundness rests on a precondition the container cannot check (nothing but
   Persia containers writes, truncates or hole-punches the file below the mapped `log_end` while a mapping lives,
   ADR-0010 decision 3), so the caller states it with an `unsafe` block, as for `memmap2::Mmap::map`.
   `enable_mapped_reads_if_local` is `unsafe` for the same reason. The `# Safety` section also names the two cases
   where Persia itself would break the precondition: a second read-write container in the same process where the
   writer lock is per process (NFS, SPEC §4.5.1), and recovery truncating below a mapped `log_end` after a slot was
   damaged.
   Choosing automatically needs the file system type (`statfs`/`f_type` on Linux, `MNT_LOCAL` on the BSDs and macOS,
   `GetDriveTypeW` on Windows), which the standard library does not expose and which would need FFI (`unsafe`) or a
   new dependency. ROADMAP 2.3.10 tracks it; until then the embedder decides.
4. **Zero-copy reads.** `Container::read_frame_source` returns the payload as a `ByteSource`: for an uncompressed
   frame, a slice of the window (or of the `pread` buffer) at its absolute file offset; for a compressed one, the
   decompressed bytes at offset 0, since they lie nowhere in the file. Every read still validates the frame (header,
   CRC, bomb limit): mapped bytes are trusted no more than read ones.

## Consequences

- One `mmap` per commit, not per frame: few mappings (one VMA), and the cost of mapping does not grow with the
  number of frames. A reader that holds any slice keeps its whole window alive (ADR-0010); remapping after a commit
  briefly keeps two windows over the same pages. A writer with mapped reads pays one `mmap`, and one `munmap` of the
  previous window once nothing holds it, per commit: microseconds next to the commit's two syncs (typically
  milliseconds), so the commit path is not benchmarked separately.
- The window needs address space for all committed frames. A 32-bit process may fail to map a large container;
  `enable_mapped_reads` then returns the error. A failed remap after a commit or compaction is not an error: the
  commit or compaction has already succeeded, and returning an error from it would read as if it had not. The
  remap is an optimization with a complete fallback (`pread`, which reports its own I/O errors on every read), so
  no I/O error that affects data is dropped (CLAUDE.md forbids swallowing those). The fallback is observable:
  `mapped_range()` then still ends at the old `log_end`, or is `None` after a compaction (`is_mapped()` is then
  `false`). Each failed remap also logs a `tracing` warning and counts in `mapping_failures()` (ROADMAP 0.6.7), so a
  container that silently fell back to `pread` shows up in logs and metrics.
- On Windows the file mapping object behind a window covers the whole file as it was when mapped (memmap2 creates
  it with maximum size 0; the view itself is only `12288..log_end`, rounded to the 64 KiB allocation granularity),
  and Windows refuses to shrink a file below a live mapping object. While a reader holds a window, a read-write
  open elsewhere that needs to truncate a torn or corrupt tail fails with an I/O error, the container unchanged; it
  succeeds once the mapping is gone. A corrupt tail is copied to a quarantine file before the truncation (ADR-0014),
  so each such failed open may leave one quarantine copy behind. The copy is not removed on failure: a truncation
  that reports an error may still have happened, and the copy would then be the only one of the tail. This costs
  availability and disk space, not safety. Unix is unaffected: the tail lies past the window.
- Measured with `cargo bench -p persia-storage --bench container_read` (warm page cache, Linux), each read including
  the frame's full validation: a 4 KiB frame 2.2 µs with `pread`, 0.78 µs mapped; a 1 MiB frame 252 µs and 164 µs
  (the CRC dominates large frames, so the gain there is mostly the copy saved).

## Alternatives considered

- **One mapping per frame**: maps only what is read, but costs an `mmap`/`munmap` and a VMA per frame, and pays the
  syscall on every uncached read, which is what mapping avoids.
- **Remapping lazily on read**: needs interior mutability (a lock or `ArcSwap`) on the read path; remapping in
  `commit`, which already takes `&mut self`, needs neither.
- **A mapping method on `ContainerFile`**: the trait is public, so it would be a safe way to map any file range,
  the unsoundness ADR-0010 rejected.
- **Automatic choice now, by parsing `/proc/self/mountinfo`**: Linux only, and racy against remounts; not worth it
  before 2.3.10 picks a portable approach.

## Amendment (2026-11-23): read-only handles map by default on local disks

Decision 3 kept mapping opt-in and `unsafe` "for now", with the automatic choice left to ROADMAP 2.3.10. ADR-0021
since gave the file system type (`enable_mapped_reads_if_local`), and ROADMAP 10.2.4 asks that read handles map where
the container is on a local disk, on by default. The owner decided (wave 7, decision n) how:

1. **An entry for read-only containers.** `Container::<File>::map_read_only_if_local` maps `12288..log_end` when
   the container was opened read-only, Persia opened its file itself by path on the operating system's file system
   (`Container::open`, `open_with`, and the `reopen_read_only` of one), and the file is on an allow-listed local disk
   file system (ADR-0021), and returns `false` otherwise; a read-write container is refused. A container on a file
   the caller handed in (`Container::open_on`, `open_in` with the caller's own `Vfs`, and their reopens) is never
   mapped by it: the caller may hold a clone of that `File`, and a `set_len` through the clone would fault the mapping
   (or change bytes under a live `&[u8]`) from safe code. Such a caller maps with the `unsafe` `enable_mapped_reads`,
   stating the contract itself (found in the wave 7 review). It lives in `persia_storage::container::mapped`,
   the module decision 1 already allows to call `MmapFile::map`. **It is an `unsafe fn`** (PR #127 review): no safe
   public function of `persia-storage` maps a file, since a safe one would let any safe code of the process cause
   the residual of item 2. Its one caller is the engine's read-only open, from one module,
   `crates/persia-engine/src/db/read_only/mapped.rs`, which `allow`s `unsafe_code` and states the contract in its
   `SAFETY` comment; it is listed in `UNSAFE_ALLOWED` (`xtask/src/repo_checks.rs`), and the engine's crate root moves
   from `#![forbid(unsafe_code)]` to `#![deny(unsafe_code)]` for it, as `persia-storage`'s did for decision 1. Every
   other engine module still denies `unsafe`, and the repository check fails if any other file allows it.
2. **What Persia vouches for, and what it does not.** The `unsafe` contract of decision 3 has parts that are about
   Persia's own code, and the entry's checks take exactly those on: no Persia writer truncates, hole-punches or rewrites
   below a committed `log_end` (SPEC §4.7); the mapping handle is read-only, so it never changes the file and never
   remaps over one it changed; its `File` is one Persia opened and never hands out, so no caller holds a clone of it;
   and on the allow-listed file systems the writer lock belongs to the open file, so the
   per-process-lock exception of decision 3 (a second read-write container of the same process on NFS) cannot arise.
   The rest of the contract is about everything that is not Persia, and no code can take it on: another program, or
   a handle of the file the same process opens on its own (a `File::open` of the path), that writes or truncates the container while it is mapped, and a
   slot damaged after the reader read it. For those the outcome is what ADR-0010 states: undefined behaviour for a
   write under a live slice, `SIGBUS` / `EXCEPTION_IN_PAGE_ERROR` for a truncation. That residual stays in the
   entry's `# Safety` contract. The engine's `ReadOnlyDatabase::open_with_options` takes it on for the containers it
   opens itself (the owner's accepted residual, decision n): it becomes a documented condition of deploying a
   read-only handle with the default (`MappedReads::IfLocal`), as databases that map their file by default leave it.
   A deployment that cannot exclude other writers of the file opens with ordinary reads (`MappedReads::Never`).
3. **On by default for read-only databases, never for the writer.** "Read handles" means read-only opens. The
   engine's `ReadOnlyDatabase::open_with_options` maps by default (`MappedReads::IfLocal`), except on Windows, where
   the default is `MappedReads::Never` (see the Windows consequence below; PR #127 review); either can be asked for
   explicitly, and the embedded API's `OpenOptions` is to take that default (ROADMAP 10.2.4). The engine's older
   read-only opens, which take a registry alone, keep ordinary reads, so nothing that calls them changes; so do the
   opens of a given file or container (`open_on_with_options`, `open_container_with_options`): they are generic over
   the file, and their caller holds the file or container, possibly with other handles of it. A container the caller
   mapped itself (with the `unsafe` opt-in) and passed to `open_container_with_options` is read through that mapping
   until the first refresh that finds something new: the refresh reopens it, and the reopened handle is not mapped.
   A read-write
   `Database` never maps and has no option for it: on Windows a live mapping would keep a read-write open from
   truncating a torn tail (Consequences, above), and a writer would map again at every commit. A caller that wants a
   mapped writer still has decision 3's `unsafe` opt-in on a container it opens itself.
4. **A refresh maps again.** A read-only handle never commits, so `Mapped::remap` never runs for it. Following a
   writer, `ReadOnlyDatabase::refresh` reopens the container (ADR-0060) and maps the new handle before it opens any
   segment on it: after a commit, up to the new `log_end`; after a compaction, the file renamed over the path. A
   refresh that finds nothing new maps nothing. Segments are slices of the mapping they were opened on, so a snapshot
   taken before keeps that mapping, and after a compaction the old file's blocks, until it is dropped. An incremental
   refresh reuses the segments whose manifest entry is unchanged, and they keep the mapping they were opened on: a
   long-lived reader can hold several mappings of the file at once, one per container its live segments came from.
5. **A mapping that fails is not an error of the open or the refresh.** As decision 2 and the Consequences say for a
   failed remap: reads fall back to `pread`, which reports its own errors, and a `tracing` warning says so. The handle
   shows it (`Container::is_mapped`).

Consequences of the amendment:

- An uncompressed segment frame read by a mapped read-only database is a slice of the mapping (decision 4), not a
  heap copy of the frame; every read still validates the frame.
- **Windows: the default is `MappedReads::Never`** (owner, PR #127 review). The Consequences above say that a live
  mapping keeps a read-write open elsewhere from truncating a torn or corrupt tail. A mapped reader holds at least one
  mapping for as long as it is open (a refresh adds a newer mapping of the same file, and the older ones stay alive
  while a reused segment or an old snapshot holds them: decision 4). So after a writer stops uncleanly and leaves a
  torn or corrupt tail, its next read-write open fails with an I/O error, the container unchanged, until every mapped
  reader of that file has closed; each attempt on a corrupt tail may leave a quarantine copy behind (ADR-0014). A
  clean tail needs no truncation and opens. Compaction's rename is not blocked on NTFS (ADR-0016), but a fixed drive
  formatted FAT32 or exFAT also counts as a local disk (ADR-0021 decides by drive type), and there `MoveFileExW` may
  refuse to rename over a mapped file (SPEC §4.7): while a reader maps the container, the writer's compactions fail,
  without harm but without reclaiming space. Neither is a choice a default reader should make for the writer, so on
  Windows (`cfg(windows)`) `MappedReads::default()` is `Never`, and a reader that wants mapped reads asks for
  `IfLocal`. Other platforms keep `IfLocal`. ROADMAP 10.1.22 (v1.1) revisits the Windows default. The CI Windows job
  runs `crates/persia-engine/tests/read_only_mapped.rs`, which checks it.
- The blob pack (`persia-blob`) still maps only through `MapPermit::new`, an `unsafe fn`, which `persia` cannot call:
  its read handle keeps `pread` until it has a permit of its own (ROADMAP 10.1.23).
- The residual of item 2 is reachable from the engine's safe `ReadOnlyDatabase::open_with_options` (decision n), not
  from any safe function of `persia-storage`. It is stated in SPEC §4.7, in `MappedReads::IfLocal`'s documentation
  and in the `SAFETY` comment of `db/read_only/mapped.rs`, and is the reason the option to turn mapping off exists.

## Amendment (2026-11-26): the read-only blob pack maps like its container (ROADMAP 10.1.23)

The amendment above left the blob pack on `pread` for read-only databases: `PackStore::enable_mapped_reads` takes a
`MapPermit`, which only the `unsafe` `MapPermit::new` makes, and `persia` forbids `unsafe`. The pack now follows its
container, the same way:

1. **An `unsafe` storage entry.** `MapPermit::read_only_if_local(file)` (in `persia_storage::container::mapped`,
   the module decision 1 allows to map) returns a permit when `file` is on an allow-listed local disk file system
   (ADR-0021; unknown counts as not local), and `None` otherwise. Its `# Safety` contract is `MapPermit::new`'s plus
   `Container::map_read_only_if_local`'s for a pack: the permit maps only through a read-only handle whose `File`
   Persia opened itself by path on the operating system's file system and never hands out. The pack's writer lock
   (`<name>.persia.blobs.lock`, ROADMAP 9.2.15) keeps a second Persia writer out, and Persia's writers never change a
   pack's bytes below a committed `log_end` (SPEC §9.3). No safe public function of `persia-storage` maps.
2. **One audited caller.** `crates/persia-blob/src/pack/store/read_only_mapped.rs` is `persia-blob`'s one module that
   allows `unsafe_code` (listed in `UNSAFE_ALLOWED`); the crate root moves from `forbid` to `deny`, as the engine's did
   for item 1 of the amendment above. It makes the map function of the handles that `PackStore::open_mapped_if_local`
   opens: read-only, by path, on `RealFs`, with no way for a caller to hand in a file or a `Vfs`, which is what the
   `SAFETY` comment rests on. The map function asks for the permit at each mapping (so a pack created after the open,
   and the file a rewrite renamed over the path, are judged on their own), and a file off a local disk is read with
   `pread`, not counted as a failed mapping.
3. **A rewrite is followed.** A read-only pack stays on the file it opened when a refresh finds it replaced (ROADMAP
   9.2.13); the reader's `PackStore::reopen` then opens the replacement, and a handle `open_mapped_if_local` opened
   reopens mapped the same way (the handle keeps the function that makes its map function). Every other open, and
   every other reopen, starts with mapped reads off, as before.
4. **One setting for both files.** The embedded API's read-only open (`ReadOnlyBlobDatabase::open_with_options`)
   reads the container's `MappedReads` from its `ReadOnlyOptions` (`mapped_reads_choice`) and opens the pack with
   `open_mapped_if_local` for `IfLocal`, with `PackStore::open` otherwise. So `OpenOptions::mapped_reads(false)`
   forces `pread` for both files, `true` maps both on a local disk, and the default is the container's: `IfLocal`,
   except on Windows, where it is `Never` for both (the Windows consequence above holds for the pack: a reader's
   mapping keeps the writer's next open from truncating a torn tail of the pack, ADR-0049 Consequences). A read-write
   `Db` never maps its pack.

The residual is the container's, for the pack file: another program, or a handle of the file this process opens on
its own, writing or truncating the pack while it is mapped, and a slot damaged later. It is stated in SPEC §4.7, in
`PackStore::open_mapped_if_local`'s documentation and in the `SAFETY` comment of `read_only_mapped.rs`, and
`mapped_reads(false)` is the way out.
