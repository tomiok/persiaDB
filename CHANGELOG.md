# Changelog

All notable, user-visible changes to Persia DB are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). On-disk format versions are tracked separately
(SPEC §15) and are called out under **Format**, a project-specific section added to the standard ones.

## [Unreleased]

### Added
- Workspace skeleton (stub crates).
- Apache-2.0 license.
- Project website under `website/` (Astro Starlight), for GitHub Pages at `https://tomiok.github.io/persiaDB/`.
- Website: a Getting started page (what works today versus the illustrative `Db` API) and a For AI agents page. `llms.txt` and `llms-full.txt` follow the llms.txt convention and are linked from the README and the site footer.
- `persia-format`: read-only memory mapping of immutable file ranges (`MmapFile`) and `ByteSource`, one buffer type for
  mapped, heap and cached remote bytes that keeps error offsets absolute (SPEC §4.7; ADR-0010).
- `persia-format`: `FrameEncoder` streams a frame whose payload is too large to buffer (declared length, incremental
  CRC, length verified on finish), and `FrameHeader` parses a frame's 24-byte header so range readers can size reads
  (SPEC §4.4).
- `persia-format`: `BlobId` (SHA-256, lowercase hex text form) and `BlobRef { blob_id, size, mime, chunk_size, user_meta }`
  with validated limits and a canonical binary encoding (SPEC §9.1; ADR-0011).
- `persia-storage`: the `Storage` trait with validated `Key`/`KeyPrefix`, `ObjectMeta`, `PutOutcome`, `Capabilities`,
  an error taxonomy (`ErrorKind`: not found, conflict, transient, corrupt, I/O, invalid input, unsupported), length-checked
  ranged reads returning `ByteSource` (`ByteSource::for_range`), and a retry policy for transient errors (exponential
  backoff with full jitter, injectable clock) (SPEC §10.1).
- `persia-storage`: `MemoryStorage`, the in-memory backend with atomic `put_if_absent`, and `FaultyStorage`, a wrapper
  that injects errors, latency, torn and dropped writes, lost responses and crashes (optionally losing unsynced writes,
  in order or per object) on a scripted or seeded schedule; a shared conformance
  suite checks every backend against SPEC §10.1.
- `persia-storage`: `LocalStorage`, the local filesystem backend (one file per object): atomic, durable `put` (temp file,
  fsync, rename, directory fsync) and `put_if_absent` (hard link), positioned reads, and file names encoded so every key
  round-trips on case-insensitive and normalizing filesystems and `a` and `a/b` can coexist (SPEC §10.1.1; ADR-0012).
- `persia-storage`: `Container`, the single-file container (SPEC §4): creation that never replaces a file, an
  exclusive writer lock, read-only opens, frame appends with group `fdatasync`, commits that write the inactive
  superblock slot, and recovery on open (superblock checks, validation of the committed WAL frames, torn-tail
  truncation; a corrupt tail is truncated, quarantined first, or refused, per ADR-0014 below). A failed write or sync poisons the handle.
  `SimFile` simulates crashes (lost, kept or sector-torn unsynced writes) for crash tests.
- `persia-storage`: container compaction (SPEC §4.6): `space_usage` reports live and dead bytes against the default
  40% dead-space ratio, and `compact` rewrites the live frames, the WAL range and the uncommitted tail into a new
  file (renumbered, payloads copied as stored, a new Manifest built from where the committed frames moved; the returned
  `Relocation` also covers the uncommitted tail) that is synced
  and renamed over the container; the old file is never written. `persia-format`: `Frame::reframe`.
- `persia-storage`: recovery reads the container in bounded 64 KiB chunks (`persia-format`: `StreamScanner`), so
  opening a container with a large uncommitted Segment frame or a long torn tail no longer needs that much memory.
- `persia-storage`: `Container::append_stream` appends a frame whose payload is written in chunks (`FrameWriter`).
- `persia-storage`: a read-write open whose frames after the last commit end in corruption no longer always fails
  (ADR-0014): zeros and stale frames are truncated, other bytes are copied to a `<name>.quarantine.*` file first, and
  only frames that may hold acknowledged writes still fail the open, unless `Container::open_with(.., Repair::Quarantine)`.
