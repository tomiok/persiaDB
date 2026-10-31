# Persia DB — Roadmap

**Versions.** M0–M15 are the MVP and ship as v1.0. Milestones M16 and later are post-MVP; each heading names its target version. The percentage and burn-up in [`PROGRESS.md`](./PROGRESS.md) count M0–M15 only.

How to read this file:
- Numbering is hierarchical: `M1` → `1.1` → `1.1.1` → `1.1.1.1`. **Every leaf is one GitHub issue** (title: `[1.1.1] short title`).
- A leaf is small: ≤ 1 day of focused work, one PR, independently testable.
- Non-leaf items group leaves (use GitHub milestones/labels or a tracking issue with task list).
- `(SPEC §x)` points to the section that defines the behavior. `[T]` marks the test expectation for that leaf.
- Dependencies are listed per milestone. Do not start a milestone before its dependencies are done unless noted.
- **Status** is tracked on leaves only: `[ ]` todo · `[~]` in progress · `[x]` done (DoD met). Parent status is derived.
  After changing a status run `cargo xtask progress --record`; it regenerates [`PROGRESS.md`](./PROGRESS.md).
  New work discovered along the way is added here as new leaves (next free id), never tracked only in TODOs.
- Labels to create: `milestone:M0`…`milestone:M15`, `area:format|storage|engine|analysis|blob|server|sdk|ci|docs`, `type:feat|test|bench|fuzz|docs|chore`.

Issue template (copy for each leaf):
```
## Context     (SPEC § + why)
## Task        (what exactly)
## Acceptance  (observable criteria)
## Tests       (unit / property / golden / fuzz / integration)
## Notes       (edge cases, perf constraints)
Definition of Done: see CLAUDE.md
```

---

# M0 — Project bootstrap
Deps: none.

## 0.1 Repository & workspace
- 0.1.1 Create cargo workspace with empty crates: `persia-format`, `persia-storage`, `persia-analysis`, `persia-engine`, `persia-blob`, `persia`, `persia-proto`, `persia-server`, `persia-cli`, `persia-testutil` (dev-only)
  - [x] 0.1.1.1 Workspace `Cargo.toml` with shared `[workspace.package]`, `[workspace.dependencies]`, lints table
  - [x] 0.1.1.2 Per-crate `Cargo.toml` + `lib.rs`/`main.rs` stubs with `#![forbid(unsafe_code)]` (except format)
  - [x] 0.1.1.3 Enforce dependency direction with a `cargo-deny`/script check (no upward deps)
- 0.1.2 Toolchain and formatting
  - [x] 0.1.2.1 `rust-toolchain.toml` (pinned stable) + MSRV policy
  - [x] 0.1.2.2 `rustfmt.toml`, `clippy.toml`, workspace lint levels
  - [x] 0.1.2.3 `.editorconfig`, `.gitignore`, `.gitattributes` (binary fixtures)
- 0.1.3 Repo hygiene
  - [x] 0.1.3.1 `LICENSE` (Apache-2.0) + `NOTICE`
  - [x] 0.1.3.2 `README.md` stub (what it is, status, quickstart placeholder)
  - [x] 0.1.3.3 `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`
  - [x] 0.1.3.4 `CHANGELOG.md` (Keep a Changelog format)

## 0.2 Continuous integration
- [x] 0.2.1 PR workflow: fmt, clippy `-D warnings`, `cargo test --workspace`, on Linux + macOS; plus `cargo deny check` and `scripts/check_deps.py`
- [x] 0.2.2 Supply chain: `cargo deny` (licenses, advisories, bans incl. tantivy/rocksdb/sled), `cargo audit` scheduled
- [x] 0.2.3 Nightly extended property tests (`--features slow`) (fuzz/bench/coverage jobs moved to 1.11)
- [x] 0.2.5 Miri job for `persia-format` pure-logic modules

## 0.3 Developer tooling
- [x] 0.3.1 `justfile` with `fmt`, `lint`, `test`, `fuzz`, `bench`, `integration`
- [x] 0.3.3 Install Claude Code assets: `CLAUDE.md`, `.claude/agents/*`, `.claude/skills/*` committed and documented
- [x] 0.3.4 GitHub issue/PR templates, labels, milestone setup script
- 0.3.5 Progress tracking
  - [x] 0.3.5.1 `scripts/progress.py`: ROADMAP checkboxes → `PROGRESS.md` (milestone graph, burn-up, ready-next list)
  - [x] 0.3.5.2 CI job: `python3 scripts/progress.py --check` fails on stale `PROGRESS.md`

## 0.4 Documentation scaffolding
- [x] 0.4.1 `docs/adr/0000-template.md` + ADR process
- [x] 0.4.2 ADR-0001: record "own engine, no Tantivy" decision and allowed dependency list
- [x] 0.4.3 ADR-0002: blob hash choice (SPEC §18.1)
- [x] 0.4.4 `docs/architecture.md` (diagram from SPEC §3, glossary)

## 0.5 Repo tooling in Rust (replaces the Python scripts)
- [x] 0.5.1 `xtask` crate + `cargo xtask check-deps` (port of `scripts/check_deps.py`, with its tests)
- [x] 0.5.2 `cargo xtask progress` (port of `scripts/progress.py`; byte-identical `PROGRESS.md`)
- [x] 0.5.3 `cargo xtask github-setup`; repo-file and toolchain checks as Rust tests; delete Python, update CI/justfile/docs

---

# M1 — Format primitives (`persia-format`)
Deps: M0. Pure functions, no I/O. This is the foundation: be paranoid.

## 1.1 Integer & byte encodings
- 1.1.1 Little-endian read/write helpers with bounds-checked cursor (`Reader`, `Writer`)
  - [x] 1.1.1.1 `Reader` over `&[u8]` returning `Err(Corrupt)` on short reads, never panics
  - [x] 1.1.1.2 `Writer` over `Vec<u8>` with alignment helpers (`pad_to(8)`), plus the matching `Reader::skip_padding(8)` (absolute-offset alignment; non-zero padding is `Corrupt`)
  - [x] 1.1.1.3 [T] property: write/read round-trip for all primitive widths; fuzz `Reader`
- 1.1.2 Varint (LEB128 u32/u64) encode/decode
  - [x] 1.1.2.1 Encode/decode with overlong-encoding rejection
  - [x] 1.1.2.2 [T] property round-trip; golden vectors; fuzz decode
- [x] 1.1.3 Zigzag encoding for signed integers
- [x] 1.1.4 Checked arithmetic helpers for offsets/lengths (`Offset`, `Len` newtypes)
- [x] 1.1.5 `#![deny(clippy::arithmetic_side_effects)]` in `persia-format` (and later in engine segment readers); fix all hits with checked ops

## 1.2 Integer compression
- 1.2.1 Delta coding for sorted `u32` sequences
  - [x] 1.2.1.1 Delta encode/decode with strictly-increasing validation
  - [x] 1.2.1.2 [T] property round-trip for random sorted sets incl. empty, single, max values
- 1.2.2 Bit-packing of 128-value blocks (width 0..=32)
  - [x] 1.2.2.1 Scalar pack/unpack for all widths
  - [x] 1.2.2.2 Compute minimal bit-width for a block
  - [x] 1.2.2.3 Packed-size formula + bounds checks on decode input
  - [x] 1.2.2.4 [T] exhaustive width test; property round-trip; fuzz
  - [x] 1.2.2.5 Benchmark scalar throughput (baseline for SIMD later)
- 1.2.3 Postings block codec: delta + bit-pack 128 docids, varint tail block
  - [x] 1.2.3.1 Encode block with `last_docid` + width header (block-max `max_tf`/`min_norm` live in the Skip section only, ADR-0004)
  - [x] 1.2.3.2 Decode block into caller-provided buffer (no allocation in hot path)
  - [x] 1.2.3.3 `advance_to(target)` within a block (galloping/binary search)
- [x] 1.2.4 Frame-of-reference codec for numeric columns (i64/f64 via order-preserving bits)

## 1.3 Checksums & hashing
- [x] 1.3.1 CRC32C wrapper (`crc32c` crate) with streaming API
- [x] 1.3.2 xxh3-64/128 wrapper for content hashes and routing
- [x] 1.3.3 `Checksummed<T>` helper for header structures; [T] bit-flip detection test (flip every bit in a sample, assert detected)

## 1.4 Compression
- [x] 1.4.1 Codec enum (`None`, `Zstd`) + `compress_block`/`decompress_block`
- [x] 1.4.2 Decompression-bomb guard: caller supplies max uncompressed size, reject mismatches
- [x] 1.4.3 [T] fuzz decompress with random/mutated inputs
- [x] 1.4.4 Resource/I-O error class in `persia-format` so allocation failures (zstd context, output buffer) are not reported as `Corrupt`

## 1.5 Block framing (SPEC §5.2)
- [x] 1.5.1 `BlockHeader {crc32c, codec, uncompressed_len, stored_len}` encode/decode
- [x] 1.5.2 `BlockWriter`: split a byte stream into fixed-size blocks, emit block table
- [x] 1.5.3 `BlockReader`: random-access decode of block *i*, verifying checksum
- [x] 1.5.4 [T] property round-trip with random sizes; corrupted block detected and reported with block index
- [x] 1.5.5 Record each section's block size on disk so readers derive the bomb-guard `max_len` (SPEC §5.2); define DocStore rows larger than one block and the meaning of the section table's `codec` field
- [x] 1.5.6 Table-only block reader for object storage: open from the section tail (`block_count` first, then the table) and decode a separately fetched block (ADR-0006)

## 1.6 Container header & superblock (SPEC §4.2–4.3)
- 1.6.1 `Header` struct: magic, versions, flags, uuid, page size; encode/decode
  - [x] 1.6.1.1 Reject unknown `format_major`, unknown `incompat_flags`; accept unknown `rocompat_flags` as read-only
  - [x] 1.6.1.2 [T] golden header bytes; fuzz decode
- [x] 1.6.2 `Superblock` slot encode/decode with CRC
- 1.6.3 Slot selection logic: pick highest valid `seq`; handle both invalid, one invalid, equal seq
  - [x] 1.6.3.1 [T] truth-table unit tests + property test with random corruption
- [x] 1.6.4 ADR: superblock slot geometry (one slot per aligned 4 KiB page vs both in one page), what `page_size` governs, and `format_minor` on rewrite (ADR-0007 open questions; decide before 2.4.1)

## 1.7 Frames (SPEC §4.4)
- [x] 1.7.1 `Frame` decode and one-shot `encode_frame` with trailing CRC and 8-byte padding
- 1.7.2 `FrameScanner` (over a byte slice: mmap or a ranged read): iterate frames from an offset, stop at first invalid frame, report reason
  - [x] 1.7.2.1 Distinguish clean EOF vs torn tail vs mid-file corruption
  - [x] 1.7.2.2 [T] property: truncate file at every byte offset → scanner yields exactly the complete prefix frames
  - [x] 1.7.2.3 [T] fuzz scanner
- [x] 1.7.3 Frame kinds registry (WalChunk, Segment, Manifest, Padding, TombstoneSidecar) with forward-compat skip of unknown kinds
- [x] 1.7.4 Frame payload compression (§4.4 flags bit0): carry an uncompressed length and a bomb limit, decode via `decompress_block`
- [x] 1.7.5 Streaming frame encoder for large payloads (header with declared `payload_len`, incremental CRC, byte count verified on finish) and a public 24-byte header parser so range readers can size reads

## 1.8 Memory-mapped reader (the only `unsafe`)
- [x] 1.8.1 `MmapFile` wrapper over `memmap2` with documented safety contract (file not truncated while mapped)
- [x] 1.8.2 `Bytes`-like slice abstraction unifying mmap, heap and remote-cached buffers (`ByteSource`)
- [x] 1.8.3 ADR documenting the unsafe boundary + SIGBUS strategy

## 1.9 Golden fixtures
- [x] 1.9.1 Fixture generator binary producing deterministic sample containers (incl. a golden 12288-byte prefix: header + both slots at their absolute offsets, ADR-0008)
- [x] 1.9.2 Check-in small fixtures (< 100 KB) + test that opens all fixtures

## 1.10 Shared value types
- [x] 1.10.1 `BlobId` + `BlobRef { blob_id, size, mime, chunk_size, user_meta? }` with encode/decode (SPEC §9.1); hash algorithm per ADR-0002

## 1.11 CI for fuzz targets, benchmarks and coverage (moved from M0: they need M1's first targets)
- [x] 1.11.1 Nightly fuzz job for every `fuzz/` target (time-boxed per target, corpus cached as artifact); clippy + `cargo deny` over `fuzz/Cargo.lock`
- [x] 1.11.2 Weekly benchmark run storing results (regression tracking)
- [x] 1.11.3 Coverage report (`cargo llvm-cov`) uploaded as artifact, no gating yet

---

# M2 — Storage abstraction & local backends (`persia-storage`)
Deps: M1.

## 2.1 Core trait and types (SPEC §10.1)
- [x] 2.1.1 `Key`, `ObjectMeta`, `PutOutcome`, `Capabilities`, error taxonomy (`NotFound`, `Conflict`, `Transient`, `Corrupt`, `Io`)
- [x] 2.1.2 Define `Storage` trait + `ByteSource` integration (decide whether `get_range` returns `ByteSource`; a fallible `ByteSource` constructor that checks a ranged read returned exactly the requested length)
- [x] 2.1.3 Retry classification (`is_retryable`) and a generic retry policy helper (exp backoff + jitter, injectable clock)

## 2.2 In-memory backend
- [x] 2.2.1 `MemoryStorage` implementing all ops incl. `put_if_absent` atomicity
- 2.2.2 Deterministic failure-injection wrapper `FaultyStorage`
  - [x] 2.2.2.1 Inject: error on Nth call, latency, torn put (partial bytes), dropped put
  - [x] 2.2.2.2 Seeded RNG schedule so failures are reproducible
  - [x] 2.2.2.3 Inject "applied, response lost" (write lands, call returns `Transient`); [T] a retried `put_if_absent` finding its own write returns `Created` (SPEC §10.1)
