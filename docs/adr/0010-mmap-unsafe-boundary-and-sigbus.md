# ADR-0010: The memory-mapping unsafe boundary and the SIGBUS strategy

- Status: accepted
- Date: 2026-10-11
- Related: SPEC §4.4, §4.6, §4.7, §10.1; ADR-0007, ADR-0008, ADR-0009; ROADMAP 1.8, 2.1.2, 2.3, 2.4.4, 2.4.5, 2.4.8

## Context

Reading a segment through `mmap` lets the OS page cache hold it and saves one copy per read. Persia allows
`memmap2` for this (ADR-0001), and CLAUDE.md allows `unsafe` only in `persia-format::mmap`. Mapping a file is
`unsafe` in Rust (`memmap2::Mmap::map`) because the compiler treats `&[u8]` as immutable while the kernel does not:

1. **Mutation.** If another writer changes mapped bytes, a `&[u8]` changes under the code that holds it. That is
   undefined behavior. In practice it also breaks every "checked once, then trusted" step: a frame CRC verified a
   moment ago no longer covers the bytes being decoded.
2. **Truncation.** Touching a mapped page that lies wholly past the end of the file raises `SIGBUS`.
3. **I/O errors.** A disk error on a page the kernel faults in also raises `SIGBUS`, and on network or FUSE
   filesystems so does a server that goes away. `read(2)` would return `EIO` instead.

None of these can be turned into a Rust error after the fact. ROADMAP 1.8 asks where the unsafe boundary is, what
contract keeps it sound, and what happens on `SIGBUS`.

## Decision

1. **One unsafe call.** `persia_format::mmap::MmapFile::map(&File, Range<u64>)` is the only `unsafe` block in the
   workspace. The module carries `#![allow(unsafe_code)]`, and every other crate keeps `#![forbid(unsafe_code)]`
   (or inherits the workspace `deny`). The mapping is read-only (`PROT_READ`, `MAP_SHARED`), so nothing in the
   process can write through it.
2. **The safety contract, enforced by the format rather than the type system.** `MmapFile::map` is a safe fn. It is
   sound because Persia never writes a range that may be mapped (SPEC §4.7):
   - only frames in `12288..log_end` of a superblock the reader has read are mapped. The header and superblock
     pages are rewritten in place and are always read with `pread`;
   - frames are append-only (SPEC §4.4), and segments are immutable;
   - a writer never truncates, hole-punches or rewrites the file below the highest `log_end` it has committed or
     recovered. Torn-tail truncation starts at or after `log_end` because the recovery scan starts there;
   - compaction (SPEC §4.6) writes a new file and renames it over the old one. Old mappings keep the old inode,
     whose contents never change.
   The exclusive writer lock (ROADMAP 2.4.1.1) keeps two Persia writers apart. Another program that modifies the
   file is outside the contract, as it is for LMDB, or for SQLite in mmap mode.
3. **The range is checked at map time.** `map` rejects a range that is inverted, does not fit in `usize`, or ends
   past the file's current length. So no mapped page starts past the end of the file, and only a later truncation
   could raise `SIGBUS`.
4. **SIGBUS is fatal, not handled.** Persia installs no `SIGBUS` handler. A handler is process-global, conflicts with
   the host application's handlers, and recovering from a fault needs `siglongjmp`-style unwinding that Rust cannot
   do soundly. A fault ends the process. Durability is unaffected: nothing is written through a mapping, and the
   next open recovers from the superblock (SPEC §8.6).
5. **pread is always available.** Mapping is an optimization, never required. The local backend reads with `pread`
   by default (ROADMAP 2.3.3). Mapped reads are an opt-in, or an automatic choice on local disk filesystems only,
   so network and FUSE mounts, where a fault is a lost connection rather than a dead disk, keep getting `EIO` as an
   error (ROADMAP 2.3.6).
6. **One buffer type.** `persia_format::source::ByteSource` wraps `bytes::Bytes` plus the absolute offset of its
   first byte. A mapping (via `Bytes::from_owner`), a heap buffer and a cached remote range look the same to the
   engine. Cheap slicing keeps the mapping alive, and error offsets stay absolute (SPEC §4.1).

## Consequences

- The workspace has exactly one audited `unsafe` block, in `persia-format`. `persia-storage` and the engine stay
  `forbid(unsafe_code)`.
- Writers carry an obligation the compiler cannot check. The crash and truncation code (ROADMAP 2.4.4, 2.4.5) must
  cite SPEC §4.7, and its reviews must check that no write lands below `log_end`.
- Tests that create a file mapping do not run under Miri, which has no file-backed `mmap`. They are skipped
  with `#[cfg_attr(miri, ignore = ...)]`, like the zstd tests. Heap-backed `ByteSource` tests still run under Miri.
- **Windows**: an open mapping makes the file impossible to truncate, and `rename` over it fails. Compaction must
  close every mapping first, or keep the old path until readers drain (ROADMAP 2.4.5.3).
- A mapping is a snapshot of `start..end`. To see newer frames, a reader maps the new range again; mappings never
  grow in place.

## Alternatives considered

- **`unsafe fn map`.** More honest in the type system, but every caller (`persia-storage`) would then need
  `unsafe` too, which spreads the boundary that CLAUDE.md confines to one module. The contract is a property of
  the file format and the write path, not of the call site, so it is documented there (SPEC §4.7).
- **A `SIGBUS` handler with `sigsetjmp`/`siglongjmp`** (what some C databases do): unsound in Rust (it skips
  destructors), process-global, and it fights the host application's own handlers.
- **Copy-on-open (`MAP_PRIVATE`) or `mlock`**: neither prevents `SIGBUS` on truncation or I/O errors, and `mlock`
  pins memory a search database cannot afford.
- **No mmap, `pread` only**: always safe, but every read pays for a copy and a syscall. Kept as the default and as
  the fallback (decision 5), not as the only path.
