# Changelog

All notable, user-visible changes to Persia DB are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). On-disk format versions are tracked separately
(SPEC §15) and are called out under **Format**, a project-specific section added to the standard ones.

## [Unreleased]

### Added
- `persia-analysis`: tokenizers (SPEC §6.3.1). `Token` carries its text, byte span and position; `StandardTokenizer`
  splits at Unicode word boundaries (UAX #29: numbers, apostrophes, emails and URLs as specified, one token per
  character for Han, Hiragana and Thai), `WhitespaceTokenizer` at Unicode white space, and `KeywordTokenizer` keeps
  the whole value. `TokenLengthGuard` truncates (default) or drops tokens longer than a limit (255 bytes by default).
- `persia-engine`: the manifest (`manifest::Manifest`, SPEC §8.7; ADR-0024): per collection its schema and live
  segments with their tombstone sidecars, plus `seq`, `fence_token` and `wal_start`. A checksummed, versioned binary
  encoding (`encode`/`decode`, corrupt input is an error naming the offset) and `ManifestHash`. Commits are planned
  as diffs (`ManifestDiff`): `apply` checks the transition (no fence, WAL or tombstone regression, schema changes
  only through evolution, fresh segment ids) and `diff` recovers the diff between two manifests.
- `persia-engine`: documents (`document::Document`, SPEC §6.4; ADR-0023). Values per field type (strings or lists of
  strings for `text`/`keyword`, one value for numerics, `bool` and `datetime`, any JSON for `stored`), validation
  against a schema with precise errors (field and list index), and limits (id, keyword length, field and value counts, nesting,
  size). JSON input rejects duplicate keys; `datetime` takes integer microseconds or RFC 3339 and is written as RFC
  3339 UTC. Non-strict collections keep unknown keys as extra stored fields. `f64` values are finite and `-0.0` is
  stored as `0.0`, so range filters never miss zero.
- `persia-engine`: collection schemas (`schema::CollectionSchema`, SPEC §6.2; ADR-0022). Field types and per-type options
  (analyzer, boost, positions, `k1`, `b`, facet, stored), ranking (`static_rank`) and relaxation defaults, and synonyms.
  The JSON definition is parsed strictly: unknown or duplicate keys and options foreign to a field's type are errors.
  Names, limits and ranges are validated. A definition may give explicit field ids; missing ones are assigned in byte
  order of the field names, and evolution keeps existing ids and gives added fields the next ones. A schema hash (XXH3-128 of a canonical
  encoding of what is indexed) ignores query-time settings. Evolution accepts added fields and rejects changes to a
  field's type, analyzer, positions, facet or stored flag.
- Website: homepage title no longer repeats the site name; share image, JSON-LD, edit links, and last-updated dates; distinct titles for the feature, roadmap, and architecture pairs; longer meta descriptions.
- Development environment: `docker-compose.dev.yml` runs MinIO (the community `pgsty/minio` build), fake-gcs-server and Azurite on localhost, every image pinned by digest; `just dev-up` / `just dev-down`. CI checks the pins resolve.
- Design for embedded Go and TypeScript SDKs (ADR-0018, `docs/sdk/`). Nothing is implemented. Go links a future C ABI with cgo; TypeScript's first package is an N-API addon, with WASM for the browser and edge deferred. The network clients in SPEC §13.2 are unchanged.
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
- `persia-storage`: `LocalStorage::sweep_temporaries` removes the `@tmp.*` files that crashed `put`s left, skipping
  those a live `put` holds (each `put` now locks its temporary file); `temporaries` lists them. Never automatic
  (ADR-0019).
- `persia-storage`: `Container::side_files` lists a container's temporary and quarantine files by path, and
  `remove_quarantine_files` removes the quarantine copies through a read-write handle.
- `persia` CLI: `persia container files|sweep [--quarantine] <path>` and `persia store files|sweep [--force] <dir>`
  list and remove those files, one tab-separated line per file. `--quarantine` removes copies that may hold acknowledged
  writes, and `--force` (where locks cannot be checked, Windows) can make a racing `put` fail: inspect the listing first.
- `persia-storage`: compaction copies each frame in bounded 64 KiB chunks, validating it on the way, so compacting a
  container with a large Segment frame no longer needs that much memory, and a small frame is copied with one write
  (`benches/compaction.rs`). `persia-format`: `StreamScanner::reframe`.
- `persia-storage`: `Container::append_stream` appends a frame whose payload is written in chunks (`FrameWriter`).
- `persia-storage`: a read-write open whose frames after the last commit end in corruption no longer always fails
  (ADR-0014): zeros and stale frames are truncated, other bytes are copied to a `<name>.quarantine.*` file first, and
  only frames that may hold acknowledged writes still fail the open, unless `Container::open_with(.., Repair::Quarantine)`.
- `persia-storage`: `vfs::Vfs`, the file-system operations of `LocalStorage` and the container's path API, with
  `RealFs` and `SimFs`, a simulated file system that loses unsynced data and directory entries in a crash;
  `LocalStorage::open_in` and `Container::create_in` / `open_in` run on any of them.
- `persia-storage`: `Container::sweep_stale_files` removes the temporary files a crashed create or compaction left next
  to the container.
- `persia-storage`: opt-in mapped reads of a container's committed frames (`Container::enable_mapped_reads`, one
  window over `12288..log_end` remapped after each commit and compaction, `pread` for everything else), and
  `Container::read_frame_source`, which returns a payload as a `ByteSource`: without copying it for an uncompressed
  frame the mapping covers; read into a buffer for other uncompressed frames, decompressed into one for compressed
  frames (SPEC §4.7; ADR-0017).

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
- Manifest encoding version 1 (SPEC §8.7; ADR-0024): magic `PMNF`, fixed-width little-endian scalar fields, and
  variable-length segment entries (id, doc count, size, object or frame location, optional tombstone sidecar),
  schemas as JSON with pinned ids and analyzer versions, CRC32C trailer. Golden files in `tests/fixtures/manifest/v1/`.
- Document encoding for `DocStore` rows (SPEC §6.4.2; ADR-0023): a deterministic CBOR subset (definite lengths,
  shortest arguments, 64-bit floats, sorted keys), `array(5)[version 1, id, blob reference, fields by id, extra
  fields]`. Decoders reject anything non-canonical. Pinned by golden vectors.
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
