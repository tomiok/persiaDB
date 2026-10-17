# ADR-0010: The memory-mapping unsafe boundary and the SIGBUS strategy

- Status: accepted
- Date: 2026-10-11
- Related: SPEC §4.4, §4.6, §4.7, §8.6, §10.1; ADR-0007, ADR-0008, ADR-0009; ROADMAP 1.8, 2.1.2, 2.3, 2.4.4, 2.4.5, 2.4.8

## Context

Reading a segment through `mmap` lets the OS page cache hold it and saves one copy per read. Persia allows
`memmap2` for this (ADR-0001), and CLAUDE.md allows `unsafe` only in `persia-format::mmap` and modules an ADR lists.
Mapping a file is `unsafe` in Rust (`memmap2::Mmap::map`) because the compiler treats `&[u8]` as immutable while
the kernel does not:

1. **Mutation.** If anyone writes mapped bytes, a `&[u8]` changes under the code that holds it. That is undefined
   behavior. In practice it also breaks every "checked once, then trusted" step: a frame CRC verified a moment ago
   no longer covers the bytes being decoded.
2. **Truncation.** Touching a mapped page that lies wholly past the end of the file faults.
3. **I/O errors.** A disk error on a page the kernel faults in also faults, where `read(2)` would return `EIO`. On
   network filesystems the result depends on the mount: a soft NFS mount faults, a hard mount blocks until the
   server returns.

A fault is `SIGBUS` on Unix and `EXCEPTION_IN_PAGE_ERROR` on Windows. Neither can be turned into a Rust error after
the fact. ROADMAP 1.8 asks where the unsafe boundary is, what contract keeps it sound, and what happens on a fault.

## Decision

1. **`MmapFile::map` is an `unsafe fn`.** `persia_format::mmap` holds the one `unsafe` block that calls memmap2.
   The mapping is read-only (`PROT_READ`, `MAP_SHARED`), so nothing in the process can write through it. Whether a
   range will stay unchanged is a property of the whole program and of other processes, not something `map` can
   check. So the caller states it: `# Safety` on `map` gives the precondition (no write, truncation or hole punch
   of the range while it is mapped), and each call site gives a `// SAFETY:` comment saying why it holds there.
   Building a `ByteSource` from an `MmapFile` is safe, because the obligation was discharged when it was mapped.
2. **Where `map` may be called.** Outside `persia-format`'s own tests, the only module allowed to call it (and so
   to contain `unsafe`) is the local backend's mapped-read module in `persia-storage` (ROADMAP 2.3.6). It maps only
   ranges that SPEC §4.7 makes immutable and cites §4.7 in each `SAFETY` comment. Every other crate keeps
   `#![forbid(unsafe_code)]`. Adding another caller means amending this ADR.
3. **The container contract (SPEC §4.7).** It makes the precondition true for container frames:
   - only frames in `12288..log_end` of a superblock the reader has read are mapped. The header and superblock
     pages are rewritten in place and are always read with `pread`;
   - frames are append-only (SPEC §4.4), and segments are immutable;
   - a writer never truncates, hole-punches or rewrites the file below the `log_end` it opened with or has since
     committed. Recovery truncates only at the end of the scan from `log_end`; an invalid frame in
     `wal_start..log_end` is corruption and is never truncated (SPEC §8.6);
   - compaction (SPEC §4.6) writes a new file and renames it over the old one, so old mappings keep the old inode,
     whose contents never change;
   - creating a container never truncates an existing file.
   The exclusive writer lock (ROADMAP 2.4.1.1) keeps two Persia writers apart. These cases fall outside the
   contract, as they do for LMDB or for SQLite in mmap mode:
   - another program modifying the file;
   - a superblock slot damaged after a reader in another process read it. Recovery may then fall back to the older
     slot and truncate below what that reader mapped.
4. **The range is checked at map time.** `map` rejects a range that is inverted, longer than `isize::MAX`, or ends
   past the file's current length. So no byte of the returned slice lies past EOF, and only a later truncation
   could fault on it. For an empty range memmap2 still maps one byte, possibly in a page past EOF. The slice is
   empty, so that page is never touched.
5. **A fault is fatal, not handled.** Persia installs no `SIGBUS` handler or vectored exception handler. A handler
   is process-global and conflicts with the host application's handlers. Recovering from a fault needs
   `siglongjmp`-style unwinding, which Rust cannot do soundly. A fault ends the process. Durability is unaffected:
   nothing is written through a mapping, and the next open recovers from the superblock (SPEC §8.6).
6. **`pread` is always available.** Mapping is an optimization, never required. The local backend reads with
   `pread` by default (ROADMAP 2.3.3). Mapped reads are opt-in, or chosen automatically on local disk filesystems
   only, so network and FUSE mounts keep seeing `EIO` as an error (ROADMAP 2.3.6). How much to map at once (one
   mapping per segment frame, or one window per `12288..log_end`, remapped as it grows) is decided in 2.3.6.
7. **One buffer type.** `persia_format::source::ByteSource` wraps `bytes::Bytes` plus the absolute offset of its
   first byte. A mapping (via `Bytes::from_owner`), a heap buffer and a cached remote range look the same to the
   engine. Slicing is cheap and keeps the mapping alive, and error offsets stay absolute (SPEC §4.1).

## Consequences

- Each mapping is justified where the knowledge lives: the storage layer that read `log_end`. The precondition is
  visible in the type system. A safe caller cannot map a file and then write it without writing `unsafe` itself.
- Writers carry an obligation the compiler cannot check. The recovery, truncation and creation code (ROADMAP 2.4.1,
  2.4.4, 2.4.5) must cite SPEC §4.7, and its reviews must check that no write lands below `log_end`.
- Tests that create a file mapping do not run under Miri, which has no file-backed `mmap`. They are skipped with
  `#[cfg_attr(miri, ignore = ...)]`, like the zstd tests. Heap-backed `ByteSource` tests still run under Miri.
- A `ByteSource` slice keeps its whole mapping alive. A block cache that holds small slices of a large mapping
  pins the mapping too. This matters on Windows, where an open mapping prevents truncating the file, and may
  prevent replacing it by `rename`. 2.4.5.3 verifies the second point by test and makes compaction work around it.
- A mapping is a snapshot of `start..end`. To see newer frames, a reader maps the new range again; mappings never
  grow in place.

## Alternatives considered

- **A safe `map` with the contract only in its docs.** One `unsafe` block in the workspace, but unsound in Rust's
  sense: any safe code could map a file and then write it. The soundness argument would also live far from the
  code that upholds it.
- **A `SIGBUS` handler with `sigsetjmp`/`siglongjmp`** (what some C databases do): unsound in Rust (it skips
  destructors), process-global, and it fights the host application's own handlers.
- **Copy-on-open (`MAP_PRIVATE`) or `mlock`**: neither prevents a fault on truncation or an I/O error, and `mlock`
  pins memory a search database cannot afford.
- **No mmap, `pread` only**: always safe, but every read pays for a copy and a syscall. Kept as the default and as
  the fallback (decision 6), not as the only path.