- [x] 2.2.3 [T] conformance test-suite (generic over `Storage`) reused by every backend
- 2.2.4 `FaultyStorage` crash modelling, needed by 6.3.5 (found in the 2.2 review)
  - [x] 2.2.4.1 Sticky crash: `Fault::Crash` (or `crash()`) fails that call and every later one, so a test models process death instead of one failed call; `Fault::CrashAfter` applies the write first (commit outcome unknown, SPEC §10.4)
  - [x] 2.2.4.2 Volatile-until-crash writes (dropped fsync): writes are visible until `crash()`, then a seeded subset of the unsynced ones is lost

## 2.3 Local filesystem backend (object-per-file)
- [x] 2.3.1 Atomic `put`: write temp → fsync → rename → fsync dir
- [x] 2.3.2 `put_if_absent` via `O_EXCL`/`link` semantics
- [x] 2.3.3 `get_range` using pread; `list` with prefix; `delete`
- [x] 2.3.4 Path-safety: reject `..`, absolute keys, invalid UTF-8; segments over 255 bytes and (Windows) reserved names and trailing `.`/space as `InvalidInput`; `list` never resolves a prefix as a path (SPEC §10.1); [T] traversal attempts
- [x] 2.3.5 Run conformance suite
- [x] 2.3.6 Mapped reads of committed container ranges via `MmapFile` (SPEC §4.7, ADR-0010, ADR-0017): opt-in (`Container::enable_mapped_reads`), `pread` the default and the fallback; one window over `12288..log_end`, remapped after each commit and compaction (a failed remap keeps the previous window; after a compaction, none); zero-copy `read_frame_source`; `container::mapped` is the only `unsafe` call site outside `persia-format`. Automatic choice on local disks is 2.3.10
- [x] 2.3.7 Store keys that are segment-prefixes of each other (`a` and `a/b`, SPEC §10.1): name encoding so an object never collides with a directory; round-trip names byte-exactly on case-insensitive and normalizing filesystems (APFS NFD, NTFS); [T] the conformance model already generates such pairs
- [x] 2.3.8 Sweep stale `@tmp.*` files a crashed writer left (ADR-0012): which process may delete them (pid no longer running, or age), and when (open, or a `persia` CLI command)
- [x] 2.3.9 `windows-latest` CI job for `persia-storage` (found in the 2.3 review): the `cfg(windows)` code (positioned reads, reserved names) is otherwise never compiled; directory fsync on Windows is 2.4.8
- [x] 2.3.10 Choose mapped reads (2.3.6) automatically on local disk file systems only (ADR-0017): detect the file system type portably (`statfs` `f_type` / `MNT_LOCAL` / `GetDriveTypeW`; std exposes none, so FFI needs an ADR or an allowed crate); network and FUSE mounts keep `pread`

## 2.4 Single-file container adapter (SPEC §4)
- 2.4.1 Open/create container: write header, init both superblock slots
  - [x] 2.4.1.1 Exclusive writer lock (`flock`) with clear error when held
  - [x] 2.4.1.2 Read-only open mode (also when rocompat flags unknown)
- [x] 2.4.2 Append frames with group-fsync API (`append`, `sync`)
- 2.4.3 Commit: write inactive superblock slot, alternate (SPEC §4.5)
  - [x] 2.4.3.1 [T] crash simulation: fail after each step 1/2/3, reopen, assert previous or new commit visible, never partial
- 2.4.4 Recovery on open (SPEC §4.4, §8.6)
  - [x] 2.4.4.1 Choose superblock, scan from `log_end`, truncate torn tail (memory bounded only by the file size until 2.4.12)
  - [x] 2.4.4.2 [T] an invalid frame in `wal_start..log_end` is `Corrupt` and never truncated; creating a container never truncates an existing file (SPEC §4.7, §8.6)
- 2.4.5 Container compaction: rewrite live frames into new file + atomic rename (SPEC §4.6)
  - [x] 2.4.5.1 Dead-space accounting
  - [x] 2.4.5.2 [T] crash during compaction leaves a valid old or new file (crashes while writing the new file; the rename and directory-sync windows are tested in 2.4.11.2)
  - [x] 2.4.5.3 Windows: test whether an open mapping blocks `rename` over the container; if so, close mappings or defer the swap until readers drain (ADR-0010)
- [x] 2.4.7 Fuzz target: arbitrary bytes as container → open must return `Err` or valid, never panic
- [x] 2.4.8 Platform notes: fsync semantics on macOS (`F_FULLFSYNC`), Windows (`FlushFileBuffers`), documented + tested where possible
- [x] 2.4.9 Define `frame_seq` (SPEC §4.4): what it numbers, its first value after a rewrite, and whether recovery stops at a non-increasing one
- [x] 2.4.10 Read-write open after a scan ends in corruption (SPEC §4.4): keep the bytes after the stop offset (quarantine copy or explicit repair flag) instead of truncating them; distinguish acknowledged WAL frames via 2.4.9 (ADR-0013: a valid frame past the stop whose `frame_seq` is not above the last valid one can never be acknowledged data, e.g. stale blocks exposed by a crash during a file extension; decide whether "only such frames and zeros" may be truncated)
- 2.4.11 File-level fault injection for the local backend and the container (found in the 2.2.4 review: `FaultyStorage` sits above the fsync points)
  - [x] 2.4.11.1 A file-system shim under the 2.3 backend and the container adapter that can fail or drop `fsync`, lose a `rename` or directory entry while the data survives (or the reverse), and tear the tail of an append
  - [x] 2.4.11.2 [T] crash at every fsync point of 2.3.1 (temp → fsync → rename → fsync dir), of `Container::create` (temp → sync → link → remove → sync dir), of `Container::compact` (temp → sync → rename → sync dir, incl. `CompactedNotDurable`) and of the commit sequence (SPEC §4.5, §8): recovery yields the old or the new state, never a mix
- [x] 2.4.12 Bound recovery memory (found in 2.4): open reads `wal_start..log_end` and the tail after `log_end` into memory; scan frame by frame (header, bound, frame) with a streaming zero check for the torn-tail rule, so a large uncommitted Segment frame does not need its size in RAM
- [x] 2.4.13 Streaming append of large frames through `FrameEncoder` (a Segment written in chunks), with the same poisoning and `frame_seq` rules as `append`
- [x] 2.4.14 Decide when `format_minor = max(stored, own)` is recorded (SPEC §4.2): the header page has no second copy, so it is never rewritten in place; create and compaction write the merged minor into a new file, and a read-write open by a newer build leaves it until then (ADR-0015)
- [x] 2.4.15 Sweep stale `<name>.tmp.*` and `<name>.compact.*` files left by a `Container::create` or `compact` that crashed (same policy question as 2.3.8)
- [x] 2.4.16 Windows writer lock (found in the 2.4 review): `LockFileEx` is mandatory, so a writer blocks every reader of the container (SPEC §4.5.1); lock a sidecar `<name>.lock` file or a byte range past any data instead, and test it in the 2.3.9 Windows CI job
- [x] 2.4.17 Compaction copies each frame in bounded memory (found in 2.4.12: `Rewriter` reads a whole frame, so a large Segment needs its size in RAM; stream it with a running CRC, compressed frames included)
- [x] 2.4.18 Windows file identity for `compact`'s "the path still names this file" check (found in 2.4.16: the standard library exposes none; `GetFileInformationByHandle` needs FFI and an ADR)
- [x] 2.4.19 `persia` CLI: list and remove a container's quarantine files (ADR-0014) and stale temporary files (2.4.15)
- [x] 2.4.20 Make the kill -9 loop tear appends (found in 2.3.6): each frame is one `pwrite`, so `kill_9_loop_never_loses_an_acknowledged_commit_or_sync` almost never leaves a torn tail; stream large frames (as the mapped-reader loop does) so torn-tail recovery is exercised
- [x] 2.4.21 Compaction coalesces the writes of small frames (found in 2.4.17 review: a streamed copy writes header, payload pieces and trailer separately, about three `pwrite`s per WAL frame where one used to do), with a compaction benchmark to justify it

## 2.5 Dev environment (moved from M0 0.3.2; needs Docker)
- [x] 2.5.1 `docker-compose.dev.yml` skeleton (MinIO, fake-gcs-server, Azurite — filled in M12), pinned image digests

---

# M3 — Schema, documents, collections (`persia-engine::schema`)
Deps: M1. (Parallel with M2.)

## 3.1 Schema model (SPEC §6)
- [x] 3.1.1 Field types enum + per-field options (analyzer, boost, positions, k1, b, facet, stored)
- 3.1.2 `CollectionSchema` with validation (names, reserved words, limits, boost ranges)
  - [x] 3.1.2.1 Schema JSON (de)serialization with strict unknown-key rejection
  - [x] 3.1.2.2 Canonical encoding + schema hash (stable across versions)
- [x] 3.1.3 Schema evolution rules: allow add field; reject type change/analyzer change; [T] table-driven tests
- [x] 3.1.4 Ranking config (`static_rank`) and relaxation config structs with defaults

## 3.2 Document model
- [x] 3.2.1 `Document { id, fields: Map<FieldId, Value>, blob_ref? }` and `Value` enum (`blob_ref` uses `persia_format::BlobRef`, 1.10.1)
- [x] 3.2.2 Validation against schema (types, multi-valued fields, limits: doc size, field count, id length)
- [x] 3.2.6 `f64` field values: decide and spec whether `-0.0`/NaN are canonicalized (or rejected) on write, so range filters on order-preserving keys (SPEC §5.2.1) never silently exclude values
- [x] 3.2.3 CBOR encode/decode for DocStore; deterministic field order
- [x] 3.2.4 JSON ↔ Document conversion (strict/non-strict mode, SPEC §6.4)
- [x] 3.2.5 [T] property: random schema + random valid docs round-trip; invalid docs rejected with precise errors

## 3.3 Manifest model
- [x] 3.3.1 `Manifest { seq, fence_token, wal_start, collections: {name → {schema, segments[]}} }`; each segment entry holds its current tombstone sidecar (ADR-0024)
- [x] 3.3.2 Manifest encode/decode (versioned) + hash
- [x] 3.3.3 Manifest diff/apply helpers for commit planning
- [x] 3.3.4 [T] golden manifest fixtures; fuzz decode

---

# M4 — Text analysis (`persia-analysis`)
Deps: M0. (Parallel with M2/M3.)