- `persia-storage`: `vfs::Vfs`, the file-system operations of `LocalStorage` and the container's path API, with
  `RealFs` and `SimFs`, a simulated file system that loses unsynced data and directory entries in a crash;
  `LocalStorage::open_in` and `Container::create_in` / `open_in` run on any of them.
- `persia-storage`: `Container::sweep_stale_files` removes the temporary files a crashed create or compaction left next
  to the container.

### Fixed
- `persia-storage`: directories are synced on Windows too (`FlushFileBuffers` on a directory handle), so an acknowledged
  local-backend write or container create survives a machine crash there (SPEC §4.5.2).
- `persia-storage`: on Windows the container writer lock is on a `<name>.lock` file beside the container, so readers
  are no longer locked out while a writer is open (SPEC §4.5.1).
- `persia-storage`: a local-backend object whose path is longer than `PATH_MAX` allows (1023 bytes on macOS and the BSDs, 4095 on Linux) is `InvalidInput`
  (`KeyDefect::PathTooLong`) instead of an I/O error.
- Website: the article column is centered between the left nav and the narrow "On this page" rail, at Starlight's 45rem reading width.
- Website: the "On this page" rail is a fixed width at the viewport edge, and long headings wrap inside it.

### Format
- Integer codecs for segment sections (SPEC §4.1, §5.2.1; ADR-0004): zigzag signed varints, delta coding of
  sorted docids, 128-value bit-packed blocks (SIMD-BP128 layout), postings full and tail blocks, and
  frame-of-reference blocks with order-preserving keys for numeric columns.
- Checksum and hash primitives (SPEC §4.1): CRC32C (one-shot and streaming), xxh3-64/128 (seed 0), and
  `Checksummed<T>` for fixed-size structures whose CRC32C field covers the rest of the region (SPEC §4.2, §4.3).
- Block codecs (SPEC §5.2; ADR-0005): codec byte `0 = None`, `1 = Zstd` (one zstd frame with declared content
  size), with a decompression-bomb guard on every decode.
- Block framing of segment sections (SPEC §5.2; ADR-0006): 16-byte block header (CRC32C, codec, lengths), fixed-size
  blocks over a byte stream, and a CRC-protected block table that records the block size (the bomb limit) and
  each block's end; corrupt blocks are reported with their index. The section table has no `codec` field.
  A block table can be opened from the section's tail alone and blocks decoded from separately fetched bytes
  (object storage). Decoding errors now separate corruption from caller errors and from running out of memory
  (an unallocatable decompression buffer is no longer reported as corrupt data).
- Container header and superblock (SPEC §4.2, §4.3; ADR-0007): `Header` (magic, format version, feature flags,
  database id, page size) and double-buffered `Superblock` slots with CRC32C, and selection of the active slot. An
  unknown `format_major` or `incompat_flags` bit is a new `Unsupported` error class rather than corruption; unknown
  `rocompat_flags` bits open the container read-only; reserved bytes must be zero; two valid slots with the same
  `seq` but different contents are corruption. Each slot is a full 4096-byte page (A at 4096, B at 8192, first frame
  at 12288) so one bad sector cannot lose both (ADR-0008); `page_size` is an advisory alignment hint; a writer never
  lowers `format_minor`.
- Frames (SPEC §4.4; ADR-0009): encode and validate the checksummed records after the superblock, with 8-byte
  padding, a kind registry whose unknown kinds are skipped, and optional zstd payloads that carry their uncompressed
  length and decode under a caller-supplied bomb limit. A frame scanner stops at the first invalid frame and reports a
  clean end, a torn tail (only zeros or nothing follow), corruption (non-zero bytes follow) or a frame with unknown
  flags, which is `Unsupported` and never truncated.
- `frame_seq` (SPEC §4.4; ADR-0013): numbers a container's frames contiguously from 1, continuing across rewrites;
  the superblock gains `next_frame_seq` at offset 48, and recovery treats a frame that breaks the run as corruption.
  The container reader checks the superblock's offsets against the file (SPEC §4.3).

<!--
Sections per release, in this order, only when non-empty:
### Added / ### Changed / ### Deprecated / ### Removed / ### Fixed / ### Security / ### Format
-->

[Unreleased]: https://github.com/tomiok/persiaDB/commits/main