## 4.1 Tokenizers
- 4.1.1 Unicode word segmentation (UAX#29) tokenizer with offsets
  - [x] 4.1.1.1 Token struct `{text, start, end, position}`
  - [x] 4.1.1.2 Handling of numbers, emails, URLs, apostrophes, CJK fallback (per-char)
  - [x] 4.1.1.3 [T] table of tricky inputs; property: offsets valid and monotonic
- [x] 4.1.2 Whitespace tokenizer, keyword tokenizer
- [x] 4.1.3 Max token length guard (drop/truncate, configurable)

## 4.2 Normalization & filters
- [x] 4.2.1 Unicode NFKC normalization filter
- [x] 4.2.2 Lowercasing (Unicode-aware, including `ß`, Turkish-i policy documented)
- [x] 4.2.3 Accent/diacritic folding (`ñ`→`n` optional per analyzer, `ü`→`u`)
- 4.2.4 Stopword filter with embedded lists
  - [x] 4.2.4.1 English list
  - [x] 4.2.4.2 Spanish list
  - [x] 4.2.4.3 Portuguese list
- [x] 4.2.5 Synonym filter (expansion at query time; config from schema) (SPEC §7.6 L1)
- [x] 4.2.6 Edge n-gram filter (for optional prefix indexing)

## 4.3 Stemmers
- [x] 4.3.1 Stemmer trait + Snowball-style English (Porter2)
- [x] 4.3.2 Spanish stemmer
- [x] 4.3.3 Portuguese stemmer
- [x] 4.3.4 [T] golden word lists per language; property: stemming is idempotent where documented

## 4.4 Analyzer pipelines
- [x] 4.4.1 `Analyzer` = char filters → tokenizer → token filters; builder + registry by name+version
- [x] 4.4.2 Built-ins: `standard`, `keyword`, `whitespace`, `en`, `es`, `pt`; SPEC §6.3 lists each one's full chain, its length guard and its folding preserved set (e.g. whether `es` keeps `ñ`, found in the PR #17 review), and whether `en` maps `’` to `'` before stemming (the English stemmer knows only U+0027, found in 4.3)
- [x] 4.4.3 Analyzer identity/versioning recorded in schema (SPEC §6.3); the schema remembers whether a definition named a version, so evolution binds an unversioned `analyzer` to the current field's version (SPEC §6.2.4)
- [x] 4.4.4 Zero-allocation fast path (reuse buffers via `TokenStream`)
- [x] 4.4.5 Benchmark: tokens/sec per analyzer; fuzz: arbitrary UTF-8 never panics
- [x] 4.4.6 Unicode version bumps (found in the 4.1 review): moving `persia_analysis::UNICODE_VERSION` (unicode-segmentation or toolchain update) bumps the version of every built-in analyzer whose output changes, with a test listing which ones (SPEC §6.3.1)
- [x] 4.4.7 Char-filter offset mapping (found in the PR #15 review): a char filter that changes the text records an offset map, and the analyzer maps token spans back to the original field value (SPEC §6.3.1); property: spans stay on char boundaries of the original value, their starts and ends never decrease (not strictly increasing: one replacement can yield several tokens, SPEC §6.3.1)
- [x] 4.4.8 Query-time analysis for fields whose analyzer stacks tokens (found in the 4.2 review): with `edge_ngram`, queries and synonym terms (SPEC §6.3.2) are analyzed without it, or `SynonymMap::find` never matches stacked prefixes; define the query-time analyzer in SPEC §6.3
- [x] 4.4.9 Folding fast path (found in the 4.2 review): `FoldingFilter` allocates for every non-ASCII token even when nothing folds; pre-scan for foldable characters, measured by the 4.4.5 benchmark
- [x] 4.4.10 Built-in `fr`, `de`, `it` analyzers (found in the PR #17 review): SPEC §6.3 lists them but 4.2.4, 4.3 and 4.4.2 plan only `en`, `es`, `pt`; add their stopword lists and stemmers, or scope them out of §6.3
- [x] 4.4.11 Unsupported analyzer versions (found in the 4.4 review): a stored schema naming an analyzer version this build lacks (written by a newer build, e.g. `es@2`) is reported as `Corrupt` by the manifest decoder; give it a distinct "unsupported" error class, like an unknown format feature (SPEC §6.2.2, §15)
- [x] 4.4.12 Allocation-free stemming and folding (found in the 4.4 batch B review): `StemmerFilter` allocates a `Vec<char>` and the stem for every word it stems, `FoldingFilter` a run buffer for every token it folds; give stemmers a reusable buffer (e.g. `Stemmer::stem_into(&str, &mut String)`), measured by the 4.4.5 benchmark (`docs/benchmarks.md`: `en` 2.45 → 3.05 M tokens/s, `de` 2.01 → 2.40 M) and checked by an allocation test (needs an allocation counter: a dev-only crate such as `dhat`, or an ADR for an `unsafe` counting allocator in tests)

---

# M5 — Segment writer & reader (`persia-engine::segment`)
Deps: M1, M3, M4. The heart of the index format.

## 5.1 In-memory index builder
- 5.1.1 Term → postings accumulator per field (docid, tf, positions)
  - [x] 5.1.1.1 Term interning/arena to avoid per-term allocation
  - [x] 5.1.1.2 Per-doc field length accounting (norms input)
- [x] 5.1.2 Column builders (keyword dict + ords, numeric, bool) per field
- [x] 5.1.3 DocStore builder (CBOR rows → zstd blocks)
- [x] 5.1.4 IdMap builder (id → docid, docid → id)
- [x] 5.1.5 Builder memory accounting (`size_in_bytes`) for flush thresholds
- [x] 5.1.6 Flush peak memory (found in the 5.1–5.2 review): `SegmentWriter::finish` holds every section as a `Vec` and copies it into the output, plus the grouped postings and dense norms and keys, so a flush peaks at 2–3× `size_in_bytes`; stream sections to the output (or a `Storage` writer) and document the bound the M6 flush threshold relies on
- [x] 5.1.7 Flush speed after 5.1.6 (found in its review): the measuring pass encodes each postings and freqs block only to learn its length (compute it from the bit widths and varint lengths instead), and `Framed` allocates a buffer per emitted block (`BlockWriter::take_output`); recover the ~18 % of `segment/finish` that 5.1.6 cost

## 5.2 Section writers (SPEC §5.3)
- 5.2.1 Term dictionary (FST) writer with term-info array
  - [x] 5.2.1.1 Sorted term emission; value = index into term-info
  - [x] 5.2.1.2 Term-info record encoding (postings off, df, ttf, skip off, positions off)
- [x] 5.2.2 Postings + freqs writer (128-blocks) with skip entries (`last_docid`, offset, `max_tf`, `min_norm`)
- [x] 5.2.3 Positions writer (opt-in per field)
- [x] 5.2.4 Norms writer (1-byte quantization table; document quantization error bounds)
- [x] 5.2.5 Columns writer (dictionary-encoded keyword, FOR numeric, roaring sparse)
- [x] 5.2.6 DocStore, IdMap, BlobRefs section writers; golden segment fixture with a `BlobRef` that has non-empty `user_meta` (SPEC §9.1, ADR-0011; a segment inside a container frame is 6.3.8)
- [x] 5.2.7 Segment footer writer: stats, section table, schema hash (with its `encoding_version` and field count, SPEC §6.2.5), CRC; trailing locator (SPEC §5.1)
- [x] 5.2.8 `SegmentWriter::finish() -> Bytes/Writer` orchestrating all sections with deterministic output (same input → same bytes)
- [x] 5.2.9 Decide which `section_kind`s are block-framed (a column in SPEC §5.3) and make TermInfo/Skip offsets into them stream positions (SPEC §5.2)

## 5.3 Segment reader
- [x] 5.3.1 Open: locate/validate footer, parse section table, verify schema hash (`CollectionSchema::hash_at` the recorded field count, SPEC §6.2.5) and version
- [x] 5.3.2 Term lookup: FST get + term-info decode
- 5.3.3 Postings iterator
  - [x] 5.3.3.1 Sequential `next()` over blocks
  - [x] 5.3.3.2 `advance(target)` using skip entries
  - [x] 5.3.3.3 Frequency access aligned with docid blocks
  - [x] 5.3.3.4 Block-level metadata access (`max_tf`, `last_docid`) for block-max WAND
- [x] 5.3.4 Positions iterator (phrase support)
- [x] 5.3.5 Norms accessor, columns accessor (typed readers)
- [x] 5.3.6 Doc fetch by docid (DocStore block cache) and by external id (IdMap FST)
- [x] 5.3.7 Tombstone overlay view (roaring) applied on iteration
- [x] 5.3.8 Prefix/range term enumeration (FST range) and Levenshtein-automaton intersection hook (used in M8)
- [x] 5.3.9 `#![deny(clippy::arithmetic_side_effects)]` on the segment-reader modules of `persia-engine` (continues 1.1.5)
- [x] 5.3.10 Block tables validated once per segment (found in 5.3): every accessor call (`terms`, `norms`, columns) and every document fetch (`document`, `id_of`, `blob_ref`) reopened its sections' `BlockReader`, re-verifying the block-table CRC; `Segment::open` now validates each table once and lookups reopen it in O(1) (`BlockReader::reopen`). Decoded TermInfo blocks are not yet reused across `terms()` calls: 5.3.11
- [x] 5.3.11 Decoded-block caching for postings (found in 5.3): `terms()` decodes its TermInfo blocks again on each call, and each `Postings` iterator decodes, and re-verifies the CRC of, the framed blocks it reads; share decoded blocks across iterators and queries (the SPEC §10.6 block cache) and measure it in 5.5.5
- [x] 5.3.12 Lazy column access (found in the 5.3 review): `numeric_column` and `keyword_column` decode a whole column into 8 bytes per document; read FOR blocks on demand (SPEC §5.2.1 makes any value O(1)) so a 100M-document segment does not need 800 MB per column
- [x] 5.3.13 Positions per document (found in the 5.3 review): `Postings::positions` decodes a block's whole positions run (capped at 2^26 values, a resource error beyond); decode only the current document's positions, on demand, so memory stays proportional to one document's term frequency
- [x] 5.3.14 Norms through the block cache (found in the 5.3.11 review): `Segment::norms` still copies, and re-checks the CRC of, a whole `Norms` section (`doc_count` bytes) on every call, outside the SPEC §10.6 cache; read it a block at a time through it, as columns are
- [x] 5.3.15 One block-cache budget per process (found in the 5.3.11 review): `Segment::open` gave every segment its own 8 MiB `BlockCache`, so a process with N segments could hold 8 MiB × N decoded bytes; it now shares one process-wide `BlockCache::shared` (8 MiB), and `Segment::open_with_cache` takes any other. Sharing one cache sized by configuration across all the segments of the embedded `Db` and of the server is 10.2.5 and 11.4.7
- [x] 5.3.16 Column scan speed (found in the 5.3.12 review): lazy columns read 30 M numeric keys/s and 11 M keyword ord lists/s against 491 M and 651 M for the eager ones (`segment_columns` in docs/benchmarks.md): every value takes a lock on its column's last decoded block and `ords` allocates a `Vec`; add a lock-free cursor for sequential scans (filters, sorting, facets in M7) and a non-allocating `ords`

## 5.4 Verification & tooling
- [x] 5.4.1 `Segment::verify()`: recompute all block CRCs, cross-check stats vs data; also the rules that span a whole section, which lookups do not check (found in 5.3): the `skip_off` chain and the end of the last skip region, TermInfo `max_tf`/`min_norm` equal to the extremes of their skip entries and norms, writers' minimal bit widths, a Skip section only when some df ≥ 128, `IdList` ids equal to the `DocStore` rows' ids; un-ignore `sections::skip_entries_not_back_to_back_are_corruption`
- [x] 5.4.2 `persia inspect segment` (CLI): footer, sections, sizes, per-field stats
- [x] 5.4.3 `persia dump segment --field f --term t` debugging output
- [x] 5.4.4 Column value ranges in `Segment::verify` (found in the 5.4 review): an `f64` key that decodes to NaN or `-0.0` and a `datetime` key outside the SPEC §6.4.1 range are corruption (SPEC §5.2.1), but only a stored field catches them now (against its row); check the key of every document that holds a value (a document without one holds an unspecified key, SPEC §5.3.5)
- [x] 5.4.5 A maximum term length for `text` fields (found in the 5.5 review): `TermDict` paths are bounded for keyword fields (`MAX_KEYWORD_LEN`) and the `IdMap` (`MAX_ID_LEN`), but a text term is bounded only by the document size (the token length guard is configurable, and a registered analyzer may omit it), so a crafted text `TermDict` sharing one long suffix across all its keys passes the structural check and makes the open-time stream cost keys × path length. Decide a SPEC-wide term cap (e.g. 32 KiB, enforced by the builder and checked at open) or a walk-cost budget, then bound text `TermDict`s in `Segment::max_key_len`
- [x] 5.4.6 Re-audit `segment/read/fst_check.rs` on every `fst` version bump (found in the 5.5 review): it re-implements fst 0.4.7's node parser to prove fst cannot panic, loop or overflow on a payload it accepts, and `fst` is pinned to `=0.4.7` for that reason; a bump compares the parser and the paths the engine uses (`get`, streams, range and automaton search) with the new code, re-runs the `fst_check` tests and fuzzes `segment`. Done as an enforced guard, not a one-off audit: `FST_AUDITED` (version, source, checksum) in `xtask/src/repo_checks.rs` fails the build on a bump until the checklist in the module's "Bumping `fst`" section is followed

## 5.5 Tests
- [x] 5.5.1 Property: build segment from random docs → read back every doc/term/posting/column exactly equals the model
- [x] 5.5.2 Golden segment fixtures (+ determinism test)
- [x] 5.5.3 Fuzz targets: footer parser, each section decoder, term dict, postings block decoder, full-segment open (the `segment` target opens raw bytes, and edits with valid CRCs to the block-framed sections, the raw FST sections and the footer)
- [x] 5.5.4 Large-doc / empty-collection / single-doc / 2^20-doc edge-case tests
- [x] 5.5.5 Bench: build throughput, term lookup, postings decode, doc fetch

---

# M6 — Write path, durability, merging (`persia-engine`)
Deps: M2, M5.

## 6.1 Memtable
- [x] 6.1.1 Mutable in-memory index structure (concurrent reads, single writer)
- [x] 6.1.2 Searchable view implementing the same iterator traits as a segment
- [x] 6.1.3 Upsert/delete semantics inside memtable (replace within same buffer)
- [x] 6.1.4 Size/age flush triggers
- [x] 6.1.5 `SearchView` parity for queries: term enumeration (prefix, range, automaton) and column access over segments and the memtable, for 7.1 `Prefix`/`Fuzzy` and 7.4.5 filters (found in 6.1.2)
- [ ] 6.1.6 Undo a failed `put` without rehashing the term arenas: `Arena::truncate` rebuilds the whole table under the write lock, O(terms) per rejected document (found in the 6.1 review)
- [ ] 6.1.7 Cache `MemtableView::df` if term statistics become hot: it walks the posting chain under the read lock on every call. The count is the view's `(docs, version)` cutoff, so it cannot reuse a live counter on the term, and a cache has to be keyed by that cutoff and bounded (found in 7.2)

## 6.2 WAL (SPEC §8.2)
- [x] 6.2.1 WAL record format: op (put/delete), collection name, payload, checksum
- [x] 6.2.2 Group commit: batch appends, single fsync, durability levels (`Buffered|Fsync|Async`)
- [x] 6.2.3 WAL chunk rotation and `wal_start` advancement
- 6.2.4 Replay: validate CRCs, stop at first invalid, idempotent
  - [x] 6.2.4.1 [T] truncate WAL at every byte → recovered state equals a prefix of operations
- [x] 6.2.5 Fuzz WAL replay

## 6.3 Flush & commit
- [x] 6.3.1 Memtable → segment (uses M5 writer) → storage frame/object
- [x] 6.3.2 Manifest build + commit protocol (local, SPEC §4.5)
- [x] 6.3.3 Atomic snapshot publication to readers (`ArcSwap`-style) with `seq`
- [x] 6.3.4 Auto-commit policy (size, interval, explicit `commit()`); the interval flushes a memtable whose writes never pause long enough for the idle trigger (SPEC §8.2 step 3, found in 6.1.4)
- [x] 6.3.5 [T] crash tests at every protocol step using `FaultyStorage`
- [x] 6.3.6 Expose the single-file container's logical objects (Segment/Manifest/WalChunk) to the engine through the `Storage` trait view (moved from 2.4.6): the container stores no object names, so the key → frame mapping is designed with the manifest and the commit protocol (SPEC §10.1, §4.5)
- [x] 6.3.7 On open, check that the Manifest frame's `seq`, `fence_token` and `wal_start` equal the active slot's, and that every frame location it holds is a frame of the right kind below `log_end`, with no object locations; remote readers reject frame locations (SPEC §8.7.1, found in 3.3)
- [x] 6.3.8 Golden container fixture holding a segment frame whose `BlobRefs` has a `BlobRef` with non-empty `user_meta` (deferred from 5.2.6, which pins the bare segment in `tests/fixtures/segment/v1.0/`): written by the commit path, opened by CI forever (SPEC §4.4, §9.1, §15)
- [x] 6.3.9 Log only what replay can apply (SPEC §8.2.1 "What may be logged"): a record of a collection whose create is committed, for an operation its memtable accepts (validation, term length, a docid left), checked before the record is acknowledged; decide whether the memtable applies first (an unlogged write is then briefly visible) or a dry-run check runs before the WAL. Includes whether a changed `strict` (§6.2.4) can make an old put fail replay. Test: a put the memtable rejects is never logged (built-in analyzers truncate tokens at 255 bytes, so `SegmentError::Full` is the reachable case) (found in the 6.2 review)
- [x] 6.3.10 Schema change and drop through commit (SPEC §8.2.1, §8.7.2): both flush every memtable first, so `wal_start` reaches the end of the log and no record of an earlier schema (or of the dropped collection) is replayed; reopen the collection's segments with the evolved schema. Test: a non-`strict` put with extra fields, then `strict`, then a crash and reopen (found in 6.3.9)
- [ ] 6.3.11 Flush without re-indexing: build the segment from the memtable's postings instead of analyzing every document again (ADR-0029, found in 6.3.1); bench the flush before and after

## 6.4 Upserts & deletes across segments
- [x] 6.4.1 Lookup docid by external id newest→oldest segment (IdMap FST)
- [x] 6.4.2 Tombstone sidecar format + versioning (SPEC §8.3, ADR); the writer builds each version as a superset of the previous one, checked before commit (the manifest only sees counts, SPEC §8.7.2)
- [x] 6.4.3 Delete-by-id and upsert through commit; visibility rules (read-your-writes). Until then a flush keeps only the memtable's live documents, so a delete or upsert of an id in an older segment is lost at the flush (ADR-0029, found in 6.3)
- [x] 6.4.4 [T] differential tests vs. reference model for random put/delete/commit sequences
- [ ] 6.4.5 Prune the commit's tombstone lookups by each segment's footer id range (`Segment::id_range`), once open or `Segment::verify` checks `min_id`/`max_id` against the `IdMap` (a wrong range would leave an older copy live). Helps time-ordered ids only; random ids (UUIDs) span every range. Bench `commit_inserts_*` before and after (found in 6.4.3: 64 segments double a 2 000-document commit)
- [x] 6.4.6 Drop a segment from the manifest in the commit that deletes its last live document, instead of keeping it listed with `deleted = doc_count` until a merge (ADR-0030 item 8)
- [ ] 6.4.7 Per-segment id membership filter (e.g. a Bloom filter section) so a commit skips segments that cannot hold an id, for random ids that id ranges do not prune; a format change (SPEC §5.3, ADR, golden file) (found in the 6.4 review)

## 6.5 Compaction / merging (SPEC §8.5)
- [x] 6.5.1 Merge policy (tiered) as pure function: segments → merge plans; [T] table tests
- 6.5.2 Segment merger: N readers → one writer, dropping tombstoned docs, remapping docids
  - [x] 6.5.2.1 Merge term dictionaries and postings (each input's terms into one dictionary the writer sorts, ADR-0031)
  - [x] 6.5.2.2 Merge columns, norms, doc store, id map, blob refs
  - [x] 6.5.2.3 Recompute statistics
- [x] 6.5.3 Background merge scheduler (thread pool, cancellation, backpressure); `Segment::verify` the merged segment before its commit, since a merge copies damage only `verify` finds (SPEC §5.5, found in the 6.5 review)
- [x] 6.5.4 Safe deletion of obsolete segments via snapshot epoch/refcount
- [x] 6.5.5 [T] property: merge(search results) == pre-merge results (same hits/scores within epsilon)
- [ ] 6.5.6 Streaming merge writer: encode the merged sections while the inputs are read, so a merge holds no more than a block per section instead of the whole merged segment in a `SegmentBuilder` (ADR-0031 item 8, found in 6.5.2)
- [ ] 6.5.7 Merge throughput: a merge-only postings reader that decodes whole lists (no per-term `Postings` cursor), and regrouping without a sort; goal: `segment_merge/copy_8` faster than `segment/add` on the same run (internal bench, `docs/benchmarks.md`; not a claim, BENCHMARKS.md)
- [ ] 6.5.8 Engine-driven container compaction (SPEC §4.6): `Database` names the live frames (the manifest's, plus the obsolete frames still referenced, 6.5.4) and the WAL range, compacts when dead space reaches the ratio, and publishes the relocated manifest (`Manifest::relocate`) (found in 6.5.4)
- [ ] 6.5.9 Merges that survive concurrent deletes: re-apply the deletions committed to its inputs since the plan to the merged segment (a docid map kept with the result) instead of discarding it (ADR-0032 item 4, found in 6.5.3)

## 6.6 Open/recovery orchestration
- [x] 6.6.1 Open sequence: superblock → manifest → replay WAL → memtable → ready (SPEC §8.6)
- [x] 6.6.2 Single-writer enforcement + read-only open for readers
- [x] 6.6.3 `persia verify` full database verification command
- [x] 6.6.4 Differential + crash-loop test harness (kill -9 child process in loop, reopen, check invariants) — shared with integration-tester
- [x] 6.6.5 Flush during replay: one collection's unflushed record holds `wal_start` back for all, so replay may hold more than a memtable threshold (or `MAX_DOCS` docids) of another collection; flush to a segment when a replayed memtable reaches its threshold instead of failing the open. Test: replay past the size threshold and the docid limit (found in the 6.2 review)
- [ ] 6.6.6 Refresh a `ReadOnlyDatabase` in place: a new snapshot from the next manifest and the WAL frames written since, instead of a reopen (ADR-0033 Alternatives, found in 6.6.2)
- [x] 6.6.7 `persia verify` with custom analyzers, and distinct exit codes for corruption and other failures (found in the 6.6 review); custom analyzers moved to 6.6.8, since a schema names only built-in analyzers today (SPEC §6.2.2)
- [ ] 6.6.8 `persia verify` with custom analyzers, once a schema can name an analyzer registered with `AnalyzerRegistry::register` (today `AnalyzerRef` accepts built-ins only, SPEC §6.2.2): a way to load them in the CLI, or a replay that validates records without analyzing their `text` fields (found in 6.6.7)
- [x] 6.6.9 A writer's open replays past a Manifest frame a cut commit left after `log_end`, then retires it with a commit that flushes nothing, so the WAL it logs next is not hidden behind it from later opens. A read-only open replays past that Manifest too while a fresh superblock read still leaves it after `log_end`, and stops only when the read names the frame as the committed manifest (SPEC §8.6, ADR-0033 item 7). Test: crash in a commit (and in the retiring open), reopen, write, crash, writer and read-only reopen; a synced put already behind the Manifest is visible to a read-only open before any writer retires it, and survives the retiring open with no later flush (found by the 6.6.4 crash loop in the 7.2 batch)
- [ ] 6.6.10 `persia inspect container` (CLI, read-only): the superblock (`seq`, `wal_start`, `log_end`), every frame from `wal_start` (kind, offset, length, `frame_seq`, whether after `log_end`), Manifest frames after `log_end` with their `seq`, and with `--id` the WAL records naming a document id. Replaces the throwaway debug test the 6.6.9 triage needed (crash-triage skill)

---

# M7 — Query engine & ranking (`persia-engine::query`)
Deps: M5 (M6 for end-to-end).

## 7.1 Query AST & parser
- [x] 7.1.1 AST: Term, Phrase, Prefix, Fuzzy, Bool{must,should,must_not,filter}, Boost, ConstantScore, MatchAll
- 7.1.2 User text → AST compiler (analysis per field, default AND, quoted phrases, `-` exclusion, `field:`)
  - [x] 7.1.2.1 Never fail on user input (invalid syntax degrades to plain terms)
  - [x] 7.1.2.2 [T] fuzz + table-driven tests
- [x] 7.1.3 Filter DSL (term, terms, range, exists, and/or/not) parser + validation against schema
- [x] 7.1.4 Query limits (max terms/clauses/expansions) with explicit errors

## 7.2 Collection statistics
- [x] 7.2.1 Aggregate `N`, `avglen`, `df` across live segments (cached per snapshot)
- [x] 7.2.2 Statistics provider trait (enables global stats in M13)

## 7.3 BM25F scorer (SPEC §7.2)
- [x] 7.3.1 Scorer implementation with per-field `k1`, `b`, weights; norm decode table
- [x] 7.3.2 Static rank contribution (`log1p`, `linear`, SPEC §6.2.3; a `saturation` function moved to 27.6.1)
- [x] 7.3.3 Score upper bounds per term/block (for WAND)
- [x] 7.3.4 [T] unit tests vs hand-computed examples; differential vs reference scorer within epsilon
- [ ] 7.3.5 (v1.0) Term proximity scoring: a bonus from the minimum span, or the pair distance, of the query terms, using positions already indexed (5.3.4). The bonus must keep pruning bounds valid (MaxScore and block-max WAND, SPEC §7.2, §7.3)

## 7.4 Matching & top-k
- [x] 7.4.1 Single-term, AND, OR, NOT iterators with skip
- [x] 7.4.2 Phrase matching using positions
- [x] 7.4.3 Top-k collector (bounded heap, tie-break determinism: score desc, then docid)
- 7.4.4 MaxScore/WAND, then block-max WAND
  - [x] 7.4.4.1 MaxScore baseline
  - [x] 7.4.4.2 Block-max WAND using skip metadata
  - [x] 7.4.4.3 [T] differential: pruned results == exhaustive results
- [x] 7.4.5 Filter evaluation to roaring bitmaps/column predicates; intersect with candidates
- [x] 7.4.6 Tombstone application; memtable as additional segment: an id the memtable has (put or delete, `MemtableView::lookup`) hides every segment copy of it, which the segments' tombstones do not yet delete before the commit (SPEC §8.3, found in the 6.4 review)
- [ ] 7.4.7 A disjunction for many clauses (relaxation expansions, SPEC §7.6: up to 50 prefix or fuzzy terms): a heap instead of a scan of every clause per document, and phrase candidates that copy positions only while starts survive; bench `search_top10/or` with 50 clauses before and after (found in the 7.4 review)
- [ ] 7.4.8 Prune conjunctions and `should` clauses beside `must` with block bounds (block-max AND, required plus optional as Lucene's ReqOpt): today a threshold reaches only a term, a pure `should` disjunction and what `must_not`/`boost` wrap (SPEC §7.3, found in 7.4.4)
- [ ] 7.4.9 Disjunction default: on `search_top10_pruning` `MaxScore` beats block-max WAND (`or` 902 µs vs 1.17 ms, `or_rare` 270 vs 432 µs); try block bounds in `MaxScore` (Lucene 9.6's block-max MaxScore) and fewer pivot sums in WAND, then pick the default by benchmark and update SPEC §7.3 (found in 7.4.4). Moves to M15.2 (where it's measured)
- [x] 7.4.10 Pruning trusts block bounds on the read path: a block whose decoded `tf` exceeds its Skip `max_tf` (or a norm below `min_norm`) drops hits silently instead of `Corrupt`; check `tf <= max_tf` at block decode, and add a fuzz arm that runs pruned `top_k_with` (both strategies) over corrupted segment bytes: `Err` or a result, never a hang (`Segment::verify` already checks it; found in the 7.4.4 review)
- [ ] 7.4.11 (v1.0) Configurable tie-break: a secondary key by column (for example `updated_at` descending) before docid. Today the order is score descending, then docid (7.4.3)
- [ ] 7.4.12 Plan `prefix` and `fuzzy`: expand each over the collection's terms once, at plan time (every view's `SearchView::terms`, ROADMAP 6.1.5), so every view scores the same terms; keep at most `max_expansions` and specify in SPEC §7.1 which terms win the cap (collection df descending, then bytes: each view's `TermEnum` df is its own, a segment's counting deleted documents, so rank by `StatisticsProvider::df`); `SearchPlan::new` rejects them today (`SearchError::Unsupported`; found in 6.1.5, shares the walk with 8.2.3 and 8.2.4.2)
- [ ] 7.4.13 Faster filters: a `range` scans every key of the column (~12 ns per document on `search_top10_filtered`); skip FOR blocks whose keys cannot match by their minimum and bit width (every key is at most `min + 2^width − 1`, no format change), and for a selective `must` check the column per candidate instead of building the whole bitmap (SPEC §7.3's "column predicates"), picked by cost (found in 7.4.5)

## 7.5 Sorting, pagination, facets
- [x] 7.5.1 Sort by column (asc/desc, missing handling) with top-k
- [x] 7.5.2 `offset/limit` and `search_after` cursor (SPEC §7.4)
- [x] 7.5.3 Facet counting over keyword columns with bitmap input; top-N merge across segments
- [x] 7.5.4 Result assembly: fetch stored fields, projection, blob_ref in hit
- [ ] 7.5.5 Keep column dictionaries across requests: a segment's `keyword_column` decodes its whole dictionary on every sort or facet request (`SearchView::keywords`), and the memtable copies its keyword values and column keys per call; cache them per segment (beside the block cache) and per memtable view (as its norms), and bench `search_request/sort_price` and `score_facet` before and after (found in 7.5)
- [ ] 7.5.6 Prune column sorts: `search_request/sort_price` visits every match (+21 % over a pruned score top-10 on 50 000 documents); skip FOR blocks whose keys cannot beat the worst kept hit (a block's minimum and bit width bound its keys, as 7.4.13) and stop a view early when its index sort matches (SPEC §5.4 `index_sort`, post-v1) (found in 7.5)

## 7.6 Parallel execution
- [ ] 7.6.1 Per-segment search tasks on a thread pool; deterministic merge. A threshold shared across views (`CollectionViews::top_k`, 7.4.6) skips "at most t" only from views that win the tie (earlier ones); one from a later view must skip strictly below t (found in the 7.4.6 review)
- [ ] 7.6.2 Query deadline/cancellation token

## 7.7 Explain
- [ ] 7.7.1 Per-hit score breakdown (per term/field) when `explain: true`

## 7.8 Tests & benchmarks
- [ ] 7.8.1 Differential suite vs. reference engine (random corpora/queries)
- [ ] 7.8.2 Relevance sanity corpus with judged queries; nDCG@10 baseline recorded
- [ ] 7.8.3 Criterion benchmarks: single term, AND/OR, phrase, filtered, sorted, facets
- [ ] 7.8.4 (v1.0) nDCG@10 relevance regression gate in CI, building on the 7.8.2 baseline (7.8.3 stays the Criterion benchmarks)

---

# M8 — Zero-results avoidance (`persia-engine::relax`)
Deps: M7. **Key differentiator — treat quality here as a product feature.**

## 8.1 Framework
- [ ] 8.1.1 `RelaxPlan` = ordered levels with per-level query transforms; configurable via schema/query (SPEC §7.6)
- [ ] 8.1.2 Orchestrator: run L0, evaluate `min_results`, escalate, stop or fill
- [ ] 8.1.3 Score offset per level to guarantee ordering across levels
- [ ] 8.1.4 Budget (`budget_ms`) and `truncated` flag
- [ ] 8.1.5 Response metadata struct (`level`, `applied`, `original`, `effective`, `corrected`)
- [ ] 8.1.6 `minimum_should_match` on `BoolQuery` (SPEC §7.1) for L2 `drop_terms` (found in the 7.1 review)
- [ ] 8.1.7 Source span and original text of each compiled clause in `Compiled`, for `original`/`effective`/`corrected` and the L3/L4 length rules (found in the 7.1 review)
- [ ] 8.1.8 Relaxed queries within `QueryLimits`: cut expansions to the remaining budget, report it in `applied` (SPEC §7.1.3, found in the 7.1 review)
- [ ] 8.1.9 Facets under relaxation: specify in SPEC §7.5 which matches a facet counts when the cascade runs (§7.6: the strict query, the level returned, or the union `fill` collects) and implement it with M8 (found in the 7.5 review)

## 8.2 Levels
- [ ] 8.2.1 L1 synonyms: expand terms with synonym groups
- 8.2.2 L2 drop_terms
  - [ ] 8.2.2.1 Term importance ordering (IDF + position + stopword flag)
  - [ ] 8.2.2.2 `minimum_should_match` ladder 100/75/50/1
  - [ ] 8.2.2.3 Single-term queries skip L2
- [ ] 8.2.3 L3 prefix expansion (FST range, cap expansions, rank by df)
- 8.2.4 L4 fuzzy (depends on 6.1.5)
  - [ ] 8.2.4.1 Levenshtein automaton builder (distance 1–2, transpositions)
  - [ ] 8.2.4.2 FST ∩ automaton term enumeration with cap + df ranking
  - [ ] 8.2.4.3 "Did you mean" corrected-query synthesis
  - [ ] 8.2.4.4 Length-based distance policy (SPEC §7.6 L4)
- [ ] 8.2.5 L5 fallback: static_rank / recency top-N; clearly flagged in response
- [ ] 8.2.6 Optional filter relaxation (`last_resort`) with explicit opt-in
- [ ] 8.2.7 (v1.0) Typo and exactness as a ranking signal: within a relaxation level, 0 edits rank above 1 edit, then 2, before BM25F; exact ranks above stemmed, then prefix (8.2.6 remains optional filter relaxation)

## 8.3 Controls & API
- [ ] 8.3.1 Per-query `relax` options override; per-collection defaults; `relax:false`
- [ ] 8.3.2 `fill` mode (continue to fill `limit` from looser levels)
- [ ] 8.3.3 Metrics: relaxation level histogram, % queries relaxed, % still empty

## 8.4 Quality evaluation
- [ ] 8.4.1 Synthetic typo/term-drop query generator (from corpus) with known ground truth
- [ ] 8.4.2 Metric: zero-result rate before/after relaxation; precision of first relaxed hit
- [ ] 8.4.3 Regression gate: zero-result rate on synthetic suite must be ≤ threshold
- [ ] 8.4.4 Document tuning guide (`docs/relaxation.md`)

---

# M9 — Blob store (`persia-blob`)
Deps: M1, M2.

## 9.1 Core model (SPEC §9)
- [ ] 9.1.0 ADR: blob upload staging and expected-id API (SPEC §18.8); update SPEC §9.3/§9.4/§9.6
- [ ] 9.1.1 Content hasher per ADR-0002 producing `BlobId` (type from 1.10.1), chunking parameters
- [ ] 9.1.2 Streaming chunker (`Read` → chunks with CRC and running content hash)
- [ ] 9.1.3 Blob manifest (chunk list) encode/decode

## 9.2 Local pack file
- [ ] 9.2.1 Pack header/superblock + `BlobChunk`/`BlobIndex` frames (SPEC §9.3)
- [ ] 9.2.2 Append blob (dedup check by id), fsync policy
- [ ] 9.2.3 Read full / range via chunk index (mmap-backed)
- [ ] 9.2.4 Recovery: torn tail truncation, index rebuild by scan
- [ ] 9.2.5 Fuzz pack reader

## 9.3 Remote layout
- [ ] 9.3.1 Key layout `blobs/<xx>/<id>/<chunk>` via `Storage`
- [ ] 9.3.2 Parallel chunk upload/download, resumable upload by chunk existence check
- [ ] 9.3.3 Small-blob single-object optimization (threshold)

## 9.4 Integration with documents
- [ ] 9.4.1 `put(doc, blob)` ordering: blob durable → document commit (SPEC §9.5)
- [ ] 9.4.2 Streaming API: `put_blob(reader)`, `get_blob_range(id, range)`
- [ ] 9.4.3 Document `blob_ref` stored in `BlobRefs` section; included in hits

## 9.5 Garbage collection
- [ ] 9.5.1 Mark: collect referenced ids from all live manifests/snapshots
- [ ] 9.5.2 Sweep: orphans older than grace period; pack-file rewrite for local
- [ ] 9.5.3 `persia verify`: referenced blob missing → error; checksum scan
- [ ] 9.5.4 [T] crash tests between blob write and doc commit; GC safety test (no live blob ever deleted)
- [ ] 9.5.5 `ObjectMeta.last_modified` (SPEC §10.1 update) so the sweep (9.5.2) can apply the grace period to remote objects

---

# M10 — Embedded API (`persia`)
Deps: M6, M7, M8, M9. First user-facing release: `v0.1`.

## 10.1 Public API design
- [ ] 10.1.1 `Db::open(path|url)`, `OpenOptions`, `Db::close`
- [ ] 10.1.2 `Collection` handle: create/get/drop, schema inspection
- [ ] 10.1.3 `put`, `put_batch`, `delete`, `get`, `commit`, `search`, `blob` APIs
- [ ] 10.1.4 Typed documents via `serde` (`put_struct`) + dynamic `Document`
- [ ] 10.1.5 Error type, `Result` aliases, stable error codes
- [ ] 10.1.6 Async-friendly wrapper (`spawn_blocking`) as optional feature `tokio`

## 10.2 Concurrency semantics
- [ ] 10.2.1 `Send + Sync` handles; reader snapshots; single-writer errors
- [ ] 10.2.2 Read-your-writes guarantees documented and tested
- [ ] 10.2.3 Background threads lifecycle (flush, merge) and clean shutdown
- [ ] 10.2.4 Read handles use mapped reads where the container is on a local disk (`Container::enable_mapped_reads_if_local`, ADR-0021; found in 2.3.10): on by default, `OpenOptions` can force `pread`; never on the writer's handle (a live mapping blocks tail truncation on Windows, ADR-0017)
- [ ] 10.2.5 One block cache per `Db` (found in 5.3.15): every segment the database opens reads through one `BlockCache` (SPEC §10.6) of `OpenOptions` size (default from 15.2.4), not the 8 MiB process-wide `BlockCache::shared`; its stats exposed for metrics

## 10.3 Docs & examples
- [ ] 10.3.1 Rustdoc with doctests for every public item
- [ ] 10.3.2 `examples/`: quickstart, products-search, blobs, relaxation demo
- [ ] 10.3.3 README quickstart (copy-paste works, tested in CI)

## 10.4 CLI (`persia-cli`)
- [ ] 10.4.1 `persia inspect|dump|verify|compact|stats` commands
- [ ] 10.4.2 `persia import` (JSONL) and `persia search` for quick experiments
- [ ] 10.4.3 `persia bench` quick micro-benchmark entry point

## 10.5 Release engineering
- [ ] 10.5.1 Versioning, changelog, `cargo publish --dry-run` in CI
- [ ] 10.5.2 Tag `v0.1.0-alpha` with explicit "format unstable" notice

---

# M11 — Server (`persia-server`, `persia-proto`)
Deps: M10.

## 11.1 Protocol
- [ ] 11.1.1 `persia.proto` v1 per SPEC §12.2 (collections, put stream, delete, get, search, blob stream, commit, stats)
- [ ] 11.1.2 Buf/lint config; breaking-change detection in CI
- [ ] 11.1.3 Rust codegen crate `persia-proto` (generated code committed; no `build.rs`, ADR-0003)
- [ ] 11.1.4 `cargo xtask proto`: regenerate `persia-proto` code; CI job fails if regeneration changes anything

## 11.2 gRPC service
- [ ] 11.2.1 Service skeleton (tonic) wired to embedded API
- [ ] 11.2.2 Collection admin RPCs
- [ ] 11.2.3 Document RPCs (put streaming, delete, get) with validation and size limits
- [ ] 11.2.4 Search RPC incl. relaxation info and `min_seq` read-your-writes
- [ ] 11.2.5 Blob streaming RPCs with range support and backpressure
- [ ] 11.2.6 Error mapping to gRPC codes; request IDs

## 11.3 HTTP/JSON gateway
- [ ] 11.3.1 axum routes mirroring gRPC (search, put, get, delete, health)
- [ ] 11.3.2 OpenAPI spec generation + tests

## 11.4 Operations
- [ ] 11.4.1 Config: flags → env → optional TOML; validation; safe defaults (SPEC §14)
- [ ] 11.4.2 Health: `/livez`, `/readyz` (ready after recovery)
- [ ] 11.4.3 Graceful shutdown: drain, flush, release locks
- [ ] 11.4.4 Metrics (Prometheus) + tracing/OTel (SPEC §12.4)
- [ ] 11.4.5 AuthN: API keys/bearer, TLS (rustls), optional mTLS
- [ ] 11.4.6 Limits: max message size, concurrent requests, per-request deadlines
- [ ] 11.4.7 Block cache size in the server config (found in 5.3.15): one `BlockCache` (SPEC §10.6) for all the server's segments, sized by a flag (default from 15.2.4), hit and miss counters in the metrics of 11.4.4

## 11.5 Packaging
- [ ] 11.5.1 Multi-stage Dockerfile: distroless, non-root, read-only rootfs
- [ ] 11.5.2 `docker-compose.yml` single-node example with volume
- [ ] 11.5.3 Image build in CI (multi-arch amd64/arm64), SBOM, vulnerability scan
- [ ] 11.5.4 [T] server integration tests in-process (tonic test channel)

---

# M12 — Cloud storage backends (`persia-storage`)
Deps: M2, M6, M9, M11.

## 12.1 Common cloud layer
- [ ] 12.1.1 Async `Storage` implementation over `object_store` + sync bridge for the engine
- [ ] 12.1.2 URL parsing & scheme dispatch (`s3://`, `gs://`, `az://`) (SPEC §10.2)
- [ ] 12.1.3 Credential resolution via provider default chains; explicit overrides; endpoint override
- [ ] 12.1.4 Conditional create (`put_if_absent`) capability detection per provider; clear error if unsupported
- [ ] 12.1.5 Retries, timeouts, request budget, tracing of each request

## 12.2 Providers
- [ ] 12.2.1 S3 (incl. MinIO / `If-None-Match` support, path-style option)
- [ ] 12.2.2 GCS (`ifGenerationMatch=0`, workload identity)
- [ ] 12.2.3 Azure Blob (`If-None-Match: *`, managed identity)
- [ ] 12.2.4 Each: run the shared conformance suite against the emulator in CI (MinIO, fake-gcs-server, Azurite)

## 12.3 Remote commit protocol (SPEC §10.4)
- [ ] 12.3.1 Remote layout implementation (manifests/segments/wal/tombstones/blobs)
- [ ] 12.3.2 Commit via `put_if_absent(manifests/<seq+1>)`; conflict handling
- [ ] 12.3.3 Reader: latest-manifest discovery (`list` + `CURRENT` hint) and snapshot open
- [ ] 12.3.4 WAL on object storage: chunk upload policy and latency/durability trade-off docs
- [ ] 12.3.5 [T] fault-injection suite: partial uploads, duplicate commits, reordered visibility, eventual-consistency simulation
- [ ] 12.3.6 Unknown commit outcome (`put_if_absent` error): keep uploads, resolve by reading `manifests/<seq+1>.mf` before retry/abort (SPEC §10.4); [T] error-after-apply never deletes referenced segments

## 12.4 Block cache (SPEC §10.6)
- [ ] 12.4.1 RAM cache (sharded LRU/ARC) keyed by `(object, block)`
- [ ] 12.4.2 NVMe disk cache tier with crash-safe index (or rebuild-on-start)
- [ ] 12.4.3 Range coalescing and readahead for sequential sections
- [ ] 12.4.4 Singleflight (dedup concurrent identical fetches)
- [ ] 12.4.5 Pinning of segment footers/FSTs and block tables (an owned, `Bytes`-backed `BlockTable`, ADR-0006); cache metrics
- [ ] 12.4.6 Optional request hedging for tail latency
- [ ] 12.4.7 Benchmark: cold vs warm query latency on MinIO

## 12.5 Replicated mode (SPEC §10.5)
- [ ] 12.5.1 Background replicator: upload committed segments then manifests, in order
- [ ] 12.5.2 Persistent `replicated_seq` + resume after restart
- [ ] 12.5.3 Read-only remote open of a replica; lag metric
- [ ] 12.5.4 `persia replicate` / `Db::replicate(url)` API; [T] crash/restart/resume tests
- [ ] 12.5.5 Restore: `Db::open_from_replica(url, local_path)` hydrate-on-demand

## 12.6 Docs
- [ ] 12.6.1 Quickstart per provider (IAM policy examples minimal-permission)
- [ ] 12.6.2 Consistency & cost guide (request counts per query/commit)

---

# M13 — Distribution & Kubernetes
Deps: M11, M12.

## 13.1 Writer lease & fencing (SPEC §11.3)
- [ ] 13.1.1 Lease object format + acquire/renew/release via conditional ops
- [ ] 13.1.2 Fence token in manifests; reader rejection of regressed tokens
- [ ] 13.1.3 Lease loss handling: stop writes, flush safely, surface error
- [ ] 13.1.4 [T] split-brain simulations (two writers, paused writer, clock skew)

## 13.2 Reader nodes
- [ ] 13.2.1 Manifest tailing with jittered polling and atomic snapshot swap (SPEC §11.4)
- [ ] 13.2.2 Cache warm-up on new segments (optional prefetch)
- [ ] 13.2.3 `persia_replica_lag_seconds` and readiness gating on lag threshold
- [ ] 13.2.4 Safe segment lifetime: readers pin old manifests while queries run; GC respects reader leases/grace

## 13.3 Sharding (SPEC §11.1)
- [ ] 13.3.1 Shard map definition + routing function (xxh3 mod N), stored in cluster manifest
- [ ] 13.3.2 Router: scatter-gather search, top-k merge, facet merge, deadline propagation
- [ ] 13.3.3 Partial-failure policy (fail / best-effort with `partial: true`)
- [ ] 13.3.4 Optional global statistics pre-phase (SPEC §11.5)
- [ ] 13.3.5 Write routing: id → shard; batch splitting; per-shard ordering
- [ ] 13.3.6 [T] differential: sharded cluster results == single-node results (with `global_stats`)

## 13.4 Roles & discovery
- [ ] 13.4.1 Server role flags (`writer`, `reader`, `router`) and combined mode
- [ ] 13.4.2 Shard discovery via headless Service/DNS; static list fallback
- [ ] 13.4.3 Client-side shard awareness endpoints for SDKs

## 13.5 Kubernetes packaging
- [ ] 13.5.1 Helm chart: writer StatefulSet, reader Deployment, Services, ConfigMap/Secret
- [ ] 13.5.2 HPA on QPS/CPU for readers; PDBs; topology spread; resource defaults
- [ ] 13.5.3 ServiceMonitor, Grafana dashboard JSON
- [ ] 13.5.4 Workload identity examples (IRSA, GKE WI, Azure WI)
- [ ] 13.5.5 `kind`-based e2e: deploy chart, ingest, query, kill writer, failover, rolling update

## 13.6 Resharding design (design only for v1)
- [ ] 13.6.1 ADR: split-by-hash-range using immutable segments + filtering merge
- [ ] 13.6.2 Prototype spike behind feature flag (optional)

---

# M14 — SDKs (Rust & Go)
Deps: M11 (stable proto); M13 for shard-awareness.

M14's scope now includes TypeScript. The heading stays "SDKs (Rust & Go)"; the title that names the scope is "SDKs (Rust, Go & TypeScript)". The network client is 14.5 (`@persiadb/client`). Embedded TypeScript stays 16.3 (`@persiadb/node`).

## 14.1 Conformance suite
- [ ] 14.1.1 Language-neutral scenario format (YAML): setup, calls, expected results
- [ ] 14.1.2 Scenarios: CRUD, search+filters, relaxation, facets, blobs (range), errors, read-your-writes
- [ ] 14.1.3 Runner harness spec so each SDK implements a small adapter

## 14.2 Rust SDK (`sdk/rust`, crate `persia-client`)
- [ ] 14.2.1 Connection/channel management, pooling, TLS, auth
- [ ] 14.2.2 Typed API: collections, put/put_batch, get, delete, search builder, facets
- [ ] 14.2.3 Blob helpers: upload from path/reader, ranged download stream
- [ ] 14.2.4 Retries (idempotent ops), deadlines, backoff
- [ ] 14.2.5 Client-side load balancing & shard-aware writes
- [ ] 14.2.6 Docs, examples, conformance adapter, publish dry-run

## 14.3 Go SDK (`sdk/go`, module `persia-go`)
- [ ] 14.3.1 Codegen (protoc-gen-go/grpc) wiring and module layout
- [ ] 14.3.2 Client with options pattern, context-first API, connection pool
- [ ] 14.3.3 Search request builder, typed results, relaxation info
- [ ] 14.3.4 Blob helpers (`io.Reader` upload, `io.ReadCloser` range download)
- [ ] 14.3.5 Retries/deadlines/load-balancing parity with Rust SDK
- [ ] 14.3.6 godoc, examples, conformance adapter, CI (`go vet`, `staticcheck`, race tests)

## 14.4 Compatibility
- [ ] 14.4.1 Compatibility matrix (SDK ↔ server) + version negotiation RPC
- [ ] 14.4.2 Release automation for both SDKs

## 14.5 TypeScript network client (`@persiadb/client`) (v1.0)
- [ ] 14.5.1 (v1.0) HTTP/JSON client `@persiadb/client` per SPEC §12, with a 14.1 conformance-suite adapter
- [ ] 14.5.2 (v1.0) Release automation for `@persiadb/client`, alongside 14.4.2

---

# M15 — Benchmarks, hardening, v1.0
Deps: M1–M14 (can start benchmarking early after M7).

## 15.1 Benchmark harness
- [ ] 15.1.1 Dataset acquisition scripts (public corpora; pinned versions/checksums)
- [ ] 15.1.2 Workload definitions (query logs: head/tail, typo-heavy, filtered, facet)
- [ ] 15.1.3 End-to-end harness (embedded and server modes), latency histograms (HdrHistogram), throughput
- [ ] 15.1.4 Comparison methodology doc + optional adapters to compare other engines (fair configs, published scripts)
- [ ] 15.1.5 CI regression gate (> 5% slowdown fails) with stored baselines

## 15.2 Performance work (driven by profiles)
- [ ] 15.2.1 SIMD bit-unpacking (runtime feature detection) behind safe wrappers
- [ ] 15.2.2 Postings decode & scoring hot-path allocation audit
- [ ] 15.2.3 FST/term-dict memory & mmap layout tuning
- [ ] 15.2.4 Cache sizing guidance and defaults from measurements

## 15.3 Reliability & chaos
- [ ] 15.3.1 Long-running soak test (24–72 h) with random ops + invariants checks
- [ ] 15.3.2 Crash-loop campaign (`kill -9`, disk-full, read-only FS)
- [ ] 15.3.3 Network chaos on cloud backends (latency, resets, throttling 503/429)
- [ ] 15.3.4 Fuzz campaign: corpus minimization, 100+ CPU-hours per target before v1

## 15.4 Security review
- [ ] 15.4.1 Threat model doc; resource-exhaustion limits verified by tests
- [ ] 15.4.2 Dependency & unsafe audit; SBOM, signed releases
- [ ] 15.4.3 Third-party review/pen-test of server surface

## 15.5 Format freeze & release
- [ ] 15.5.1 Finalize open questions (SPEC §18) as ADRs
- [ ] 15.5.2 Format 1.0 freeze: publish format spec doc, golden fixtures committed
- [ ] 15.5.3 Upgrade/compatibility test: open all historical fixtures
- [ ] 15.5.4 Documentation site (guides: embedded, server, cloud, k8s, relaxation tuning, operations runbook)
- [ ] 15.5.5 `v1.0.0` release checklist and announcement

---

# After v1.0

M0–M15 above are the MVP. Nothing in them is renumbered. A later milestone still does not start until its dependencies are done, unless its own text says otherwise.

| Target | Milestones |
|---|---|
| v1.0 | M0–M15, the MVP |
| v1.1 | M16 Embedded SDKs, M17 Highlighting and snippets, M18 Autocomplete, M19 Import, M31 Persia Studio, M32 Ranking rules, M33 Search features |
| v1.2 | M20 Encryption at rest, M21 Python SDK, M34 Backup, export, and query analytics |
| v2.0 | M22 Multi-tenant, M23 Persia Cloud, M24 CDC connectors, M25 Web dashboard |
| v2.1 | M26 UI components |
| v2.2 | M27 AI search |
| v2.3 | M28 On-device |
| v3.0 | M29 Geo, M30 Enterprise |

Two orderings differ from a straight list of the features. Multi-tenant is M22 and Persia Cloud is M23, so the hosted service can depend on one file per tenant. v2.1, v2.2, and v2.3 are separate targets so the UI kit does not wait on embeddings, and embeddings do not wait on a portable storage backend. Geo (M29) has no dependency on M26–M28; it stays on v3.0 beside the enterprise format work, and it can move earlier later without renumbering M16–M28. Encryption (M20) is a dependency of Persia Cloud. That only orders work inside the table above: v1.2 still comes before v2.0.

M32 is ranking rules (v1.1). Its BM25+/BM25L leaf (32.3.1) is v1.2 and ships only if the nDCG baseline justifies it. M33 (v1.1) is the search features v1.0 does not cover: CJK segmentation and pluggable tokenizers, reindex with an alias swap, delete-by-query, numeric and range facets, and distinct/group-by. M34 (v1.2) is user-facing backup and export, and analytics of zero-result queries. The read-only WASM build Persia Studio loads is 16.6.

---

# M16 — Embedded SDKs (target: v1.1)
Deps: M15 (format 1.0 and the M10 facade).

The design is ADR-0018 and `docs/sdk/` (C ABI, Go, TypeScript, distribution). This milestone schedules that design. It does not restate it. Network clients stay M14. The browser package is M28. A read-only WASM build for Persia Studio is 16.6 (v1.1); it is not the M28 package.

## 16.1 C ABI (`persia-ffi`)
- [ ] 16.1.1 Add `persia-ffi` (crate types `lib`, `staticlib`, `cdylib`) depending only on `persia`; record the edge in `ALLOWED` and the C-export module in `UNSAFE_ALLOWED` (ADR-0018, ADR-0003)
- [ ] 16.1.2 Safe Rust API over `Db` and `Collection`: open, close, create and open a collection, put, upsert, delete, get, put_batch, commit, search, blobs (SPEC §8.1, §9.6, §13.1)
- [ ] 16.1.3 Committed `persia.h` from `cargo xtask ffi-header` (no `build.rs`). C exports catch panics; that handle is poisoned afterwards (`docs/sdk/c-abi.md`)
- [ ] 16.1.4 Stable error codes mapped from the Rust error type. Corruption is an error. A mapped-page fault still ends the process (SPEC §17, ADR-0010)
- [ ] 16.1.5 [T] A document and a search round-trip through the C API; a panic in the core returns an error and poisons that handle; hostile UTF-8 is an error

## 16.2 Go (`sdk/persia`, cgo)
- [ ] 16.2.1 Module `github.com/tomiok/persiaDB/sdk/persia`, package `persia`, cgo linked to `libpersia` (ADR-0018, `docs/sdk/go.md`)
- [ ] 16.2.2 `Open`, collections, put, get, delete, search, blobs, `context.Context`, and `errors.Is` sentinels, with the cancellation check `docs/sdk/go.md` already describes
- [ ] 16.2.3 Checksummed prebuilt static libraries per OS and architecture, and a documented source build (`docs/sdk/distribution.md`)
- [ ] 16.2.4 Conformance adapter for the 14.1 scenarios with `transport: embedded` (SPEC §13.2)
- [ ] 16.2.5 [T] `go test` with the race detector; a file written by Go opens in Rust and returns the same hits

## 16.3 TypeScript (`sdk/node`, `@persiadb/node`)
- [ ] 16.3.1 N-API addon outside the Cargo workspace, linking the `persia-ffi` rlib (ADR-0003, ADR-0018, `docs/sdk/typescript.md`)
- [ ] 16.3.2 Promises, `PersiaError.code`, and `get` of a missing id returns null. Workers stay as `docs/sdk/typescript.md` describes them
- [ ] 16.3.3 Prebuilt `.node` binaries per platform, and an npm pack dry-run in CI
- [ ] 16.3.4 Conformance adapter, same scenarios as 16.2.4
- [ ] 16.3.5 [T] Node, and Bun or Deno, opens a file written by Rust and returns the same hits

## 16.4 Release
- [ ] 16.4.1 Publish only after the v1.0 tag. File format, C ABI, and package versions stay distinct numbers (ADR-0018, SPEC §15, `docs/sdk/distribution.md`)
- [ ] 16.4.2 Compatibility table for the Go module and `@persiadb/node` against the core tag they were built from
- [ ] 16.4.3 CI builds the static libraries and the `.node` packages. Users of the engine do not run that matrix (ADR-0018)

## 16.6 Read-only WASM build (v1.1)
- [ ] 16.6.1 (v1.1) Read-only WASM build of the core for Persia Studio (M31). It does not wait on the M28.2 package or the 28.1 portable backend. It does not fsync and it does not claim the local commit protocol (SPEC §4.5, ADR-0018). [T] The M31 fixture directory opens and a query returns the same ids as the native engine; a write API is absent

---

# M17 — Highlighting and snippets (target: v1.1)
Deps: M7, M15.

SPEC §7.7: `explain` is already 7.7.1. Highlights are post-v1 and land here. A highlight is a slice of the stored field value, using token spans (SPEC §6.3.1) and the char-filter offset map (4.4.7).

## 17.1 SPEC
- [ ] 17.1.1 Extend SPEC §7.7 and the §7.1 query sketch: fields, fragment size, tags, and max fragments. Offsets are into the stored value. The flag defaults off

## 17.2 Snippets
- [ ] 17.2.1 Build fragments from stored text. When the field has positions, align them with analyzer spans. Without positions, scan the stored value with the query analyzer, and document that this path is the slow one
- [ ] 17.2.2 A multi-value field, an empty value, or a value with no match yields no fragment for that value. That is not an error
- [ ] 17.2.3 [T] Property: every span is on a character boundary of the original value, and the slice matches the term after query-time analysis (SPEC §6.3.4). Fuzz stored text plus a query

## 17.3 API
- [ ] 17.3.1 `highlight` on the embedded query, in `persia.proto`, and on the HTTP search route. Generated stubs stay committed (ADR-0003)
- [ ] 17.3.2 The hit carries fragments. Score order does not change, including relaxation offsets (SPEC §7.6)
- [ ] 17.3.3 [T] Differential against a naive highlighter in the reference model: the same spans for a fixed analyzer

## 17.4 Cost
- [ ] 17.4.1 Bench snippet generation on the 15.1 harness. Highlighting has its own budget and reports truncation; it does not silently spend `relax.budget_ms` (SPEC §7.6)

---

# M18 — Autocomplete (target: v1.1)
Deps: M4, M8, M10, M15.

Search-as-you-type is a `suggest` API, not `search` with a short string. A field that indexed edge n-grams answers from those terms (SPEC §6.3.2, §6.3.4). Any other field answers from an FST prefix walk, the same dictionary walk as relaxation L3 (SPEC §7.6). Suggestions are completions. They do not run the relaxation cascade.

## 18.1 SPEC
- [ ] 18.1.1 SPEC: `suggest(prefix, fields, limit, filter)`. Say which fields use edge n-grams and which use the FST walk. Fuzzy suggestions stay off unless the request asks

## 18.2 Terms
- [ ] 18.2.1 Return terms that start with the analyzed prefix, ordered by document frequency, and capped by `limit`. A term whose live postings are all tombstoned is not suggested
- [ ] 18.2.2 Extra on-disk bytes, such as a popularity count, need an ADR, a format flag, and a golden fixture before they are written (SPEC §15). Query-time state stays out of the schema hash, as synonyms do (SPEC §6.2)
- [ ] 18.2.3 [T] Property: every suggestion starts with the prefix after query analysis; `limit` holds; deleting the last document for a term drops it. Fuzz the prefix

## 18.3 API
- [ ] 18.3.1 `Collection::suggest`, `persia suggest`, and the same RPC and HTTP route. Generated stubs stay committed
- [ ] 18.3.2 Keyword filters apply. `relax` is not a suggest parameter
- [ ] 18.3.3 [T] Index "red shoes" and "red socks". "re" returns both, "red s" narrows, and "rd" returns nothing while fuzzy is off

## 18.4 Latency
- [ ] 18.4.1 Bench in-process suggest and record p99. The path reads the dictionary, not stored fields

---

# M19 — Import (target: v1.1)
Deps: M10, M15.

`persia import` of JSONL is 10.4.2. This milestone adds Elasticsearch, Meilisearch, and SQLite FTS5. Documents go through the normal write path (SPEC §8). The importer does not keep the source engine's ranking.

## 19.1 Shape
- [ ] 19.1.1 Subcommands `elasticsearch`, `meilisearch`, and `sqlite-fts5` beside JSONL (10.4.2). Shared flags: source, mapping file, batch size, checkpoint path
- [ ] 19.1.2 Mapping from a source field to a Persia field type and analyzer (SPEC §6.1, §6.2). Unknown fields are stored extras or skipped, per flag. A `strict` collection rejects what SPEC §6.4 rejects
- [ ] 19.1.3 [T] Dry-run prints the mapping and the counts and writes no file

## 19.2 Elasticsearch
- [ ] 19.2.1 Read `_source` pages (scroll or point-in-time). Map text, keyword, long, double, boolean, and date onto SPEC §6.1. Any other type is stored JSON or a documented skip
- [ ] 19.2.2 Resume from the last committed batch. Auth and TLS options. Secrets stay out of logs (SPEC §17)
- [ ] 19.2.3 [T] Fixture HTTP responses, no live cluster: fields map, and a stop between batches then a rerun upserts without duplicating ids (SPEC §8.3)

## 19.3 Meilisearch
- [ ] 19.3.1 Page the documents endpoint with the same mapping, checkpoint, and secret rules as 19.2
- [ ] 19.3.2 [T] A fixture payload of a few documents round-trips the mapped values

## 19.4 SQLite FTS5
- [ ] 19.4.1 Read a JSON or CSV dump of the FTS5 content table, and document the `sqlite3` invocation that writes it. Do not link `rusqlite` (banned, ADR-0001). Do not shell out from a library crate. A CLI-only SQLite reader waits on its own ADR
- [ ] 19.4.2 [T] A checked-in dump of three rows becomes three documents, and importing it again upserts in place

## 19.5 Docs
- [ ] 19.5.1 One example per source, including what is not imported: scripted scores, Meilisearch ranking rules, and the FTS5 tokenizer configuration

---

# M20 — Encryption at rest (target: v1.2)
Deps: M2, M12, M15.

SPEC §1.2 leaves encryption to the disk and the storage service for v1. This milestone encrypts bytes Persia writes. It does not replace TLS (SPEC §12.3) or per-document access control (M30).

## 20.1 SPEC and ADR
- [ ] 20.1.1 ADR and SPEC §17: the threat model, the primitive, where the nonce lives, and the crate added to the allowed list and `deny.toml` in that same change. The key is not stored in the file
- [ ] 20.1.2 An `incompat_flags` bit (SPEC §15). A reader without the key gets a distinct error, not plaintext and not a generic `Corrupt`. Golden fixture of a header that sets the bit
- [ ] 20.1.3 SPEC §1.2: encryption at rest is this v1.2 flag, and the v1 non-goal stays true for files that do not set it

## 20.2 Local container
- [ ] 20.2.1 Encrypt the ranges the ADR names. The header stays readable enough to report that the file is encrypted. A bit flip fails closed. The ADR says whether the checksum covers plaintext or ciphertext, and a test locks that choice
- [ ] 20.2.2 A key-provider interface and one file-based provider. The key never appears in logs, `persia inspect`, or error text (SPEC §17)
- [ ] 20.2.3 [T] The wrong key fails. A crash at each commit step still reopens on the previous snapshot or the new one, never a mix (SPEC §4.5). An unencrypted fixture still opens. Fuzz ciphertext

## 20.3 Object storage
- [ ] 20.3.1 The same scheme for remote segment, manifest, and blob objects (SPEC §10.4, §9.4), so a bucket is not a plaintext copy
- [ ] 20.3.2 [T] Conformance on memory storage and one emulator: put with the key, get with the key, refuse without it

## 20.4 Rotation
- [ ] 20.4.1 A new key wraps new commits. Older frames stay readable until compaction rewrites them (SPEC §4.6). Document which keys a reader must still hold
- [ ] 20.4.2 [T] A file with frames from two keys opens with both. Dropping the old key fails the old frames and still opens the new ones

---

# M21 — Python SDK (target: v1.2)
Deps: M14, M16.

One engine (ADR-0001). The embedded package calls the C ABI from M16 (`docs/sdk/c-abi.md`). It does not reimplement segments, the WAL, or ranking. The network client speaks the server's HTTP JSON (SPEC §12) for processes that will not load a native library.

## 21.1 Embedded (`sdk/python`)
- [ ] 21.1.1 Package layout. Bind `persia.h` with cffi or ctypes. Installing the package does not require a Rust toolchain (`docs/sdk/distribution.md`)
- [ ] 21.1.2 Open, collection, put, get, delete, search, blobs, and commit. Exceptions carry the stable `code` from 16.1.4
- [ ] 21.1.3 Release the GIL around the native call. One database still has one writer (SPEC §8.4). A call already inside the engine does not cancel
- [ ] 21.1.4 Wheels that ship the prebuilt library, and a `pip` dry-run in CI
- [ ] 21.1.5 [T] Conformance adapter (`transport: embedded`, 14.1). A file written in Python opens in Rust and returns the same hits

## 21.2 Network client
- [ ] 21.2.1 Search, put, and get over the HTTP JSON gateway, with timeouts and the idempotent retries the Rust client documents (14.2.4)
- [ ] 21.2.2 [T] The 14.1 scenarios against `persia-server`, with the same expectations as `persia-client`

## 21.3 Docs
- [ ] 21.3.1 SPEC §13 names the Python package beside the Go and TypeScript bindings, on its own path. A short guide page

---

# M22 — Multi-tenant mode (target: v2.0)
Deps: M12, M15.

One `.persia` file per tenant, plus that file's blob sidecar. The tenant id is a path segment, not a field. Persia Cloud (M23) runs on this layout. Per-document principals are M30.

## 22.1 Isolation
- [ ] 22.1.1 ADR: where tenant files live, and the rule that one query never opens two tenants. SPEC §1.2 already rules out cross-collection queries; this rules out cross-file queries
- [ ] 22.1.2 The server routes a credential to one tenant file. The wrong tenant id is not found, and the handler does not stat another tenant's path
- [ ] 22.1.3 [T] Two tenants and the same document id produce independent hits. The test fails if one request can see the other file

## 22.2 Backups
- [ ] 22.2.1 Copy one tenant to an S3-compatible URL on a schedule, reusing replicate and restore (12.5) rather than a second uploader
- [ ] 22.2.2 Restore one tenant without opening the others. [T] After restore, search matches the backed-up committed snapshot
- [ ] 22.2.3 Quotas: a max file size and a max document count. Over quota is a distinct error, and the write is refused before commit

## 22.3 Operations
- [ ] 22.3.1 Create, list, and delete a tenant. Delete is explicit. When a backup exists, the grace period is documented and tested
- [ ] 22.3.2 Metrics may carry a tenant label. Document the cardinality cap (SPEC §12.4)

---

# M23 — Persia Cloud (target: v2.0)
Deps: M13, M20, M22.

A hosted service. The data plane is `persia-server` on tenant files (M22), deployed with the M13 chart, with encryption from M20. The control plane is accounts, projects, and API keys. A local file keeps serving reads. After sync, the cloud project is the source of truth.

## 23.1 Control plane
- [ ] 23.1.1 ADR: what the control plane stores, and what stays inside the tenant file. A project owns one or more tenants
- [ ] 23.1.2 Free-tier limits (tenants, documents, bytes, request rate) published as numbers. Over the limit is a machine-readable error, not a silently truncated index
- [ ] 23.1.3 API keys for a project. Opening a local path still has no auth (SPEC §12.3)

## 23.2 Sync
- [ ] 23.2.1 SPEC for the sync protocol. The cloud project is the source of truth. The local file serves reads while offline and queues writes in an outbox
- [ ] 23.2.2 Push committed local batches and pull remote commits. The ADR picks the conflict rule, last writer wins or a stored conflict, and the tests lock that rule
- [ ] 23.2.3 A search reads the open local snapshot and does not wait on the network. The response can carry sync lag
- [ ] 23.2.4 [T] An offline put is searchable locally, then on the cloud after reconnect. A cloud put appears locally after pull. A crash during push drops neither a committed local batch nor a remote commit, and it does not duplicate an id

## 23.3 Client
- [ ] 23.3.1 Open a cloud project by URL. The credential is an API key, documented next to the provider chains (SPEC §10.3), and it is not described as a cloud IAM role
- [ ] 23.3.2 Meter requests and stored bytes as metrics. Taking payment is out of scope; the ADR says so

## 23.4 Operations
- [ ] 23.4.1 Runbook: Helm install of the data plane, free-tier limits, backup age (M22), and encryption keys (M20)
- [ ] 23.4.2 Document the operator URL. The UI itself is M25; this leaf does not build a second one

---

# M24 — CDC connectors (target: v2.0)
Deps: M11, M15.

A connector tails Postgres, MySQL, or Supabase and writes through the public API (SPEC §8.1). It is its own process. Indexing does not run on the query path. Supabase uses the Postgres connector and does not get a second parser.

## 24.1 Framework
- [ ] 24.1.1 ADR: where the checkpoint lives, delivery (at-least-once plus upsert), and how a row becomes a document id
- [ ] 24.1.2 Map a table to a collection, the primary key to the document id, and the common SQL types onto SPEC §6.1. A column with no mapping is skipped or stored, per flag
- [ ] 24.1.3 [T] Kill the connector after a commit and start it again. Committed rows are not lost and ids are not duplicated (SPEC §8.3)

## 24.2 Postgres
- [ ] 24.2.1 Logical replication. The ADR picks the plugin or `pgoutput`. Insert, update, and delete
- [ ] 24.2.2 [T] Docker-compose Postgres: a committed row is searchable within the documented lag, and a delete stops matching

## 24.3 MySQL
- [ ] 24.3.1 A binlog reader for insert, update, and delete, with the same checkpoint and mapping as 24.1
- [ ] 24.3.2 [T] The same assertions as 24.2.2 against a MySQL container

## 24.4 Supabase
- [ ] 24.4.1 Document the publication and the replication slot. Reuse 24.2
- [ ] 24.4.2 [T] A fixture publication that includes a delete. After the connector applies it, the row does not match

## 24.5 Operator
- [ ] 24.5.1 `persia sync postgres`, `mysql`, and `supabase`. A config file. Secrets redacted (SPEC §17)
- [ ] 24.5.2 A lag metric, and a distinct error when the slot or the binlog position is gone

---

# M25 — Web dashboard (target: v2.0)
Deps: M11, M15.

A UI on `persia-server`: what is indexed, what a query did, and which queries came back empty. Persia Cloud links here (23.4.2). Auth is the server's API keys (SPEC §12.3). There is no second account system.

## 25.1 Explorer
- [ ] 25.1.1 ADR: static assets served by the server, or a separate app under `website/`. Either way it calls only the public HTTP API
- [ ] 25.1.2 Collections, field types, and segment stats from the existing inspect and stats surfaces (5.4, 10.4)
- [ ] 25.1.3 [T] An empty database and one collection with documents both render

## 25.2 Playground
- [ ] 25.2.1 Run a search. Show hits, the relaxation object (SPEC §7.6), and explain when the request set it (7.7.1)
- [ ] 25.2.2 Show highlight fragments and suggestions when the response includes them (M17, M18). A v1.0 server, which has neither, still shows hits

## 25.3 Relevance
- [ ] 25.3.1 Edit field boosts, synonyms, and relaxation defaults inside the evolution rules (SPEC §6.2.4)
- [ ] 25.3.2 ADR and SPEC: where a pin, a hide, or a query rewrite is stored, and when those rules run relative to scoring. They do not drop filters (SPEC §7.6). Any on-disk change follows the format discipline
- [ ] 25.3.3 The dashboard edits those rules only after 25.3.2, and it does not invent a second layout
- [ ] 25.3.4 [T] A pin puts that id first, a hide removes it, and a synonym edit changes the next search. The behavior is the one 25.3.2 specified. Rules do not drop filters (SPEC §7.6)

## 25.4 Analytics
- [ ] 25.4.1 Record the query text, the hit count, and the relaxation level. The zero-result list is the queries that stayed empty, including after the cascade
- [ ] 25.4.2 Retention, sampling, and a switch that stops storing the raw query text (SPEC §17)
- [ ] 25.4.3 [T] A query that is still empty after relaxation appears on the zero-result list. A query that matched only through fuzzy does not

---

# M26 — UI components (target: v2.1)
Deps: M11, M17, M18.

A search box, autocomplete, hits, and facets for React, Vue, and Svelte. One framework-free client talks HTTP JSON to the server (SPEC §12). The widgets do not embed the engine. `@persiadb/node` (M16) stays the in-process SDK; these packages are the browser UI.

## 26.1 Client
- [ ] 26.1.1 `@persiadb/ui` data client: search, suggest, and facets. No ranking code
- [ ] 26.1.2 A state helper for the text, the filters, the facet refinements, and the page. The three bindings share it

## 26.2 Widgets
- [ ] 26.2.1 Search box, suggestion list (M18), hits with a highlight slot (M17), and a facet list (SPEC §7.5)
- [ ] 26.2.2 React, Vue, and Svelte packages, each a wrapper over 26.1
- [ ] 26.2.3 [T] The framework-free core against a fixture HTTP response. One component test per framework: type a prefix, see a suggestion, see a hit

## 26.3 Docs
- [ ] 26.3.1 One example per framework on the website, pointed at a local server

---

# M27 — AI search (target: v2.2)
Deps: M9, M15, M17.

Hybrid text and vector search, an embedding model shipped with the repo, an MCP server, and retrieval over one file. SPEC §1.2 and §18.6 reserve vectors for after v1. This milestone returns passages. It does not call a chat model to write an answer.

## 27.1 Vectors
- [ ] 27.1.1 ADR for the segment layout (SPEC §18.6), the metric, and the `incompat_flags` bit (SPEC §15). A reader that does not know the bit refuses the file
- [ ] 27.1.2 Field type `vector`: a fixed dimension of finite `f32` values. The schema hash includes the dimension (SPEC §6.2). Evolution can add the field and cannot change the dimension
- [ ] 27.1.3 Writer and reader for the vector section, including the cluster postings the ADR chose. A golden fixture
- [ ] 27.1.4 [T] A round-trip property. Fuzz the section. Truncation and a NaN are `Corrupt`

## 27.2 Hybrid query
- [ ] 27.2.1 SPEC §7: a query may carry text, a vector, or both. Document how the two rankings combine. Filters still apply and are not relaxed (SPEC §7.6). Hybrid combination uses RRF (reciprocal rank fusion)
- [ ] 27.2.2 [T] Differential against a brute-force scan on the reference model, within a stated epsilon. A filter removes a near neighbor

## 27.3 Embeddings
- [ ] 27.3.1 Ship one embedding model in the repo. Record its license, its dimension, and a checksum of the weights. Embedding does not call the network
- [ ] 27.3.2 Helpers that fill a `vector` field from a text field at index time and at query time. A caller may still supply the vector
- [ ] 27.3.3 [T] The same string embeds to the same vector. One golden sentence pins the model

## 27.4 MCP
- [ ] 27.4.1 An MCP server exposes `search`, `get`, and `suggest` on the public API. The default is read-only
- [ ] 27.4.2 [T] A scripted MCP client and `Collection::search` return the same ids on a fixture database

## 27.5 Single-file RAG
- [ ] 27.5.1 Chunks live in a collection: text, a vector, and a `blob_ref` or a source id (SPEC §9). Retrieval returns those chunks with snippets (M17)
- [ ] 27.5.2 The command returns passages and citations. It does not call an external model. The docs say that generating an answer happens outside this process
- [ ] 27.5.3 [T] Two blobs in one file. The returned chunk is the one that contains the query term, and the citation id is that blob

## 27.6 Ranking features
- [ ] 27.6.1 Static rank function `saturation`, `v / (v + pivot)` with a `pivot` key in `ranking.static_rank` (SPEC §6.2.3, §7.2); deferred from 7.3.2. SPEC and schema JSON first; a schema written with it is unsupported by older builds. Moves to the M32 function-score item (32.2.1); this leaf stays

---

# M28 — On-device (target: v2.3)
Deps: M2, M16.

iOS and Android link `libpersia` (M16). WASM does not, until a `Storage` backend exists with no mmap and no POSIX fsync (ADR-0018, `docs/sdk/typescript.md`). That backend is 28.1. The ADR states what a crash can lose. It does not assume the local commit protocol (SPEC §4.5) holds unchanged.

## 28.1 Portable storage
- [ ] 28.1.1 ADR: the backend, which SPEC §10.1 capabilities it does not offer, and what a crash can drop. This is the gap ADR-0018 left open
- [ ] 28.1.2 Implement that backend. Run the conformance suite for the capabilities it claims, and test that the ones it lacks return unsupported

## 28.2 WASM
- [ ] 28.2.1 `@persiadb/wasm` (`sdk/wasm`) on 28.1. Do not ship a build of the mmap container that merely hopes the host can map it (ADR-0018)
- [ ] 28.2.2 A browser demo opens an in-memory or origin-private database, puts a document, and searches, with no network
- [ ] 28.2.3 [T] The embedded conformance subset that 28.1 supports passes in the wasm package

## 28.3 Apple platforms
- [ ] 28.3.1 A package that links `libpersia.a` for a device and for the simulator. Document the slices
- [ ] 28.3.2 [T] A simulator test opens a file in the app sandbox and searches it

## 28.4 Android
- [ ] 28.4.1 An archive that links the same static library through the NDK, via JNI or the C ABI
- [ ] 28.4.2 [T] An emulator test with the same contract as 28.3.2

---

# M29 — Geo filters and sorting (target: v3.0)
Deps: M7, M15.

A `geo_point` field, a bounding box, a circle, and sort by distance. Nothing here depends on M26–M28. The target stays v3.0 so the new field type lands with the other v3.0 format work. These are filters, so relaxation does not drop them (SPEC §7.6).

## 29.1 SPEC
- [ ] 29.1.1 SPEC §6.1: `geo_point`, WGS84, and the valid ranges. Adding the field is allowed evolution; changing it is not (SPEC §6.2.4)
- [ ] 29.1.2 SPEC §7: bounding box, circle, and distance sort. The circle uses haversine. The antimeridian and the poles have specified behavior

## 29.2 Index
- [ ] 29.2.1 ADR for the on-disk encoding and the format flag (SPEC §15). A golden fixture
- [ ] 29.2.2 Writer, reader, and a filter bitmap intersected like any other filter (SPEC §7.3)
- [ ] 29.2.3 [T] A point inside the box matches and a point outside does not. The antimeridian and the poles follow 29.1.2. Distance sort is non-decreasing. A corrupt coordinate is an error. Fuzz the section

## 29.3 API
- [ ] 29.3.1 The query JSON and the embedded API accept the filters and the sort
- [ ] 29.3.2 Bench a selective geo filter against a full scan on the 15.1 harness. Record the numbers, and do not treat them as a promise (SPEC §16.2)

---

# M30 — Enterprise (target: v3.0)
Deps: M13, M20, M22, M25.

Per-document access control, an audit log, SSO, and an on-prem bundle. Tenant isolation stays M22. Encryption of bytes at rest stays M20. This milestone is the layer above those.

## 30.1 Document access control
- [ ] 30.1.1 ADR and SPEC: allow and deny principals stored on the document. The request cannot turn the query-time filter off. Embedded mode with no principal stays open, as SPEC §12.3 says, unless the caller passes one
- [ ] 30.1.2 The filter runs for every search, including relaxed levels (SPEC §7.6). A forbidden id never appears in hits, facets, or suggestions
- [ ] 30.1.3 [T] Two principals and two documents. A query whose text matches both returns one. A server request with access control on and no principal matches nothing. Embedded mode with no principal stays open (SPEC §12.3, 30.1.1)

## 30.2 Audit log
- [ ] 30.2.1 An append-only log of who did what, to which tenant and collection, and when. A hash chain makes a flipped byte detectable. This log is not the WAL, and compaction does not drop it
- [ ] 30.2.2 Export, a retention period, and a page on the M25 dashboard
- [ ] 30.2.3 [T] A put and a denied read each append one record. A flipped byte fails verification. Fuzz the log decoder

## 30.3 Single sign-on
- [ ] 30.3.1 OpenID Connect authorization-code login on the server. The identity is the principal from 30.1. API keys still work for machines (SPEC §12.3)
- [ ] 30.3.2 [T] A local identity-provider stub issues a token, a search runs as that principal, and the audit record names the principal

## 30.4 On-premises packaging
- [ ] 30.4.1 The M13 Helm chart, an air-gapped image bundle, the SBOM from 15.4.2, and the M20 key provider. Booting the chart does not call Persia Cloud
- [ ] 30.4.2 An install guide for the cluster, single sign-on, backup (M22), and audit export. The guide says the hosted control plane (M23) is not in the bundle

---

# M31 — Persia Studio (target: v1.1)
Deps: M16 (the WASM build of the core, shared with the TypeScript SDK).

A browser client that inspects a Persia database. It lives entirely in the browser. There is no server. The core is compiled to WASM, the build shared with the TypeScript SDK work in this version (M16), so the page does not carry a second engine (ADR-0001). The user picks the `database.persia/` directory with the File System Access API. That API is available in Chromium-based browsers. Firefox and Safari do not offer it, and this version does not cover them with a server or an upload. Read-only: browse segments and the manifest, view documents, see tombstones, run queries and the BM25 score breakdown, and inspect WAL state. Write mode, and a local `persia studio` server, are out of scope. This milestone is the seed of the v2.0 dashboard (M25). It does not build that dashboard. The read-only WASM build this page loads is 16.6 (v1.1). This milestone does not wait on M28.2.

## 31.1 Page and WASM
- [ ] 31.1.1 A static page outside the Cargo workspace. It loads the WASM build shared with the TypeScript SDK (M16). No `build.rs` in a workspace crate (ADR-0003). After the page loads, it does not talk to a server. The read-only build is 16.6
- [ ] 31.1.2 The user picks the `database.persia/` directory with the File System Access API. The page reads the files and passes the bytes to the core. It does not compile the mmap container to WASM (ADR-0018, ADR-0010, SPEC §4.7). Firefox and Safari have no directory picker; the page says so
- [ ] 31.1.3 The adapter is read-only. It does not fsync and it does not claim the local commit protocol (SPEC §4.5). Cancelling the picker leaves the page idle
- [ ] 31.1.4 [T] A fixture directory is opened through a stand-in for the picker. The listed objects match the directory, and no file in it changes. A cancelled picker renders no database

## 31.2 Inspect
- [ ] 31.2.1 Segments and the manifest: segment ids, document counts, deletions, and the commit `seq` (SPEC §5, §8.7). A corrupt segment is an error on that row (SPEC §17), not an empty database
- [ ] 31.2.2 A stored document by id (SPEC §6.4). A missing id is shown as missing
- [ ] 31.2.3 Tombstones for a segment: the sidecar version and the deleted ids (SPEC §8.3). A segment with no sidecar shows none
- [ ] 31.2.4 WAL state: `wal_start`, the frames still needed, and whether they would replay (SPEC §8.2, §8.6). The page does not append a record
- [ ] 31.2.5 [T] On a fixture directory, the segment list, one document, one tombstone, and the WAL range match the engine reading the same bytes

## 31.3 Query
- [ ] 31.3.1 Run a query and list the hits. Ranking and the relaxation object come from the core (SPEC §7.2, §7.6)
- [ ] 31.3.2 The BM25 score breakdown for a hit when explain is on (SPEC §7.2, §7.7, ROADMAP 7.7.1). The page does not recompute the score in JavaScript
- [ ] 31.3.3 [T] The same query on the fixture returns the same ids and the same per-term contributions as the engine

## 31.4 Boundary
- [ ] 31.4.1 Document that write mode and a local `persia studio` server are not this version. The v2.0 dashboard remains M25, on `persia-server`

---

# M32 — Ranking rules (target: v1.1)
Deps: M7, M8, M15.

Ordered ranking rules, applied as a lexicographic bucket sort. The default order is words, typo, proximity, attribute, exactness, then sort. BM25F is one rule in that order, not a score the other rules only break ties on. The v1.0 proximity bonus is 7.3.5. The v1.0 typo and exactness signal is 8.2.7. Leaf 32.3.1 is v1.2.

## 32.1 Rules
- [ ] 32.1.1 (v1.1) Default ranking rules, in order: words, typo, proximity, attribute, exactness, sort. Hits are ordered by a lexicographic bucket sort on those rules. BM25F is one rule a collection can place in the list. Proximity uses positions already indexed (5.3.4). [T] Two documents that differ on an earlier rule keep that order when a later rule, including BM25F, would reverse them

## 32.2 Function score
- [ ] 32.2.1 (v1.1) Function score: date decay (gauss, exp, linear), a popularity term, and a combine over numeric fields. The `saturation` function deferred in 27.6.1 (`v / (v + pivot)`) moves here. [T] A fixed `now` makes gauss, exp, and linear decay monotone in distance from the origin, and saturation matches `v / (v + pivot)`

## 32.3 Optional BM25 variants
- [ ] 32.3.1 (v1.2) Optional BM25+ or BM25L per collection, only if the nDCG@10 baseline (7.8.2) and the CI gate (7.8.4) justify it. The default scorer stays BM25F (SPEC §7.2). [T] With the option off, scores match the reference BM25F scorer

## 32.4 Rescore
- [ ] 32.4.1 (v1.1) A rescore top-N hook on the query API: after the first pass, a caller-supplied rescorer may reorder the top N hits. It exists so a later learning-to-rank model (v2.x) can plug in. This leaf does not ship a model. [T] A rescorer that reverses the top N changes only those hits; hits outside N stay in the first-pass order

---

# M33 — Search features (target: v1.1)
Deps: M4, M6, M7, M15.

v1.0 ships per-character CJK (4.1.1.2), keyword facets (7.5.3), and delete-by-id (SPEC §8.1). These leaves are the gaps. Each one is v1.1.

## 33.1 Analysis
- [ ] 33.1.1 (v1.1) CJK segmentation beyond the per-character fallback (4.1.1.2), and pluggable tokenizers a collection can register beside the built-in analyzers (SPEC §6.3). A pluggable tokenizer is code in this repo or registered by the host; it is not another search engine. [T] A CJK string segments the same way on index and on query, and a registered tokenizer's tokens round-trip into postings

## 33.2 Reindex
- [ ] 33.2.1 (v1.1) Reindex a collection and swap an alias so readers move to the new collection in one commit. SPEC §6.2.4 already requires a reindex when analysis changes; it does not name the alias. Any new on-disk record follows the format discipline (SPEC §15). [T] Readers see the old collection until the swap commits, then the new one, and a crash during the swap leaves one of the two, not a mix

## 33.3 Deletes
- [ ] 33.3.1 (v1.1) Delete-by-query: delete the documents a query matches, through the same tombstones and commit as delete-by-id (SPEC §8.1, §8.3). [T] A query that matches two of three documents leaves the third searchable after commit, and a crash before commit leaves all three

## 33.4 Facets
- [ ] 33.4.1 (v1.1) Numeric and range facets, and facet search (narrow the returned facet values), in addition to keyword counts (7.5.3, SPEC §7.5). [T] A range counts only documents inside it, and a facet search hides values that do not match the facet query

## 33.5 Grouping
- [ ] 33.5.1 (v1.1) Distinct / group-by: at most one hit per distinct value of a column, with a documented rule for which document represents the group. [T] Three documents sharing a value return one hit, and the representative is the one the rule names

---

# M34 — Backup, export, and query analytics (target: v1.2)
Deps: M8, M10, M15.

User-facing operations for one database. Tenant backups on object storage stay 22.2 (v2.0). The dashboard's zero-result list stays 25.4. The relaxation histogram stays 8.3.3.

## 34.1 Backup and export
- [ ] 34.1.1 (v1.2) User-facing backup and export of one database from the CLI and the embedded API: a copy a later open can search. It does not upload a tenant file (22.2). [T] An exported copy opens and returns the same hits as the source; a crash during the copy does not truncate the source

## 34.2 Zero-result analytics
- [ ] 34.2.1 (v1.2) Analytics of zero-result queries: record queries that stayed empty, including after relaxation (SPEC §7.6). This is the record; the v2.0 dashboard list remains 25.4, and the metric histogram remains 8.3.3. [T] A query that is still empty after relaxation is recorded. A query that matched is not

